//! Advisory approval of a reviewed adaptation plan; never proof or dispatch authority.
use super::{
    Change, ClaimState, StateStore, Trust, event, last_seq, pivot, read_intent,
    read_managed_proof_recipe, require_claim, reviewed_lean,
};
use crate::retained::valid_id;
use ai4math_core::{Engine, validate_source_path};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::path::Path;

pub(super) const APPROVED: &str = "SEDIMENT_ADAPTATION_APPROVED";
const TABLE: &str = "sediment_adaptation_approvals";
const MAX_REPO: usize = 4096;
const MAX_IDENTITY: usize = 256;

/// Exact proposed use of a historical Lean archive as an advisory input for a
/// different Claim. The coordinator must establish the archive, Git, and frozen
/// slot provenance; neither this plan nor its digest proves any of those facts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SedimentAdaptationPlan {
    pub archive_id: String,
    pub origin_claim: String,
    pub promoted_commit: String,
    pub target_claim: String,
    pub reviewed_commit: String,
    pub repo: String,
    /// The destination module, not the historical archive's source module.
    pub target_module: String,
    pub target_decl: String,
    /// SHA-256 of the exact destination source with its proof slot frozen.
    pub frozen_source_sha256: String,
    pub spec_file: String,
    pub spec_decl: String,
    pub spec_prefix_sha256: String,
    pub expected_event_seq: i64,
}

/// Durable historical readback, not a task enrollment or a proof receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SedimentAdaptationApproval {
    pub approval_sha256: String,
    pub event_seq: i64,
    pub plan: SedimentAdaptationPlan,
}

impl SedimentAdaptationPlan {
    pub fn approval_sha256(&self) -> Result<String> {
        self.validate()?;
        // The versioned tuple fixes field order independently of object serialization.
        let bytes = serde_json::to_vec(&(
            "ai4math:sediment-adaptation-approval:v1",
            &self.archive_id,
            &self.origin_claim,
            &self.promoted_commit,
            &self.target_claim,
            &self.reviewed_commit,
            &self.repo,
            &self.target_module,
            &self.target_decl,
            &self.frozen_source_sha256,
            &self.spec_file,
            &self.spec_decl,
            &self.spec_prefix_sha256,
            self.expected_event_seq,
        ))?;
        Ok(format!("{:x}", Sha256::digest(bytes)))
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            self.archive_id.strip_prefix("SED-").is_some_and(valid_id),
            "invalid sediment archive ID; expected SED- followed by 64 lowercase hex digits"
        );
        ensure!(
            full_commit(&self.promoted_commit)
                && full_commit(&self.reviewed_commit)
                && self.promoted_commit.len() == self.reviewed_commit.len(),
            "expected full lowercase Git commits in the managed repository"
        );
        ensure!(
            self.repo.len() <= MAX_REPO
                && Path::new(&self.repo).is_absolute()
                && !self.repo.chars().any(char::is_control),
            "expected a bounded absolute managed repository path"
        );
        ensure!(
            bounded_text(&self.origin_claim) && bounded_text(&self.target_claim),
            "invalid or oversized sediment adaptation Claim ID"
        );
        ensure!(
            self.origin_claim != self.target_claim,
            "sediment adaptation requires a distinct target Claim"
        );
        ensure!(
            lean_identity(&self.target_module, &self.target_decl)
                && lean_identity(&self.spec_file, &self.spec_decl),
            "expected bounded canonical Lean files and declarations for the target and spec"
        );
        ensure!(
            valid_id(&self.frozen_source_sha256) && valid_id(&self.spec_prefix_sha256),
            "expected lowercase SHA-256 for the frozen destination source and reviewed spec prefix"
        );
        ensure!(
            self.expected_event_seq >= 0,
            "invalid reviewed ledger event sequence"
        );
        Ok(())
    }
}

fn bounded_text(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_IDENTITY
        && value == value.trim()
        && !value.chars().any(char::is_control)
}

fn full_commit(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn lean_identity(file: &str, decl: &str) -> bool {
    file.len() <= MAX_IDENTITY
        && validate_source_path(Engine::Lean4, file).is_ok()
        && decl.len() <= MAX_IDENTITY
        && !decl.starts_with("_root_.")
        && decl.split('.').all(|part| {
            part != "_"
                && part.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
                && part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'\''))
        })
}

fn claim_matches(claim: &ClaimState, file: &str, decl: &str) -> bool {
    claim.engine == "lean4"
        && claim.module_path.as_deref() == Some(file)
        && claim
            .formal_target
            .as_deref()
            .and_then(|s| s.split_once('#'))
            == Some((file, decl))
}

