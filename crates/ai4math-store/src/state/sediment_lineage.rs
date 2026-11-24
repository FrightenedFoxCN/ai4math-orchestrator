//! Historical adaptation lineage only. Neither the link nor its prospective
//! CORE identifier confers proof authority or materializes a Wiki entry.
use super::{
    Change, LeanSpecComparison, ManagedProofRecipe, SedimentAdaptationPlan, StateStore, Trust,
    event, last_seq, pivot, read_intent, read_managed_proof_recipe, require_claim, reviewed_lean,
    sediment_adaptation,
};
use crate::retained::valid_id;
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::Path;

pub(super) const RETAINED: &str = "SEDIMENT_ADAPTATION_LINEAGE_RETAINED";
const TABLE: &str = "sediment_adaptation_lineage";
const ACTOR: &str = "operator:sediment-adaptation";

#[derive(Debug, Clone, Copy)]
pub struct SedimentLineageInput<'a> {
    pub approval_sha256: &'a str,
    pub repo: &'a str,
    pub promoted_commit: &'a str,
    /// Ledger head observed by the operator; an append precondition, not part of the identity.
    pub expected_event_seq: i64,
}

/// An immutable historical edge linking approval, native suggestion and recorded
/// T1 promotion. This attests to matching evidence, not to current proof status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SedimentLineage {
    pub approval_sha256: String,
    pub archive_id: String,
    pub origin_claim: String,
    pub origin_promoted_commit: String,
    pub target_claim: String,
    pub repo: String,
    pub promoted_commit: String,
    pub task_id: String,
    /// Reserved prospective identifier only; no CORE or Wiki entry was written.
    pub core_entry_id: String,
    pub core_entry_prospective: bool,
    pub origin_t1_receipt_sha256: String,
    pub origin_comparison_receipt_sha256: String,
    /// Digest of the recorded destination T1 receipt, not the untrusted candidate.
    pub candidate_receipt_sha256: String,
    pub comparison_receipt_sha256: String,
    pub expected_event_seq: i64,
    pub event_seq: i64,
}

/// A current ledger candidate for native CORE projection, not a Git verification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SedimentCoreEvidence {
    pub lineage: SedimentLineage,
    /// The recorded T1 promotion or the exact current T3 review snapshot.
    pub verified_at_commit: String,
}

impl SedimentLineage {
    fn event_payload(&self) -> Value {
        json!({
            "approval_sha256": self.approval_sha256,
            "archive_id": self.archive_id,
            "origin_claim": self.origin_claim,
            "origin_promoted_commit": self.origin_promoted_commit,
            "promoted_commit": self.promoted_commit,
            "task_id": self.task_id,
            "core_entry_id": self.core_entry_id,
            "core_entry_prospective": self.core_entry_prospective,
            "origin_t1_receipt_sha256": self.origin_t1_receipt_sha256,
            "origin_comparison_receipt_sha256": self.origin_comparison_receipt_sha256,
            "candidate_receipt_sha256": self.candidate_receipt_sha256,
            "comparison_receipt_sha256": self.comparison_receipt_sha256,
            "expected_event_seq": self.expected_event_seq,
        })
    }
}

