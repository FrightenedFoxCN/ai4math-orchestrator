//! Durable idle-time literature verification queue (migration 10) unifying
//! BP backward-VoI automatic tickets and explicit Human-submitted tickets with
//! closed-loop `replicates` / `contradicts` graph updates.

use super::{Change, NOW, StateStore, event};
use crate::{AutomatableLiteratureEntry, retained::valid_id};
use ai4math_core::{
    BeliefPropagationReport, ConflictEdgeKind, EpistemicDebt, EpistemicTag, ReproducibilityClass,
};
use ai4math_evidence::{SealedReceipt, is_safe_compute_token};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};

const IDLE_ENQUEUED: &str = "IDLE_VERIFICATION_ENQUEUED";
const IDLE_DISPATCHED: &str = "IDLE_VERIFICATION_DISPATCHED";
const IDLE_REPLICATED: &str = "IDLE_VERIFICATION_REPLICATED";
const IDLE_CONTRADICTED: &str = "IDLE_VERIFICATION_CONTRADICTED";
const IDLE_CANCELLED: &str = "IDLE_VERIFICATION_CANCELLED";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdleTicketSource {
    BpVoi,
    Human,
}

impl IdleTicketSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BpVoi => "bp_voi",
            Self::Human => "human",
        }
    }

    fn parse(s: &str) -> Result<Self> {
        match s {
            "bp_voi" => Ok(Self::BpVoi),
            "human" => Ok(Self::Human),
            _ => anyhow::bail!("invalid idle ticket source {s:?}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdleTicketStatus {
    Queued,
    Dispatched,
    Replicated,
    Contradicted,
    Failed,
    Cancelled,
}

impl IdleTicketStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Dispatched => "dispatched",
            Self::Replicated => "replicated",
            Self::Contradicted => "contradicted",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Replicated | Self::Contradicted | Self::Failed | Self::Cancelled
        )
    }

    fn parse(s: &str) -> Result<Self> {
        match s {
            "queued" => Ok(Self::Queued),
            "dispatched" => Ok(Self::Dispatched),
            "replicated" => Ok(Self::Replicated),
            "contradicted" => Ok(Self::Contradicted),
            "failed" => Ok(Self::Failed),
            "cancelled" => Ok(Self::Cancelled),
            _ => anyhow::bail!("invalid idle ticket status {s:?}"),
        }
    }
}

/// Input to enqueue one literature entry for idle-time formalization or replication.
#[derive(Debug)]
pub struct NewIdleVerificationTicket<'a> {
    pub ticket_id: &'a str,
    pub source: IdleTicketSource,
    pub entry_id: &'a str,
    pub epistemic_node_id: Option<&'a str>,
    pub domain_id: &'a str,
    pub reproducibility_class: ReproducibilityClass,
    pub target_protocol_or_engine: &'a str,
    pub priority_score: f64,
    pub human_note: Option<&'a str>,
}

/// Durable readback of one idle verification ticket.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RetainedIdleTicket {
    pub ticket_id: String,
    pub source: IdleTicketSource,
    pub entry_id: String,
    pub epistemic_node_id: Option<String>,
    pub domain_id: String,
    pub reproducibility_class: ReproducibilityClass,
    pub target_protocol_or_engine: String,
    pub priority_score: f64,
    pub human_note: Option<String>,
    pub status: IdleTicketStatus,
    pub compute_job_id: Option<String>,
    pub sealed_receipt_sha256: Option<String>,
    pub wall_seconds_spent: f64,
    pub gpu_seconds_spent: f64,
    pub detail: Option<String>,
    pub created_event_seq: i64,
    pub updated_event_seq: i64,
    pub created_at: String,
    pub updated_at: String,
}

/// Cumulative idle-budget accounting read from `idle_verification_queue`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct IdleUsageTotals {
    pub executed_tickets: u32,
    pub total_wall_seconds: f64,
    pub total_gpu_seconds: f64,
}

fn has_migration_10(db: &Connection) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM state_migrations WHERE version=10)",
        [],
        |r| r.get(0),
    )?)
}