pub(super) fn validate_for_append(db: &Connection, plan: &SedimentAdaptationPlan) -> Result<()> {
    ensure!(
        read_intent(db)?.is_none() && pivot::read_pending(db)?.is_none(),
        "recover the pending promotion or Pivot before proceeding with sediment adaptation"
    );
    let repo: Option<String> = db
        .query_row("SELECT repo FROM managed_repo WHERE singleton=1", [], |r| {
            r.get(0)
        })
        .optional()?;
    ensure!(
        repo.as_deref() == Some(plan.repo.as_str()),
        "sediment adaptation repository is not the bound managed repository"
    );

    let origin = require_claim(db, &plan.origin_claim)?;
    ensure!(
        origin.local_trust != Trust::L2b && origin.effective_trust != Trust::L2b,
        "sediment adaptation origin Claim is refuted"
    );
    let recipe = read_managed_proof_recipe(db, &plan.origin_claim, Some(&plan.promoted_commit))?
        .context("sediment adaptation requires an exact historical managed promotion recipe")?;
    ensure!(
        claim_matches(&origin, &recipe.file_path, &recipe.target_decl),
        "sediment adaptation origin Claim does not match its historical Lean proof target"
    );
    let comparison = reviewed_lean::read_comparison(db, &plan.promoted_commit)?
        .context("historical managed promotion is missing its retained Lean comparison")?;
    ensure!(
        reviewed_lean::matches_managed_t1(db, &recipe, Some(&comparison), None)?,
        "retained Lean comparison does not identify the historical managed T1 proof receipt"
    );
    ensure!(
        comparison.spec_file == plan.spec_file
            && comparison.spec_decl == plan.spec_decl
            && comparison.spec_prefix_sha256 == plan.spec_prefix_sha256,
        "historical Lean comparison does not match the sediment adaptation spec identity"
    );

    let target = require_claim(db, &plan.target_claim)?;
    ensure!(
        !target.stale
            && !target.effective_stale
            && target.local_trust != Trust::L2b
            && !matches!(target.effective_trust, Trust::L2b | Trust::L3),
        "sediment adaptation target Claim is stale, refuted or inactive"
    );
    ensure!(
        read_managed_proof_recipe(db, &plan.target_claim, None)?.is_none(),
        "sediment adaptation target Claim already has a managed proof recipe"
    );
    ensure!(
        claim_matches(&target, &plan.target_module, &plan.target_decl),
        "sediment adaptation target does not match the registered Claim module and declaration"
    );
    // Managed files may be on a case-insensitive filesystem; Lean declarations
    // are still case-sensitive. Distinct Claim IDs alone do not prove separation.
    ensure!(
        !recipe.file_path.eq_ignore_ascii_case(&plan.target_module)
            || recipe.target_decl != plan.target_decl,
        "sediment adaptation requires a distinct physical Lean target"
    );

    for claim in [&origin, &target] {
        let binding = reviewed_lean::read_binding(db, &claim.id)?.with_context(|| {
            format!(
                "sediment adaptation Claim {} has no reviewed Lean spec binding",
                claim.id
            )
        })?;
        ensure!(
            binding.repo == plan.repo
                && binding.spec_file == plan.spec_file
                && binding.spec_decl == plan.spec_decl
                && binding.spec_prefix_sha256 == plan.spec_prefix_sha256,
            "reviewed Lean spec binding for {} does not match the sediment adaptation spec identity",
            claim.id
        );
    }
    Ok(())
}

