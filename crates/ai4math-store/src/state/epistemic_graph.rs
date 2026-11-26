//! Generalized multi-domain AND-OR-Conflict epistemic graph and dual-channel
//! Subjective Logic Belief Propagation persistence (migration 9).
//!
//! Bridges legacy ITP `claims` / `proof_plans` seamlessly into one unified
//! `BeliefGraph` alongside multi-domain `epistemic_nodes`, `epistemic_plans`,
//! and `epistemic_conflict_edges`, enforcing strict domain <-> level separation
//! and sealed-receipt upgrades via `&dyn SealedReceipt`.

use super::{Change, NOW, StateStore, event};
use crate::retained::valid_id;
use ai4math_core::{
    BeliefConflictEdge, BeliefGraph, BeliefNodeInput, BeliefPlanInput, BeliefPropagationConfig,
    BeliefPropagationReport, BeliefPropagator, ConflictEdgeKind, EpistemicDebt, EpistemicTag,
    ReproducibilityClass, SubjectiveLogicPropagator, TagAttenuationPolicy, Trust,
};
use ai4math_evidence::{SealedReceipt, is_safe_compute_token};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::Serialize;
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};

const NODE_UPSERTED: &str = "EPISTEMIC_NODE_UPSERTED";
const NODE_UPGRADED: &str = "EPISTEMIC_NODE_SEALED_UPGRADE";
const PLAN_UPSERTED: &str = "EPISTEMIC_PLAN_UPSERTED";
const CONFLICT_RECORDED: &str = "EPISTEMIC_CONFLICT_EDGE_RECORDED";
const BELIEF_SNAPSHOT: &str = "EPISTEMIC_BELIEF_SNAPSHOT_RECORDED";

/// Input for registering a domain-qualified epistemic node in the generalized graph.
#[derive(Debug)]
pub struct NewEpistemicNode<'a> {
    pub node_id: &'a str,
    pub domain_id: &'a str,
    pub backend_id: &'a str,
    pub title: &'a str,
    pub statement: &'a str,
    pub is_critical_path: bool,
    pub local_level: &'a str,
    pub condition_tags: &'a [&'a str],
    pub local_debts: &'a [EpistemicDebt],
    pub reproducibility_class: Option<ReproducibilityClass>,
    pub source_entry_id: Option<&'a str>,
}

/// Durable readback of a generalized epistemic node.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RetainedEpistemicNode {
    pub node_id: String,
    pub domain_id: String,
    pub backend_id: String,
    pub title: String,
    pub statement: String,
    pub is_critical_path: bool,
    pub local_level: String,
    pub condition_tags: Vec<String>,
    pub local_debts: BTreeSet<EpistemicDebt>,
    pub reproducibility_class: Option<ReproducibilityClass>,
    pub source_entry_id: Option<String>,
    pub sealed_receipt_sha256: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// Input for registering an AND factor plan connecting a target node to dependencies.
#[derive(Debug)]
pub struct NewEpistemicPlan<'a> {
    pub plan_id: &'a str,
    pub target_node_id: &'a str,
    pub name: &'a str,
    pub dependencies: &'a [&'a str],
    pub active: bool,
}

/// Input for recording a `Contradicts` or `Replicates` edge between two graph nodes.
#[derive(Debug)]
pub struct NewConflictEdge<'a> {
    pub edge_id: &'a str,
    pub source_node_id: &'a str,
    pub target_node_id: &'a str,
    pub kind: ConflictEdgeKind,
    pub weight: f64,
    pub rationale: &'a str,
}

/// Durable readback of a persisted BP snapshot.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RetainedBeliefSnapshot {
    pub snapshot_seq: i64,
    pub event_seq: i64,
    pub report: BeliefPropagationReport,
    pub computed_at: String,
}

fn has_migration_9(db: &Connection) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM state_migrations WHERE version=9)",
        [],
        |r| r.get(0),
    )?)
}