fn read_lineage(db: &Connection, approval: &str) -> Result<Option<SedimentLineage>> {
    let task_id = StateStore::sediment_adaptation_task_id(approval)?;
    let record = db.query_row(
        "SELECT a.archive_id,a.origin_claim,a.promoted_commit,a.target_claim,a.repo,
                l.promoted_commit,l.origin_t1_receipt_sha256,l.origin_comparison_receipt_sha256,
                l.candidate_receipt_sha256,l.comparison_receipt_sha256,l.expected_event_seq,l.event_seq
         FROM sediment_adaptation_lineage l JOIN sediment_adaptation_approvals a USING(approval_sha256)
         WHERE l.approval_sha256=?1",
        [approval],
        |r| Ok(SedimentLineage {
            approval_sha256: approval.into(),
            archive_id: r.get(0)?,
            origin_claim: r.get(1)?,
            origin_promoted_commit: r.get(2)?,
            target_claim: r.get(3)?,
            repo: r.get(4)?,
            promoted_commit: r.get(5)?,
            task_id,
            core_entry_id: format!("CORE-ADAPT-{approval}"),
            core_entry_prospective: true,
            origin_t1_receipt_sha256: r.get(6)?,
            origin_comparison_receipt_sha256: r.get(7)?,
            candidate_receipt_sha256: r.get(8)?,
            comparison_receipt_sha256: r.get(9)?,
            expected_event_seq: r.get(10)?,
            event_seq: r.get(11)?,
        }),
    ).optional()?;
    if let Some(record) = &record {
        let payload: Option<String> = db
            .query_row(
                "SELECT payload_json FROM events WHERE event_seq=?1 AND event_type=?2
             AND entity_type='claim' AND entity_id=?3 AND git_commit=?4 AND actor=?5",
                params![
                    record.event_seq,
                    RETAINED,
                    record.target_claim,
                    record.promoted_commit,
                    ACTOR
                ],
                |r| r.get(0),
            )
            .optional()?;
        ensure!(
            payload
                .as_deref()
                .and_then(|p| serde_json::from_str::<Value>(p).ok())
                == Some(record.event_payload()),
            "retained sediment adaptation lineage event identity mismatch"
        );
    }
    Ok(record)
}

fn exact_comparison(
    db: &Connection,
    plan: &SedimentAdaptationPlan,
    recipe: &ManagedProofRecipe,
) -> Result<LeanSpecComparison> {
    let comparison = reviewed_lean::read_comparison(db, &recipe.promoted_commit)?
        .context("recorded promotion is missing its retained Lean comparison")?;
    ensure!(
        comparison.spec_file == plan.spec_file
            && comparison.spec_decl == plan.spec_decl
            && comparison.spec_prefix_sha256 == plan.spec_prefix_sha256,
        "retained Lean comparison does not match the approved spec identity"
    );
    ensure!(
        reviewed_lean::matches_managed_t1(db, recipe, Some(&comparison), None)?,
        "retained Lean comparison does not identify the exact recorded T1 receipt"
    );
    Ok(comparison)
}