pub(super) fn read_approval(
    db: &Connection,
    approval: &str,
) -> Result<Option<SedimentAdaptationApproval>> {
    let record = db
        .query_row(
            "SELECT approval_sha256,event_seq,archive_id,origin_claim,promoted_commit,target_claim,
                reviewed_commit,repo,target_module,target_decl,frozen_source_sha256,spec_file,
                spec_decl,spec_prefix_sha256,expected_event_seq
         FROM sediment_adaptation_approvals WHERE approval_sha256=?1",
            [approval],
            |row| {
                Ok(SedimentAdaptationApproval {
                    approval_sha256: row.get(0)?,
                    event_seq: row.get(1)?,
                    plan: SedimentAdaptationPlan {
                        archive_id: row.get(2)?,
                        origin_claim: row.get(3)?,
                        promoted_commit: row.get(4)?,
                        target_claim: row.get(5)?,
                        reviewed_commit: row.get(6)?,
                        repo: row.get(7)?,
                        target_module: row.get(8)?,
                        target_decl: row.get(9)?,
                        frozen_source_sha256: row.get(10)?,
                        spec_file: row.get(11)?,
                        spec_decl: row.get(12)?,
                        spec_prefix_sha256: row.get(13)?,
                        expected_event_seq: row.get(14)?,
                    },
                })
            },
        )
        .optional()?;
    if let Some(record) = &record {
        ensure!(
            record.plan.approval_sha256()? == approval,
            "retained sediment adaptation approval identity mismatch"
        );
        ensure!(
            db.query_row(
                "SELECT EXISTS(SELECT 1 FROM events WHERE event_seq=?1 AND event_type=?2
                 AND entity_type='claim' AND entity_id=?3 AND git_commit=?4
                 AND actor='operator:sediment-adaptation'
                 AND json_extract(payload_json,'$.approval_sha256')=?5
                 AND json_extract(payload_json,'$.archive_id')=?6
                 AND json_extract(payload_json,'$.origin_claim')=?7
                 AND json_extract(payload_json,'$.promoted_commit')=?8)",
                params![
                    record.event_seq,
                    APPROVED,
                    record.plan.target_claim,
                    record.plan.reviewed_commit,
                    approval,
                    record.plan.archive_id,
                    record.plan.origin_claim,
                    record.plan.promoted_commit
                ],
                |r| r.get::<_, bool>(0),
            )?,
            "retained sediment adaptation approval event identity mismatch"
        );
    }
    Ok(record)
}

impl StateStore {
    /// Append an explicitly approved advisory plan. The operator coordinator must
    /// authenticate the archive, the exact reviewed Git snapshot and destination
    /// frozen slot under the managed repository lock. The store checks only ledger
    /// identities and eligibility; it does not enroll work or write proof/Wiki state.
    pub fn approve_sediment_adaptation(
        &mut self,
        plan: &SedimentAdaptationPlan,
        approval: &str,
    ) -> Result<SedimentAdaptationApproval> {
        self.write(|tx| {
            ensure!(
                valid_id(approval) && plan.approval_sha256()? == approval,
                "approval SHA-256 does not match the sediment adaptation plan"
            );
            // Exact retries are historical readback, not new approvals at the
            // current snapshot. This intentionally precedes the append CAS.
            if let Some(previous) = read_approval(tx, approval)? {
                ensure!(
                    &previous.plan == plan,
                    "conflicting sediment adaptation approval"
                );
                return Ok(previous);
            }
            ensure!(
                last_seq(tx)? == plan.expected_event_seq,
                "ledger event sequence changed while reviewing sediment adaptation; review again"
            );
            validate_for_append(tx, plan)?;
            let event_seq = event(
                tx,
                Change {
                    actor: "operator:sediment-adaptation",
                    git_commit: Some(&plan.reviewed_commit),
                },
                APPROVED,
                "claim",
                &plan.target_claim,
                json!({
                    "approval_sha256": approval,
                    "archive_id": plan.archive_id,
                    "origin_claim": plan.origin_claim,
                    "promoted_commit": plan.promoted_commit,
                }),
            )?;
            tx.execute(
                "INSERT INTO sediment_adaptation_approvals (
                    approval_sha256,event_seq,archive_id,origin_claim,promoted_commit,target_claim,
                    reviewed_commit,repo,target_module,target_decl,frozen_source_sha256,spec_file,
                    spec_decl,spec_prefix_sha256,expected_event_seq
                 ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)",
                params![
                    approval,
                    event_seq,
                    plan.archive_id,
                    plan.origin_claim,
                    plan.promoted_commit,
                    plan.target_claim,
                    plan.reviewed_commit,
                    plan.repo,
                    plan.target_module,
                    plan.target_decl,
                    plan.frozen_source_sha256,
                    plan.spec_file,
                    plan.spec_decl,
                    plan.spec_prefix_sha256,
                    plan.expected_event_seq
                ],
            )?;
            Ok(SedimentAdaptationApproval {
                approval_sha256: approval.to_owned(),
                event_seq,
                plan: plan.clone(),
            })
        })
    }

    /// Read an existing historical approval; never implies current eligibility.
    pub fn sediment_adaptation(
        &self,
        approval: &str,
    ) -> Result<Option<SedimentAdaptationApproval>> {
        ensure!(
            valid_id(approval),
            "invalid sediment adaptation approval SHA-256"
        );
        reviewed_lean::read_optional_table(&self.db, TABLE, |db| read_approval(db, approval))
    }
}

#[cfg(test)]
pub(super) mod tests;