struct RawEpistemicNodeRow {
    node_id: String,
    domain_id: String,
    backend_id: String,
    title: String,
    statement: String,
    is_critical_path: bool,
    local_level: String,
    tags_json: String,
    debts_json: String,
    repro_str: Option<String>,
    source_entry_id: Option<String>,
    sealed_receipt_sha256: Option<String>,
    created_at: String,
    updated_at: String,
}

fn read_node_row(db: &Connection, node_id: &str) -> Result<Option<RetainedEpistemicNode>> {
    let row = db
        .query_row(
            "SELECT node_id, domain_id, backend_id, title, statement, is_critical_path,
                    local_level, condition_tags_json, local_debts_json,
                    reproducibility_class, source_entry_id, sealed_receipt_sha256,
                    created_at, updated_at
             FROM epistemic_nodes WHERE node_id=?1",
            [node_id],
            |r| {
                Ok(RawEpistemicNodeRow {
                    node_id: r.get(0)?,
                    domain_id: r.get(1)?,
                    backend_id: r.get(2)?,
                    title: r.get(3)?,
                    statement: r.get(4)?,
                    is_critical_path: r.get(5)?,
                    local_level: r.get(6)?,
                    tags_json: r.get(7)?,
                    debts_json: r.get(8)?,
                    repro_str: r.get(9)?,
                    source_entry_id: r.get(10)?,
                    sealed_receipt_sha256: r.get(11)?,
                    created_at: r.get(12)?,
                    updated_at: r.get(13)?,
                })
            },
        )
        .optional()?;
    let Some(RawEpistemicNodeRow {
        node_id,
        domain_id,
        backend_id,
        title,
        statement,
        is_critical_path,
        local_level,
        tags_json,
        debts_json,
        repro_str,
        source_entry_id,
        sealed_receipt_sha256,
        created_at,
        updated_at,
    }) = row
    else {
        return Ok(None);
    };
    let condition_tags: Vec<String> = serde_json::from_str(&tags_json)?;
    let local_debts: BTreeSet<EpistemicDebt> = serde_json::from_str(&debts_json)?;
    let reproducibility_class = repro_str
        .map(|s| {
            s.parse::<ReproducibilityClass>()
                .map_err(|e| anyhow::anyhow!("stored reproducibility_class invalid: {e}"))
        })
        .transpose()?;
    Ok(Some(RetainedEpistemicNode {
        node_id,
        domain_id,
        backend_id,
        title,
        statement,
        is_critical_path,
        local_level,
        condition_tags,
        local_debts,
        reproducibility_class,
        source_entry_id,
        sealed_receipt_sha256,
        created_at,
        updated_at,
    }))
}