/// Find the recheck bound to the current receipt, never merely the newest
/// recheck for a Claim or any recheck of the same declaration.
fn current_recheck_commit(
    db: &Connection,
    lineage: &SedimentLineage,
    receipt_id: &str,
) -> Result<Option<String>> {
    let plan_hash: Option<String> = db
        .query_row(
            "SELECT q.plan_sha256 FROM verification_receipts r
             JOIN applied_t3_plans p ON p.reviewed_commit=r.base_commit
             JOIN lean_spec_rechecks q ON q.plan_sha256=p.plan_sha256 AND q.claim_id=?1
             WHERE r.receipt_id=?2 AND q.promoted_commit=?3
             AND r.tier='T3_TAINT_REBUILD' AND r.engine='lean4'
             AND r.allowlist_passed=1 AND r.external_checker_passed IS NOT 0
             AND q.candidate_receipt_sha256=r.receipt_sha256 AND q.reviewed_commit=r.base_commit
             AND q.candidate_file=json_extract(r.raw_receipt_json,'$.file_path')
             AND q.candidate_decl=r.target_decl AND q.candidate_source_sha256=r.patch_sha256",
            params![lineage.target_claim, receipt_id, lineage.promoted_commit],
            |r| r.get(0),
        )
        .optional()?;
    let Some(plan_hash) = plan_hash else {
        return Ok(None);
    };
    let recheck = reviewed_lean::read_recheck(db, &lineage.target_claim, &plan_hash)?
        .context("matched Lean recheck vanished during projection")?;
    let approval = sediment_adaptation::read_approval(db, &lineage.approval_sha256)?
        .context("lineage approval vanished during projection")?;
    let plan = approval.plan;
    let Some(binding) = reviewed_lean::read_binding(db, &lineage.target_claim)? else {
        return Ok(None);
    };
    let Some(recipe) =
        read_managed_proof_recipe(db, &lineage.target_claim, Some(&lineage.promoted_commit))?
    else {
        return Ok(None);
    };
    let Some(comparison) = reviewed_lean::read_comparison(db, &lineage.promoted_commit)? else {
        return Ok(None);
    };
    let spec = (&plan.spec_file, &plan.spec_decl, &plan.spec_prefix_sha256);
    if binding.repo != plan.repo
        || (
            &binding.spec_file,
            &binding.spec_decl,
            &binding.spec_prefix_sha256,
        ) != spec
        || (
            &comparison.spec_file,
            &comparison.spec_decl,
            &comparison.spec_prefix_sha256,
        ) != spec
        || (
            &recheck.spec_file,
            &recheck.spec_decl,
            &recheck.spec_prefix_sha256,
        ) != spec
        || comparison.candidate_receipt_sha256 != lineage.candidate_receipt_sha256
        || comparison.receipt_sha256 != lineage.comparison_receipt_sha256
        || recipe.base_commit != plan.reviewed_commit
        || recipe.file_path != plan.target_module
        || recipe.target_decl != plan.target_decl
        || recheck.candidate_file != recipe.file_path
        || recheck.candidate_decl != recipe.target_decl
        || recheck.candidate_source_sha256 != recipe.source_sha256
        || !reviewed_lean::matches_managed_t1(db, &recipe, Some(&comparison), None)?
    {
        return Ok(None);
    }

    // Rechecks are written atomically with this trust event. A historical
    // match is insufficient: require the latest local trust or taint event
    // for the Claim to be this exact restoration and name this receipt ID.
    let trust: Option<String> = db
        .query_row(
            "SELECT payload_json FROM events WHERE event_seq=(
                 SELECT event_seq FROM events WHERE entity_type='claim' AND entity_id=?1
                 AND event_type IN ('CLAIM_TRUST_CHANGED','CLAIM_TAINTED')
                 ORDER BY event_seq DESC LIMIT 1
             ) AND event_type='CLAIM_TRUST_CHANGED' AND git_commit=?2 AND event_seq>?3",
            params![
                lineage.target_claim,
                recheck.reviewed_commit,
                lineage.event_seq
            ],
            |r| r.get(0),
        )
        .optional()?;
    let expected = json!({
        "previous_local": Trust::L5,
        "local": Trust::L5,
        "was_stale": true,
        "detail": {"receipt_id": receipt_id, "lean_spec_recheck": recheck},
    });
    if trust.as_deref().and_then(|p| serde_json::from_str(p).ok()) != Some(expected) {
        return Ok(None);
    }
    let applied = super::require_applied_t3_plan(db, &plan_hash)?;
    Ok((applied.approval.reviewed_commit == recheck.reviewed_commit
        && applied.claims.contains(&lineage.target_claim))
    .then_some(recheck.reviewed_commit))
}

// Mirror the native proof-body envelope. Deserialize the typed variant rather
// than projecting JSON with SQL: alternate/duplicate/unknown fields must fail
// closed, and equality is over the exact decoded body (no trimming).
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum NativeProofBody {
    ProofBody { proof_body: String },
}

impl StateStore {
    /// Read a historical lineage record, without checking current eligibility.
    pub fn sediment_lineage(&self, approval: &str) -> Result<Option<SedimentLineage>> {
        ensure!(
            valid_id(approval),
            "invalid sediment adaptation approval SHA-256"
        );
        reviewed_lean::read_optional_table(&self.db, TABLE, |db| read_lineage(db, approval))
    }