const SELECT_IDLE_TICKET: &str = "SELECT
    ticket_id, source, entry_id, epistemic_node_id, domain_id, reproducibility_class,
    target_protocol_or_engine, priority_score, human_note, status, compute_job_id,
    sealed_receipt_sha256, wall_seconds_spent, gpu_seconds_spent, detail,
    created_event_seq, updated_event_seq, created_at, updated_at
  FROM idle_verification_queue";

fn decode_ticket(r: &rusqlite::Row<'_>) -> Result<RetainedIdleTicket> {
    let source_s: String = r.get(1)?;
    let repro_s: String = r.get(5)?;
    let status_s: String = r.get(9)?;
    Ok(RetainedIdleTicket {
        ticket_id: r.get(0)?,
        source: IdleTicketSource::parse(&source_s)?,
        entry_id: r.get(2)?,
        epistemic_node_id: r.get(3)?,
        domain_id: r.get(4)?,
        reproducibility_class: repro_s
            .parse()
            .map_err(|e| anyhow::anyhow!("stored reproducibility_class invalid: {e}"))?,
        target_protocol_or_engine: r.get(6)?,
        priority_score: r.get(7)?,
        human_note: r.get(8)?,
        status: IdleTicketStatus::parse(&status_s)?,
        compute_job_id: r.get(10)?,
        sealed_receipt_sha256: r.get(11)?,
        wall_seconds_spent: r.get(12)?,
        gpu_seconds_spent: r.get(13)?,
        detail: r.get(14)?,
        created_event_seq: r.get(15)?,
        updated_event_seq: r.get(16)?,
        created_at: r.get(17)?,
        updated_at: r.get(18)?,
    })
}

fn read_ticket(db: &Connection, ticket_id: &str) -> Result<Option<RetainedIdleTicket>> {
    let mut stmt = db.prepare(&format!("{SELECT_IDLE_TICKET} WHERE ticket_id=?1"))?;
    let mut rows = stmt.query([ticket_id])?;
    match rows.next()? {
        Some(r) => Ok(Some(decode_ticket(r)?)),
        None => Ok(None),
    }
}