impl StateStore {
    /// Register or update a multi-domain epistemic node (validating domain <-> level
    /// consistency). Positive verification upgrades above preliminary states must
    /// go through `upgrade_epistemic_node_with_receipt` when sealed evidence exists.
    pub fn upsert_epistemic_node(
        &mut self,
        change: Change<'_>,
        node: &NewEpistemicNode<'_>,
    ) -> Result<RetainedEpistemicNode> {
        ensure!(is_safe_compute_token(node.node_id, 128), "invalid node_id");
        ensure!(
            is_safe_compute_token(node.backend_id, 64),
            "invalid backend_id"
        );
        ensure!(
            !node.title.trim().is_empty() && node.title.len() <= 512,
            "invalid node title"
        );
        ensure!(
            !node.statement.trim().is_empty() && node.statement.len() <= 16_384,
            "invalid node statement"
        );
        let tag = EpistemicTag::parse(node.domain_id, node.local_level)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let tags_vec: Vec<String> = node
            .condition_tags
            .iter()
            .map(|t| {
                ensure!(is_safe_compute_token(t, 64), "invalid condition tag {t:?}");
                Ok((*t).to_owned())
            })
            .collect::<Result<_>>()?;
        let debts_set: BTreeSet<EpistemicDebt> = node.local_debts.iter().cloned().collect();
        let tags_json = serde_json::to_string(&tags_vec)?;
        let debts_json = serde_json::to_string(&debts_set)?;
        let repro = node.reproducibility_class.map(ReproducibilityClass::as_str);

        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            &format!(
                "INSERT INTO epistemic_nodes(
                    node_id, domain_id, backend_id, title, statement, is_critical_path,
                    local_level, condition_tags_json, local_debts_json,
                    reproducibility_class, source_entry_id, sealed_receipt_sha256,
                    created_at, updated_at
                 ) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,NULL,{NOW},{NOW})
                 ON CONFLICT(node_id) DO UPDATE SET
                    backend_id=excluded.backend_id,
                    title=excluded.title,
                    statement=excluded.statement,
                    is_critical_path=excluded.is_critical_path,
                    local_level=excluded.local_level,
                    condition_tags_json=excluded.condition_tags_json,
                    local_debts_json=excluded.local_debts_json,
                    reproducibility_class=excluded.reproducibility_class,
                    source_entry_id=excluded.source_entry_id,
                    updated_at={NOW}"
            ),
            params![
                node.node_id,
                tag.domain_id,
                node.backend_id,
                node.title,
                node.statement,
                node.is_critical_path,
                tag.code,
                tags_json,
                debts_json,
                repro,
                node.source_entry_id,
            ],
        )?;
        event(
            &tx,
            change,
            NODE_UPSERTED,
            "epistemic_node",
            node.node_id,
            json!({
                "domain_id": node.domain_id,
                "backend_id": node.backend_id,
                "local_level": node.local_level,
                "condition_tags": tags_vec,
                "source_entry_id": node.source_entry_id,
            }),
        )?;
        let saved = read_node_row(&tx, node.node_id)?.context("epistemic node missing")?;
        tx.commit()?;
        Ok(saved)
    }

    /// Upgrade an epistemic node using a broker-sealed `&dyn SealedReceipt`.
    /// Replaces `unreproduced_literature` / `incomplete_conditions` tags only when
    /// the broker receipt attests a positive level in the node's exact domain.
    pub fn upgrade_epistemic_node_with_receipt(
        &mut self,
        change: Change<'_>,
        node_id: &str,
        receipt: &dyn SealedReceipt,
        clear_condition_tags: &[&str],
    ) -> Result<RetainedEpistemicNode> {
        ensure!(
            valid_id(receipt.receipt_sha256()),
            "sealed receipt has malformed receipt_sha256"
        );
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing = read_node_row(&tx, node_id)?
            .with_context(|| format!("unknown epistemic node {node_id:?}"))?;
        ensure!(
            receipt.domain_id() == existing.domain_id,
            "sealed receipt domain {:?} does not match node domain {:?}",
            receipt.domain_id(),
            existing.domain_id
        );
        let new_tag = EpistemicTag::parse(receipt.domain_id(), receipt.epistemic_level())
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        let clear_set: BTreeSet<&str> = clear_condition_tags.iter().copied().collect();
        let next_tags: Vec<String> = existing
            .condition_tags
            .into_iter()
            .filter(|t| !clear_set.contains(t.as_str()))
            .collect();
        let mut next_debts = BTreeSet::new();
        for code in receipt.epistemic_debts() {
            next_debts.insert(EpistemicDebt::new(
                code.clone(),
                node_id,
                "broker_sealed_receipt",
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
        event(
            &tx,
            change,
            NODE_UPGRADED,
            "epistemic_node",
            node_id,
            json!({
                "domain_id": receipt.domain_id(),
                "backend_id": receipt.backend_id(),
                "epistemic_level": receipt.epistemic_level(),
                "receipt_sha256": receipt.receipt_sha256(),
                "cleared_tags": clear_condition_tags,
            }),
        )?;
        let updated =
            read_node_row(&tx, node_id)?.context("epistemic node missing after upgrade")?;
        tx.commit()?;
        Ok(updated)
    }

    /// Register an AND factor plan linking `target_node_id` to its premises.
    pub fn upsert_epistemic_plan(
        &mut self,
        change: Change<'_>,
        plan: &NewEpistemicPlan<'_>,
    ) -> Result<()> {
        ensure!(is_safe_compute_token(plan.plan_id, 128), "invalid plan_id");
        ensure!(
            !plan.name.trim().is_empty() && plan.name.len() <= 256,
            "invalid plan name"
        );
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            &format!(
                "INSERT INTO epistemic_plans(plan_id, target_node_id, name, active, created_at, updated_at)
                 VALUES(?1,?2,?3,?4,{NOW},{NOW})
                 ON CONFLICT(plan_id) DO UPDATE SET
                   target_node_id=excluded.target_node_id,
                   name=excluded.name,
                   active=excluded.active,
                   updated_at={NOW}"
            ),
            params![plan.plan_id, plan.target_node_id, plan.name, plan.active],
        )?;
        tx.execute(
            "DELETE FROM epistemic_plan_dependencies WHERE plan_id=?1",
            [plan.plan_id],
        )?;
        for dep in plan.dependencies {
            tx.execute(
                "INSERT INTO epistemic_plan_dependencies(plan_id, dep_node_id) VALUES(?1,?2)",
                params![plan.plan_id, dep],
            )?;
        }
        event(
            &tx,
            change,
            PLAN_UPSERTED,
            "epistemic_plan",
            plan.plan_id,
            json!({
                "target_node_id": plan.target_node_id,
                "dependencies": plan.dependencies,
                "active": plan.active,
            }),
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Record an explicit `Contradicts` or `Replicates` factor edge between two nodes
    /// (optionally backed by a broker-sealed `&dyn SealedReceipt`).
    pub fn record_epistemic_conflict_edge(
        &mut self,
        change: Change<'_>,
        edge: &NewConflictEdge<'_>,
        receipt: Option<&dyn SealedReceipt>,
    ) -> Result<()> {
        ensure!(is_safe_compute_token(edge.edge_id, 128), "invalid edge_id");
        ensure!(
            is_safe_compute_token(edge.source_node_id, 128)
                && is_safe_compute_token(edge.target_node_id, 128),
            "invalid conflict edge endpoint"
        );
        ensure!(
            edge.weight.is_finite() && edge.weight > 0.0 && edge.weight <= 1.0,
            "conflict edge weight must be finite in (0.0, 1.0]"
        );
        let receipt_sha = match receipt {
            Some(r) => {
                ensure!(valid_id(r.receipt_sha256()), "malformed receipt_sha256");
                Some(r.receipt_sha256().to_owned())
            }
            None => None,
        };
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let seq = event(
            &tx,
            change,
            CONFLICT_RECORDED,
            "epistemic_conflict_edge",
            edge.edge_id,
            json!({
                "source_node_id": edge.source_node_id,
                "target_node_id": edge.target_node_id,
                "kind": edge.kind.as_str(),
                "weight": edge.weight,
                "receipt_sha256": receipt_sha,
            }),
        )?;
        tx.execute(
            &format!(
                "INSERT INTO epistemic_conflict_edges(
                    edge_id, source_node_id, target_node_id, edge_kind, weight,
                    evidence_receipt_sha256, rationale, event_seq, created_at
                 ) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,{NOW})"
            ),
            params![
                edge.edge_id,
                edge.source_node_id,
                edge.target_node_id,
                edge.kind.as_str(),
                edge.weight,
                receipt_sha,
                edge.rationale,
                seq,
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn read_epistemic_node(&self, node_id: &str) -> Result<Option<RetainedEpistemicNode>> {
        if !has_migration_9(&self.db)? {
            return Ok(None);
        }
        read_node_row(&self.db, node_id)
    }

    /// Assemble one unified `BeliefGraph` merging legacy ITP `claims`/`proof_plans`
    /// with multi-domain `epistemic_nodes`/`epistemic_plans`/`epistemic_conflict_edges`.
    pub fn build_unified_belief_graph(&self) -> Result<BeliefGraph> {
        let mut nodes_by_id: BTreeMap<String, BeliefNodeInput> = BTreeMap::new();
        let mut plans = Vec::new();
        let mut conflict_edges = Vec::new();

        // 1. Lift legacy ITP claims & active proof_plans seamlessly into the unified graph.
        {
            let mut stmt = self.db.prepare(
                "SELECT claim_id, local_trust_level, is_critical_path, oracle_debt FROM claims ORDER BY claim_id",
            )?;
            let mut rows = stmt.query([])?;
            while let Some(r) = rows.next()? {
                let claim_id: String = r.get(0)?;
                let level_str: String = r.get(1)?;
                let critical: bool = r.get(2)?;
                let oracle_debt: bool = r.get(3)?;
                let trust: Trust = level_str
                    .parse()
                    .map_err(|e| anyhow::anyhow!("invalid legacy claim trust: {e}"))?;
                let mut debts = BTreeSet::new();
                if oracle_debt {
                    debts.insert(EpistemicDebt::new(
                        "oracle_unreconstructed",
                        &claim_id,
                        "legacy_claim_oracle_debt",
                    ));
                }
                nodes_by_id.insert(
                    claim_id.clone(),
                    BeliefNodeInput {
                        node_id: claim_id,
                        epistemic: EpistemicTag::from_itp(trust),
                        is_critical_path: critical,
                        base_opinion: None,
                        condition_tags: Vec::new(),
                        local_debts: debts,
                        reproducibility: Some(ReproducibilityClass::FormalizableInItp),
                    },
                );
            }

            let mut plan_stmt = self.db.prepare(
                "SELECT plan_id, target_claim_id, status FROM proof_plans ORDER BY plan_id",
            )?;
            let mut dep_stmt = self.db.prepare(
                "SELECT dep_claim_id FROM plan_dependencies WHERE plan_id=?1 ORDER BY dep_claim_id",
            )?;
            let mut p_rows = plan_stmt.query([])?;
            while let Some(r) = p_rows.next()? {
                let plan_id: String = r.get(0)?;
                let target_claim_id: String = r.get(1)?;
                let status: String = r.get(2)?;
                let deps: Vec<String> = dep_stmt
                    .query_map([&plan_id], |dr| dr.get(0))?
                    .collect::<std::result::Result<_, _>>()?;
                plans.push(BeliefPlanInput {
                    plan_id,
                    target_node_id: target_claim_id,
                    dependencies: deps,
                    active: matches!(status.as_str(), "ACTIVE" | "PROVED"),
                });
            }
        }

        // 2. Overlay / append multi-domain epistemic_nodes, epistemic_plans, and conflict edges.
        if has_migration_9(&self.db)? {
            let mut n_stmt = self
                .db
                .prepare("SELECT node_id FROM epistemic_nodes ORDER BY node_id")?;
            let node_ids: Vec<String> = n_stmt
                .query_map([], |r| r.get(0))?
                .collect::<std::result::Result<_, _>>()?;
            for nid in node_ids {
                if let Some(n) = read_node_row(&self.db, &nid)? {
                    let epistemic = EpistemicTag::parse(&n.domain_id, &n.local_level)
                        .map_err(|e| anyhow::anyhow!("{e}"))?;
                    nodes_by_id.insert(
                        n.node_id.clone(),
                        BeliefNodeInput {
                            node_id: n.node_id,
                            epistemic,
                            is_critical_path: n.is_critical_path,
                            base_opinion: None,
                            condition_tags: n.condition_tags,
                            local_debts: n.local_debts,
                            reproducibility: n.reproducibility_class,
                        },
                    );
                }
            }

            let mut ep_stmt = self.db.prepare(
                "SELECT plan_id, target_node_id, active FROM epistemic_plans ORDER BY plan_id",
            )?;
            let mut ed_stmt = self.db.prepare(
                "SELECT dep_node_id FROM epistemic_plan_dependencies WHERE plan_id=?1 ORDER BY dep_node_id",
            )?;
            let mut ep_rows = ep_stmt.query([])?;
            while let Some(r) = ep_rows.next()? {
                let plan_id: String = r.get(0)?;
                let target_node_id: String = r.get(1)?;
                let active: bool = r.get(2)?;
                let deps: Vec<String> = ed_stmt
                    .query_map([&plan_id], |dr| dr.get(0))?
                    .collect::<std::result::Result<_, _>>()?;
                plans.push(BeliefPlanInput {
                    plan_id,
                    target_node_id,
                    dependencies: deps,
                    active,
                });
            }

            let mut c_stmt = self.db.prepare(
                "SELECT edge_id, source_node_id, target_node_id, edge_kind, weight
                 FROM epistemic_conflict_edges ORDER BY edge_id",
            )?;
            let mut c_rows = c_stmt.query([])?;
            while let Some(r) = c_rows.next()? {
                let edge_id: String = r.get(0)?;
                let source_node_id: String = r.get(1)?;
                let target_node_id: String = r.get(2)?;
                let kind_str: String = r.get(3)?;
                let weight: f64 = r.get(4)?;
                let kind: ConflictEdgeKind = kind_str
                    .parse()
                    .map_err(|e| anyhow::anyhow!("stored conflict edge kind invalid: {e}"))?;
                conflict_edges.push(BeliefConflictEdge {
                    edge_id,
                    source_node_id,
                    target_node_id,
                    kind,
                    weight,
                });
            }
        }

        Ok(BeliefGraph {
            nodes: nodes_by_id.into_values().collect(),
            plans,
            conflict_edges,
        })
    }

    /// Run dual-channel Subjective Logic BP over the current unified graph without writing.
    pub fn evaluate_belief_graph(
        &self,
        attenuation: &TagAttenuationPolicy,
        config: &BeliefPropagationConfig,
    ) -> Result<BeliefPropagationReport> {
        let graph = self.build_unified_belief_graph()?;
        SubjectiveLogicPropagator
            .propagate(&graph, attenuation, config)
            .map_err(|e| anyhow::anyhow!("belief propagation failed: {e}"))
    }

    /// Evaluate and persist an append-only BP + VoI snapshot to `epistemic_belief_snapshots`.
    pub fn record_belief_snapshot(
        &mut self,
        change: Change<'_>,
        attenuation: &TagAttenuationPolicy,
        config: &BeliefPropagationConfig,
    ) -> Result<RetainedBeliefSnapshot> {
        let report = self.evaluate_belief_graph(attenuation, config)?;
        let report_json = serde_json::to_string(&report)?;
        let top_voi = report.voi_rankings.first().map(|v| v.node_id.clone());

        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let seq = event(
            &tx,
            change,
            BELIEF_SNAPSHOT,
            "epistemic_graph",
            "unified",
            json!({
                "converged": report.converged,
                "iterations": report.iterations,
                "node_count": report.nodes.len(),
                "top_voi_node": top_voi,
            }),
        )?;
        tx.execute(
            &format!(
                "INSERT INTO epistemic_belief_snapshots(event_seq, converged, iterations, report_json, computed_at)
                 VALUES(?1,?2,?3,?4,{NOW})"
            ),
            params![seq, report.converged, i64::from(report.iterations), report_json],
        )?;
        let snapshot_seq = tx.last_insert_rowid();
        let computed_at: String = tx.query_row(
            "SELECT computed_at FROM epistemic_belief_snapshots WHERE snapshot_seq=?1",
            [snapshot_seq],
            |r| r.get(0),
        )?;
        tx.commit()?;
        Ok(RetainedBeliefSnapshot {
            snapshot_seq,
            event_seq: seq,
            report,
            computed_at,
        })
    }

    pub fn latest_belief_snapshot(&self) -> Result<Option<RetainedBeliefSnapshot>> {
        if !has_migration_9(&self.db)? {
            return Ok(None);
        }
        let row: Option<(i64, i64, String, String)> = self
            .db
            .query_row(
                "SELECT snapshot_seq, event_seq, report_json, computed_at
                 FROM epistemic_belief_snapshots ORDER BY snapshot_seq DESC LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        let Some((snapshot_seq, event_seq, report_json, computed_at)) = row else {
            return Ok(None);
        };
        let report: BeliefPropagationReport = serde_json::from_str(&report_json)?;
        Ok(Some(RetainedBeliefSnapshot {
            snapshot_seq,
            event_seq,
            report,
            computed_at,
        }))
    }
}