    /// Bounded projection input only. Historical lineage readback is separate.
    /// The destination must be non-stale L5 under the exact recorded T1 or a
    /// sealed T3 recheck that restores that same latest T1 promotion. The caller
    /// must separately establish current Git ancestry and source compatibility.
    pub fn current_sediment_lineages(&self) -> Result<Vec<SedimentCoreEvidence>> {
        const LIMIT: usize = 64;
        Ok(reviewed_lean::read_optional_table(&self.db, TABLE, |db| {
            let snapshot = db.unchecked_transaction()?;
            let mut statement = snapshot.prepare(
                "SELECT l.approval_sha256,c.receipt_id,(c.receipt_id=p.receipt_id)
                 FROM sediment_adaptation_lineage l
                 JOIN sediment_adaptation_approvals a USING(approval_sha256)
                 JOIN recorded_promotions p ON p.git_commit=l.promoted_commit AND p.claim_id=a.target_claim
                 JOIN claims c ON c.claim_id=a.target_claim
                 JOIN verification_receipts t1 ON t1.receipt_id=p.receipt_id
                    AND t1.receipt_sha256=l.candidate_receipt_sha256
                 JOIN verification_receipts r ON r.receipt_id=c.receipt_id
                 WHERE c.engine='lean4' AND c.stale=0 AND c.effective_stale=0
                 AND c.local_trust_level='L5' AND c.effective_trust_level='L5'
                 AND t1.tier='T1_PROMOTION' AND t1.engine='lean4'
                 AND (r.receipt_id=p.receipt_id OR (r.tier='T3_TAINT_REBUILD' AND r.engine='lean4'))
                 AND c.module_path=a.target_module AND c.formal_target=(a.target_module || '#' || a.target_decl)
                 AND NOT EXISTS(SELECT 1 FROM recorded_promotions newer WHERE newer.claim_id=c.claim_id AND newer.event_seq>p.event_seq)
                 ORDER BY l.event_seq LIMIT ?1",
            )?;
            let mut candidates = statement.query([(LIMIT + 1) as i64])?;
            let mut evidence = Vec::new();
            let mut examined = 0;
            while let Some(row) = candidates.next()? {
                examined += 1;
                ensure!(examined <= LIMIT, "managed CORE projection limit exceeded; narrow the projection before rebuilding");
                let approval: String = row.get(0)?;
                let lineage = read_lineage(&snapshot, &approval)?
                    .context("lineage vanished during projection")?;
                let verified_at_commit = if row.get::<_, bool>(2)? {
                    lineage.promoted_commit.clone()
                } else {
                    let receipt_id: String = row.get(1)?;
                    let Some(commit) = current_recheck_commit(&snapshot, &lineage, &receipt_id)? else {
                        continue;
                    };
                    commit
                };
                evidence.push(SedimentCoreEvidence { lineage, verified_at_commit });
            }
            Ok(Some(evidence))
        })?.unwrap_or_default())
    }