impl StateStore {
    /// Enqueue a literature entry (from either BP VoI or explicit Human queue)
    /// for idle-time formalization or computational replication.
    pub fn enqueue_idle_verification(
        &mut self,
        change: Change<'_>,
        ticket: &NewIdleVerificationTicket<'_>,
    ) -> Result<RetainedIdleTicket> {
        ensure!(
            is_safe_compute_token(ticket.ticket_id, 128),
            "invalid idle ticket_id"
        );
        ensure!(
            !ticket.entry_id.trim().is_empty() && ticket.entry_id.len() <= 128,
            "invalid entry_id"
        );
        ensure!(
            ticket.reproducibility_class.is_automatable(),
            "only formalizable_in_itp or replicable_by_protocol entries may enter idle verification queue"
        );
        ensure!(
            ticket.priority_score.is_finite() && ticket.priority_score >= 0.0,
            "priority_score must be finite and non-negative"
        );
        ensure!(
            is_safe_compute_token(ticket.target_protocol_or_engine, 128),
            "invalid target_protocol_or_engine"
        );

        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(existing) = read_ticket(&tx, ticket.ticket_id)? {
            ensure!(
                existing.entry_id == ticket.entry_id && existing.domain_id == ticket.domain_id,
                "idle ticket {:?} already exists for a different entry or domain",
                ticket.ticket_id
            );
            if existing.status == IdleTicketStatus::Queued
                && (ticket.priority_score > existing.priority_score
                    || (ticket.source == IdleTicketSource::Human
                        && existing.source == IdleTicketSource::BpVoi))
            {
                let next_source = if ticket.source == IdleTicketSource::Human {
                    IdleTicketSource::Human
                } else {
                    existing.source
                };
                let next_priority = existing.priority_score.max(ticket.priority_score);
                let seq = event(
                    &tx,
                    change,
                    IDLE_ENQUEUED,
                    "idle_verification_ticket",
                    ticket.ticket_id,
                    json!({
                        "entry_id": ticket.entry_id,
                        "source": next_source.as_str(),
                        "priority_score": next_priority,
                        "refreshed": true,
                    }),
                )?;
                tx.execute(
                    &format!(
                        "UPDATE idle_verification_queue
                         SET source=?2, priority_score=?3, human_note=COALESCE(?4, human_note),
                             updated_event_seq=?5, updated_at={NOW}
                         WHERE ticket_id=?1"
                    ),
                    params![
                        ticket.ticket_id,
                        next_source.as_str(),
                        next_priority,
                        ticket.human_note,
                        seq,
                    ],
                )?;
                let refreshed =
                    read_ticket(&tx, ticket.ticket_id)?.context("ticket missing after refresh")?;
                tx.commit()?;
                return Ok(refreshed);
            }
            tx.commit()?;
            return Ok(existing);
        }

        let seq = event(
            &tx,
            change,
            IDLE_ENQUEUED,
            "idle_verification_ticket",
            ticket.ticket_id,
            json!({
                "entry_id": ticket.entry_id,
                "epistemic_node_id": ticket.epistemic_node_id,
                "domain_id": ticket.domain_id,
                "source": ticket.source.as_str(),
                "reproducibility_class": ticket.reproducibility_class.as_str(),
                "target_protocol_or_engine": ticket.target_protocol_or_engine,
                "priority_score": ticket.priority_score,
            }),
        )?;

        tx.execute(
            &format!(
                "INSERT INTO idle_verification_queue(
                    ticket_id, source, entry_id, epistemic_node_id, domain_id,
                    reproducibility_class, target_protocol_or_engine, priority_score,
                    human_note, status, compute_job_id, sealed_receipt_sha256,
                    wall_seconds_spent, gpu_seconds_spent, detail,
                    created_event_seq, updated_event_seq, created_at, updated_at
                 ) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,'queued',NULL,NULL,0.0,0.0,NULL,?10,?10,{NOW},{NOW})"
            ),
            params![
                ticket.ticket_id,
                ticket.source.as_str(),
                ticket.entry_id,
                ticket.epistemic_node_id,
                ticket.domain_id,
                ticket.reproducibility_class.as_str(),
                ticket.target_protocol_or_engine,
                ticket.priority_score,
                ticket.human_note,
                seq,
            ],
        )?;
        let saved = read_ticket(&tx, ticket.ticket_id)?.context("ticket missing after insert")?;
        tx.commit()?;
        Ok(saved)
    }

    /// Synchronize BP backward-VoI rankings with non-retracted automatable
    /// `LITERATURE` vault entries, automatically enqueueing high-VoI items.
    pub fn sync_voi_idle_queue(
        &mut self,
        change: Change<'_>,
        report: &BeliefPropagationReport,
        automatable_entries: &[AutomatableLiteratureEntry],
        default_target_by_domain: &BTreeMap<String, String>,
        min_voi_threshold: f64,
    ) -> Result<Vec<RetainedIdleTicket>> {
        // Map source_entry_id -> epistemic_node_id from epistemic_nodes (migration 9).
        let mut entry_to_node: BTreeMap<String, String> = BTreeMap::new();
        {
            let mut stmt = self.db.prepare(
                "SELECT source_entry_id, node_id FROM epistemic_nodes
                 WHERE source_entry_id IS NOT NULL ORDER BY node_id",
            )?;
            let mut rows = stmt.query([])?;
            while let Some(r) = rows.next()? {
                entry_to_node.insert(r.get(0)?, r.get(1)?);
            }
        }
        let voi_by_node: BTreeMap<&str, f64> = report
            .voi_rankings
            .iter()
            .map(|v| (v.node_id.as_str(), v.belief_gain_on_critical_paths))
            .collect();

        let mut enqueued = Vec::new();
        for lit in automatable_entries {
            if !lit.reproducibility_class.is_automatable() {
                continue;
            }
            let linked_node = entry_to_node
                .get(&lit.entry_id)
                .map(String::as_str)
                .or(lit.origin_claim.as_deref());
            let Some(node_id) = linked_node else {
                continue;
            };
            let Some(&voi) = voi_by_node.get(node_id) else {
                continue;
            };
            if voi < min_voi_threshold {
                continue;
            }
            let target = default_target_by_domain
                .get(&lit.domain_id)
                .map(String::as_str)
                .unwrap_or(match lit.reproducibility_class {
                    ReproducibilityClass::FormalizableInItp => "lean4",
                    _ => "protocol_default",
                });
            let ticket_id = format!("idle-voi-{}", lit.entry_id);
            let saved = self.enqueue_idle_verification(
                change,
                &NewIdleVerificationTicket {
                    ticket_id: &ticket_id,
                    source: IdleTicketSource::BpVoi,
                    entry_id: &lit.entry_id,
                    epistemic_node_id: Some(node_id),
                    domain_id: &lit.domain_id,
                    reproducibility_class: lit.reproducibility_class,
                    target_protocol_or_engine: target,
                    priority_score: voi,
                    human_note: None,
                },
            )?;
            enqueued.push(saved);
        }
        Ok(enqueued)
    }

    pub fn mark_idle_ticket_dispatched(
        &mut self,
        change: Change<'_>,
        ticket_id: &str,
        compute_job_id: Option<&str>,
    ) -> Result<RetainedIdleTicket> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing = read_ticket(&tx, ticket_id)?
            .with_context(|| format!("unknown idle ticket {ticket_id:?}"))?;
        ensure!(
            existing.status == IdleTicketStatus::Queued,
            "cannot dispatch idle ticket {:?} in status {}",
            ticket_id,
            existing.status.as_str()
        );
        let seq = event(
            &tx,
            change,
            IDLE_DISPATCHED,
            "idle_verification_ticket",
            ticket_id,
            json!({
                "entry_id": existing.entry_id,
                "compute_job_id": compute_job_id,
            }),
        )?;
        tx.execute(
            &format!(
                "UPDATE idle_verification_queue
                 SET status='dispatched', compute_job_id=?2, updated_event_seq=?3, updated_at={NOW}
                 WHERE ticket_id=?1"
            ),
            params![ticket_id, compute_job_id, seq],
        )?;
        let saved = read_ticket(&tx, ticket_id)?.context("ticket missing after dispatch")?;
        tx.commit()?;
        Ok(saved)
    }

    /// Close an idle verification ticket as `replicated` using a broker-sealed
    /// `&dyn SealedReceipt`, atomically upgrading the linked epistemic node
    /// (clearing literature/condition tags and attaching the sealed receipt).
    pub fn complete_idle_ticket_replicated(
        &mut self,
        change: Change<'_>,
        ticket_id: &str,
        receipt: &dyn SealedReceipt,
        wall_seconds_spent: f64,
        gpu_seconds_spent: f64,
    ) -> Result<RetainedIdleTicket> {
        ensure!(
            valid_id(receipt.receipt_sha256()),
            "sealed receipt has malformed receipt_sha256"
        );
        ensure!(
            wall_seconds_spent.is_finite()
                && wall_seconds_spent >= 0.0
                && gpu_seconds_spent.is_finite()
                && gpu_seconds_spent >= 0.0,
            "idle usage seconds must be finite and non-negative"
        );
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing = read_ticket(&tx, ticket_id)?
            .with_context(|| format!("unknown idle ticket {ticket_id:?}"))?;
        ensure!(
            !existing.status.is_terminal(),
            "idle ticket {:?} is already terminal ({})",
            ticket_id,
            existing.status.as_str()
        );
        ensure!(
            receipt.domain_id() == existing.domain_id,
            "sealed receipt domain {:?} does not match idle ticket domain {:?}",
            receipt.domain_id(),
            existing.domain_id
        );
        let new_tag = EpistemicTag::parse(receipt.domain_id(), receipt.epistemic_level())
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        ensure!(
            new_tag.positive_rank().is_some_and(|r| r >= 3),
            "replicated idle ticket requires verified positive receipt (>= L4a / E4a)"
        );

        let seq = event(
            &tx,
            change,
            IDLE_REPLICATED,
            "idle_verification_ticket",
            ticket_id,
            json!({
                "entry_id": existing.entry_id,
                "epistemic_node_id": existing.epistemic_node_id,
                "receipt_sha256": receipt.receipt_sha256(),
                "epistemic_level": receipt.epistemic_level(),
                "wall_seconds_spent": wall_seconds_spent,
                "gpu_seconds_spent": gpu_seconds_spent,
            }),
        )?;

        tx.execute(
            &format!(
                "UPDATE idle_verification_queue
                 SET status='replicated', sealed_receipt_sha256=?2,
                     wall_seconds_spent=?3, gpu_seconds_spent=?4,
                     detail='replicated and eligible for CORE promotion',
                     updated_event_seq=?5, updated_at={NOW}
                 WHERE ticket_id=?1"
            ),
            params![
                ticket_id,
                receipt.receipt_sha256(),
                wall_seconds_spent,
                gpu_seconds_spent,
                seq,
            ],
        )?;

        // Automatically upgrade the linked epistemic node inside the same atomic transaction!
        if let Some(ref node_id) = existing.epistemic_node_id {
            let old_tags_json: Option<String> = tx
                .query_row(
                    "SELECT condition_tags_json FROM epistemic_nodes WHERE node_id=?1",
                    [node_id],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(tags_json) = old_tags_json {
                let old_tags: Vec<String> = serde_json::from_str(&tags_json)?;
                let cleared = [
                    "unreproduced_literature",
                    "incomplete_conditions",
                    "missing_standing_assumptions",
                ];
                let next_tags: Vec<String> = old_tags
                    .into_iter()
                    .filter(|t| !cleared.contains(&t.as_str()))
                    .collect();
                let mut next_debts = BTreeSet::new();
                for code in receipt.epistemic_debts() {
                    next_debts.insert(EpistemicDebt::new(
                        code.clone(),
                        node_id,
                        "idle_replication_receipt",
                    ));
                }
                tx.execute(
                    &format!(
                        "UPDATE epistemic_nodes
                         SET backend_id=?2, local_level=?3, condition_tags_json=?4,
                             local_debts_json=?5, sealed_receipt_sha256=?6, updated_at={NOW}
                         WHERE node_id=?1"
                    ),
                    params![
                        node_id,
                        receipt.backend_id(),
                        new_tag.code,
                        serde_json::to_string(&next_tags)?,
                        serde_json::to_string(&next_debts)?,
                        receipt.receipt_sha256(),
                    ],
                )?;
            }
        }

        let saved = read_ticket(&tx, ticket_id)?.context("ticket missing after replication")?;
        tx.commit()?;
        Ok(saved)
    }

    /// Close an idle verification ticket as `contradicted` using a broker-sealed
    /// receipt, atomically attaching a `Contradicts` factor edge to the linked
    /// epistemic node so BP immediately raises `conflict_mass` and `disbelief`.
    #[allow(clippy::too_many_arguments)]
    pub fn complete_idle_ticket_contradicted(
        &mut self,
        change: Change<'_>,
        ticket_id: &str,
        counter_node_id: &str,
        receipt: &dyn SealedReceipt,
        rationale: &str,
        wall_seconds_spent: f64,
        gpu_seconds_spent: f64,
    ) -> Result<RetainedIdleTicket> {
        ensure!(
            valid_id(receipt.receipt_sha256()),
            "sealed receipt has malformed receipt_sha256"
        );
        ensure!(
            is_safe_compute_token(counter_node_id, 128),
            "invalid counter_node_id"
        );
        ensure!(
            wall_seconds_spent.is_finite()
                && wall_seconds_spent >= 0.0
                && gpu_seconds_spent.is_finite()
                && gpu_seconds_spent >= 0.0,
            "idle usage seconds must be finite and non-negative"
        );
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing = read_ticket(&tx, ticket_id)?
            .with_context(|| format!("unknown idle ticket {ticket_id:?}"))?;
        ensure!(
            !existing.status.is_terminal(),
            "idle ticket {:?} is already terminal",
            ticket_id
        );

        let seq = event(
            &tx,
            change,
            IDLE_CONTRADICTED,
            "idle_verification_ticket",
            ticket_id,
            json!({
                "entry_id": existing.entry_id,
                "epistemic_node_id": existing.epistemic_node_id,
                "counter_node_id": counter_node_id,
                "receipt_sha256": receipt.receipt_sha256(),
                "rationale": rationale,
            }),
        )?;

        tx.execute(
            &format!(
                "UPDATE idle_verification_queue
                 SET status='contradicted', sealed_receipt_sha256=?2,
                     wall_seconds_spent=?3, gpu_seconds_spent=?4,
                     detail=?5, updated_event_seq=?6, updated_at={NOW}
                 WHERE ticket_id=?1"
            ),
            params![
                ticket_id,
                receipt.receipt_sha256(),
                wall_seconds_spent,
                gpu_seconds_spent,
                rationale,
                seq,
            ],
        )?;

        if let Some(ref target_node_id) = existing.epistemic_node_id {
            let edge_id = format!("idle-contra-{ticket_id}");
            tx.execute(
                &format!(
                    "INSERT INTO epistemic_conflict_edges(
                        edge_id, source_node_id, target_node_id, edge_kind, weight,
                        evidence_receipt_sha256, rationale, event_seq, created_at
                     ) VALUES(?1,?2,?3,?4,0.85,?5,?6,?7,{NOW})
                     ON CONFLICT(edge_id) DO NOTHING"
                ),
                params![
                    edge_id,
                    counter_node_id,
                    target_node_id,
                    ConflictEdgeKind::Contradicts.as_str(),
                    receipt.receipt_sha256(),
                    rationale,
                    seq,
                ],
            )?;
        }

        let saved = read_ticket(&tx, ticket_id)?.context("ticket missing after contradiction")?;
        tx.commit()?;
        Ok(saved)
    }

    pub fn cancel_idle_ticket(
        &mut self,
        change: Change<'_>,
        ticket_id: &str,
        reason: &str,
    ) -> Result<RetainedIdleTicket> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing = read_ticket(&tx, ticket_id)?
            .with_context(|| format!("unknown idle ticket {ticket_id:?}"))?;
        ensure!(
            !existing.status.is_terminal(),
            "idle ticket {:?} is already terminal",
            ticket_id
        );
        let seq = event(
            &tx,
            change,
            IDLE_CANCELLED,
            "idle_verification_ticket",
            ticket_id,
            json!({ "reason": reason }),
        )?;
        tx.execute(
            &format!(
                "UPDATE idle_verification_queue
                 SET status='cancelled', detail=?2, updated_event_seq=?3, updated_at={NOW}
                 WHERE ticket_id=?1"
            ),
            params![ticket_id, reason, seq],
        )?;
        let saved = read_ticket(&tx, ticket_id)?.context("ticket missing after cancel")?;
        tx.commit()?;
        Ok(saved)
    }

    pub fn list_queued_idle_tickets(&self) -> Result<Vec<RetainedIdleTicket>> {
        if !has_migration_10(&self.db)? {
            return Ok(Vec::new());
        }
        let mut stmt = self.db.prepare(&format!(
            "{SELECT_IDLE_TICKET}
             WHERE status='queued'
             ORDER BY CASE WHEN source='human' THEN 0 ELSE 1 END,
                      priority_score DESC, created_event_seq ASC"
        ))?;
        let mut rows = stmt.query([])?;
        let mut out = Vec::new();
        while let Some(r) = rows.next()? {
            out.push(decode_ticket(r)?);
        }
        Ok(out)
    }

    pub fn read_idle_ticket(&self, ticket_id: &str) -> Result<Option<RetainedIdleTicket>> {
        if !has_migration_10(&self.db)? {
            return Ok(None);
        }
        read_ticket(&self.db, ticket_id)
    }

    pub fn idle_usage_totals(&self) -> Result<IdleUsageTotals> {
        if !has_migration_10(&self.db)? {
            return Ok(IdleUsageTotals::default());
        }
        let (executed, wall, gpu): (i64, f64, f64) = self
            .db
            .query_row(
                "SELECT COUNT(*), COALESCE(SUM(wall_seconds_spent), 0.0), COALESCE(SUM(gpu_seconds_spent), 0.0)
                 FROM idle_verification_queue
                 WHERE status IN ('dispatched','replicated','contradicted','failed')",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?
            .unwrap_or((0, 0.0, 0.0));
        Ok(IdleUsageTotals {
            executed_tickets: u32::try_from(executed)?,
            total_wall_seconds: wall,
            total_gpu_seconds: gpu,
        })
    }
}