    /// Retain an explicitly reviewed historical lineage edge. The coordinator
    /// establishes Git/archive provenance under the repository lock; the Store
    /// validates the exact ledger relationships. This writes only lineage and
    /// its event, not proof, Wiki, trust, or scheduling state.
    pub fn retain_sediment_lineage(
        &mut self,
        input: SedimentLineageInput<'_>,
    ) -> Result<SedimentLineage> {
        ensure!(
            valid_id(input.approval_sha256),
            "invalid sediment adaptation approval SHA-256"
        );
        ensure!(
            input.repo.len() <= 4096
                && Path::new(input.repo).is_absolute()
                && !input.repo.chars().any(char::is_control),
            "expected a bounded absolute managed repository path"
        );
        ensure!(
            matches!(input.promoted_commit.len(), 40 | 64)
                && input
                    .promoted_commit
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "expected a full lowercase promoted Git commit"
        );
        ensure!(
            input.expected_event_seq >= 0,
            "invalid reviewed ledger event sequence"
        );
        self.write(|tx| {
            // Exact retries are historical lookup, not a new operation against
            // current trust, pending journals or ledger head.
            if let Some(old) = read_lineage(tx, input.approval_sha256)? {
                ensure!(
                    old.repo == input.repo && old.promoted_commit == input.promoted_commit,
                    "conflicting sediment adaptation lineage for this approval"
                );
                return Ok(old);
            }
            ensure!(
                last_seq(tx)? == input.expected_event_seq,
                "ledger event sequence changed while reviewing sediment adaptation lineage; review again"
            );
            ensure!(
                read_intent(tx)?.is_none() && pivot::read_pending(tx)?.is_none(),
                "recover the pending promotion or Pivot before retaining sediment adaptation lineage"
            );
            let repo: Option<String> = tx.query_row(
                "SELECT repo FROM managed_repo WHERE singleton=1", [], |r| r.get(0),
            ).optional()?;
            ensure!(
                repo.as_deref() == Some(input.repo),
                "sediment adaptation lineage repository is not the bound managed repository"
            );
            let approval = sediment_adaptation::read_approval(tx, input.approval_sha256)?
                .context("unknown sediment adaptation approval")?;
            let plan = approval.plan;
            ensure!(
                plan.repo == input.repo && input.promoted_commit.len() == plan.reviewed_commit.len()
                    && input.promoted_commit != plan.reviewed_commit
                    && input.promoted_commit != plan.promoted_commit,
                "sediment adaptation lineage requires a distinct new promotion in the approved repository"
            );
            for claim_id in [&plan.origin_claim, &plan.target_claim] {
                let binding = reviewed_lean::read_binding(tx, claim_id)?
                    .context("sediment adaptation lineage Claim lacks a reviewed Lean spec binding")?;
                ensure!(
                    binding.repo == plan.repo && binding.spec_file == plan.spec_file
                        && binding.spec_decl == plan.spec_decl
                        && binding.spec_prefix_sha256 == plan.spec_prefix_sha256,
                    "reviewed Lean spec binding does not match the approved spec identity"
                );
            }
            let origin_claim = require_claim(tx, &plan.origin_claim)?;
            ensure!(
                origin_claim.local_trust != Trust::L2b && origin_claim.effective_trust != Trust::L2b,
                "sediment adaptation lineage origin Claim is refuted"
            );
            let origin = read_managed_proof_recipe(tx, &plan.origin_claim, Some(&plan.promoted_commit))?
                .context("sediment adaptation lineage requires the historical origin promotion recipe")?;
            let original = exact_comparison(tx, &plan, &origin)?;

            let target = require_claim(tx, &plan.target_claim)?;
            ensure!(
                !target.stale && !target.effective_stale
                    && target.local_trust == Trust::L5 && target.effective_trust == Trust::L5,
                "sediment adaptation lineage requires a non-stale local and effective L5 target Claim"
            );
            ensure!(
                target.engine == "lean4"
                    && target.module_path.as_deref() == Some(plan.target_module.as_str())
                    && target.formal_target.as_deref().and_then(|s| s.split_once('#'))
                        == Some((plan.target_module.as_str(), plan.target_decl.as_str()))
                    && (!origin.file_path.eq_ignore_ascii_case(&plan.target_module)
                        || origin.target_decl != plan.target_decl),
                "recorded destination does not match the approved distinct Lean target"
            );
            let recipe = read_managed_proof_recipe(tx, &plan.target_claim, None)?
                .context("sediment adaptation lineage requires a latest managed destination promotion recipe")?;
            ensure!(
                recipe.promoted_commit == input.promoted_commit
                    && recipe.base_commit == plan.reviewed_commit
                    && recipe.file_path == plan.target_module && recipe.target_decl == plan.target_decl,
                "latest destination promotion does not match the approved Lean target and base"
            );
            let recorded_seq: i64 = tx.query_row(
                "SELECT event_seq FROM recorded_promotions
                 WHERE git_commit=?1 AND claim_id=?2 AND receipt_id=?3",
                params![recipe.promoted_commit, plan.target_claim, target.receipt_id],
                |r| r.get(0),
            ).optional()?.context("latest destination promotion receipt is not the current Claim receipt")?;
            let promoted = exact_comparison(tx, &plan, &recipe)?;
            let task_id = Self::sediment_adaptation_task_id(input.approval_sha256)?;
            let (task_seq, result): (i64, String) = tx.query_row(
                "SELECT t.queue_seq,c.result_json FROM scheduler_sediment_adaptations l
                 JOIN scheduler_tasks t ON t.queue_seq=l.task_seq
                 JOIN scheduler_task_candidates c ON c.task_seq=l.task_seq
                 WHERE l.approval_sha256=?1 AND t.task_id=?2
                   AND t.track='infrastructure' AND t.engine='lean4'
                   AND t.module_path=?3 AND t.target=?4 AND t.completed_at_ms IS NOT NULL
                   AND c.claim_id=?5 AND c.base_commit=?6 AND c.role='prover_worker'
                   AND l.registered_event_seq>?7
                   AND EXISTS(SELECT 1 FROM events e WHERE e.event_type='TASK_COMPLETED'
                        AND e.entity_type='task' AND e.entity_id=t.task_id
                        AND e.event_seq>l.registered_event_seq AND e.event_seq<?8
                        AND json_type(e.payload_json,'$.untrusted_candidate')='true')",
                params![input.approval_sha256, task_id, plan.target_module, plan.target_decl,
                    plan.target_claim, recipe.base_commit, approval.event_seq, recorded_seq],
                |r| Ok((r.get(0)?, r.get(1)?)),
            ).optional()?.context("sediment adaptation lineage requires a completed native candidate matching the exact approval scope")?;
            let NativeProofBody::ProofBody { proof_body } = serde_json::from_str(&result)
                .map_err(|_| anyhow::anyhow!("scoped native candidate is not a supported proof-body result"))?;
            ensure!(proof_body == recipe.proof_body, "scoped native proof body does not exactly match the promoted recipe");

            ensure!(
                !tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM sediment_adaptation_lineage WHERE promoted_commit=?1)",
                    [&recipe.promoted_commit], |r| r.get::<_, bool>(0),
                )?,
                "destination promotion already has retained sediment adaptation lineage"
            );

            let mut record = SedimentLineage {
                approval_sha256: input.approval_sha256.into(),
                archive_id: plan.archive_id,
                origin_claim: plan.origin_claim,
                origin_promoted_commit: plan.promoted_commit,
                target_claim: plan.target_claim,
                repo: plan.repo,
                promoted_commit: recipe.promoted_commit,
                task_id,
                core_entry_id: format!("CORE-ADAPT-{}", input.approval_sha256),
                core_entry_prospective: true,
                origin_t1_receipt_sha256: original.candidate_receipt_sha256,
                origin_comparison_receipt_sha256: original.receipt_sha256,
                candidate_receipt_sha256: promoted.candidate_receipt_sha256,
                comparison_receipt_sha256: promoted.receipt_sha256,
                expected_event_seq: input.expected_event_seq,
                event_seq: 0,
            };
            record.event_seq = event(
                tx,
                Change { actor: ACTOR, git_commit: Some(&record.promoted_commit) },
                RETAINED,
                "claim",
                &record.target_claim,
                record.event_payload(),
            )?;
            tx.execute(
                "INSERT INTO sediment_adaptation_lineage (
                    approval_sha256,task_seq,promoted_commit,origin_t1_receipt_sha256,
                    origin_comparison_receipt_sha256,candidate_receipt_sha256,
                    comparison_receipt_sha256,expected_event_seq,event_seq
                 ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                params![record.approval_sha256, task_seq, record.promoted_commit,
                    record.origin_t1_receipt_sha256, record.origin_comparison_receipt_sha256,
                    record.candidate_receipt_sha256, record.comparison_receipt_sha256,
                    record.expected_event_seq, record.event_seq],
            )?;
            Ok(record)
        })
    }
}

#[cfg(test)]
mod tests;
