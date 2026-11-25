//! Retention of sealed Lean sediment replay evidence; this never confers trust.
use super::{
    Change, StateStore, Trust, event, last_seq, pivot, read_intent, read_managed_proof_recipe,
    require_claim, reviewed_lean,
};
use crate::retained::valid_id;
use ai4math_core::Engine;
use ai4math_evidence::{CandidateEvidence, LeanComparisonEvidence, ReceiptTier};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::Serialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::path::Path;

pub(super) const RETAINED: &str = "SEDIMENT_REPLAY_RETAINED";
const TABLE: &str = "sediment_replays";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct SedimentReplayScope<'a> {
    pub repo: &'a str,
    pub archive_id: &'a str,
    pub claim_id: &'a str,
    pub promoted_commit: &'a str,
    pub reviewed_commit: &'a str,
    /// Ledger head observed by the operator; only a precondition for a new append.
    pub expected_event_seq: i64,
}

/// Historical advisory readback, not a sealed broker result or a proof receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RetainedSedimentReplay {
    pub id: String,
    pub event_seq: i64,
    pub repo: String,
    pub archive_id: String,
    pub claim_id: String,
    pub promoted_commit: String,
    pub reviewed_commit: String,
    pub expected_event_seq: i64,
    pub candidate_receipt_sha256: String,
    pub comparison_receipt_sha256: Option<String>,
}

impl RetainedSedimentReplay {
    fn digest(&self) -> Result<String> {
        // This versioned tuple excludes the append sequence and CAS token. A
        // retry of the identical sealed evidence returns the first append.
        let bytes = serde_json::to_vec(&(
            "ai4math:retained-sediment-replay:v1",
            &self.repo,
            &self.archive_id,
            &self.claim_id,
            &self.promoted_commit,
            &self.reviewed_commit,
            &self.candidate_receipt_sha256,
            &self.comparison_receipt_sha256,
        ))?;
        Ok(format!("{:x}", Sha256::digest(bytes)))
    }
}

fn read_replay(db: &Connection, id: &str) -> Result<Option<RetainedSedimentReplay>> {
    let replay = db
        .query_row(
            "SELECT id,event_seq,repo,archive_id,claim_id,promoted_commit,reviewed_commit,
                expected_event_seq,candidate_receipt_sha256,comparison_receipt_sha256
         FROM sediment_replays WHERE id=?1",
            [id],
            |r| {
                Ok(RetainedSedimentReplay {
                    id: r.get(0)?,
                    event_seq: r.get(1)?,
                    repo: r.get(2)?,
                    archive_id: r.get(3)?,
                    claim_id: r.get(4)?,
                    promoted_commit: r.get(5)?,
                    reviewed_commit: r.get(6)?,
                    expected_event_seq: r.get(7)?,
                    candidate_receipt_sha256: r.get(8)?,
                    comparison_receipt_sha256: r.get(9)?,
                })
            },
        )
        .optional()?;
    if let Some(replay) = &replay {
        ensure!(
            replay.digest()? == id,
            "retained sediment replay identity mismatch"
        );
    }
    Ok(replay)
}

fn full_commit(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn validate_binding(
    db: &Connection,
    scope: SedimentReplayScope<'_>,
    candidate: &dyn CandidateEvidence,
    comparison: Option<&dyn LeanComparisonEvidence>,
) -> Result<()> {
    ensure!(
        read_intent(db)?.is_none() && pivot::read_pending(db)?.is_none(),
        "recover the pending promotion or Pivot before retaining sediment replay"
    );
    let repo: Option<String> = db
        .query_row("SELECT repo FROM managed_repo WHERE singleton=1", [], |r| {
            r.get(0)
        })
        .optional()?;
    ensure!(
        repo.as_deref() == Some(scope.repo),
        "sediment replay repository is not the bound managed repository"
    );

    let claim = require_claim(db, scope.claim_id)?;
    ensure!(
        claim.local_trust != Trust::L2b && claim.effective_trust != Trust::L2b,
        "cannot retain a sediment replay for a refuted Claim"
    );
    let recipe = read_managed_proof_recipe(db, scope.claim_id, Some(scope.promoted_commit))?
        .context("sediment replay requires an exact historical managed promotion recipe")?;
    let target = format!("{}#{}", recipe.file_path, recipe.target_decl);
    ensure!(
        claim.engine == "lean4"
            && claim.module_path.as_deref() == Some(recipe.file_path.as_str())
            && claim.formal_target.as_deref() == Some(target.as_str()),
        "registered Claim does not match the historical Lean proof target"
    );
    let receipt = candidate.receipt();
    ensure!(
        receipt.matches(
            scope.reviewed_commit,
            &recipe.file_path,
            candidate.source().as_bytes()
        ) && receipt.target_id() == recipe.target_decl
            && receipt.patch_sha256() == recipe.source_sha256,
        "sediment replay candidate does not match the reviewed commit and historical proof target and source"
    );

    let spec = reviewed_lean::read_binding(db, scope.claim_id)?;
    let original = reviewed_lean::read_comparison(db, scope.promoted_commit)?;
    match (spec, comparison, original.as_ref()) {
        (None, None, None) => {
            ensure!(
                reviewed_lean::binding_owner(db, &recipe.file_path, &recipe.target_decl)?.is_none(),
                "Lean target is bound to a reviewed spec on another Claim"
            );
            ensure!(
                reviewed_lean::matches_managed_t1(
                    db,
                    &recipe,
                    None,
                    Some(receipt.dependencies_sha256())
                )?,
                "unbound sediment replay does not match the managed T1 proof and dependency prefix"
            );
        }
        (Some(spec), Some(comparison), Some(original)) => {
            ensure!(
                spec.repo == scope.repo
                    && original.spec_file == spec.spec_file
                    && original.spec_decl == spec.spec_decl
                    && original.spec_prefix_sha256 == spec.spec_prefix_sha256,
                "original Lean comparison does not match the current reviewed spec binding"
            );
            ensure!(
                comparison.spec_file() == spec.spec_file
                    && comparison.spec_decl() == spec.spec_decl
                    && comparison.spec_prefix_sha256() == spec.spec_prefix_sha256,
                "fresh Lean comparison does not match the current reviewed spec binding"
            );
            ensure!(
                comparison.base_commit() == receipt.base_commit()
                    && comparison.candidate_receipt_sha256() == receipt.receipt_sha256()
                    && comparison.candidate_file() == receipt.file_path()
                    && comparison.candidate_decl() == receipt.target_id()
                    && comparison.candidate_source_sha256() == receipt.patch_sha256(),
                "fresh Lean comparison does not match this exact replay candidate receipt"
            );
            ensure!(
                reviewed_lean::matches_managed_t1(db, &recipe, Some(original), None)?,
                "retained Lean comparison does not identify its managed T1 proof receipt"
            );
        }
        _ => anyhow::bail!(
            "reviewed Lean sediment replay requires a current spec binding and both original and fresh comparisons, or none of them"
        ),
    }
    Ok(())
}

// Call only after validating the sealed evidence and ledger binding in this transaction.
fn retain_bound_replay(
    tx: &Transaction<'_>,
    scope: SedimentReplayScope<'_>,
    candidate_receipt_sha256: &str,
    comparison_receipt_sha256: Option<&str>,
) -> Result<RetainedSedimentReplay> {
    let mut record = RetainedSedimentReplay {
        id: String::new(),
        event_seq: 0,
        repo: scope.repo.into(),
        archive_id: scope.archive_id.into(),
        claim_id: scope.claim_id.into(),
        promoted_commit: scope.promoted_commit.into(),
        reviewed_commit: scope.reviewed_commit.into(),
        expected_event_seq: scope.expected_event_seq,
        candidate_receipt_sha256: candidate_receipt_sha256.into(),
        comparison_receipt_sha256: comparison_receipt_sha256.map(str::to_owned),
    };
    record.id = record.digest()?;
    if let Some(previous) = read_replay(tx, &record.id)? {
        record.event_seq = previous.event_seq;
        record.expected_event_seq = previous.expected_event_seq;
        ensure!(record == previous, "conflicting sediment replay evidence");
        return Ok(previous);
    }
    ensure!(
        last_seq(tx)? == scope.expected_event_seq,
        "ledger event sequence changed while verifying sediment replay; review again"
    );
    record.event_seq = event(
        tx,
        Change {
            actor: "broker",
            git_commit: Some(scope.reviewed_commit),
        },
        RETAINED,
        "claim",
        scope.claim_id,
        json!({
            "id": record.id,
            "archive_id": record.archive_id,
            "promoted_commit": record.promoted_commit,
            "candidate_receipt_sha256": record.candidate_receipt_sha256,
            "comparison_receipt_sha256": record.comparison_receipt_sha256,
        }),
    )?;
    tx.execute(
        "INSERT INTO sediment_replays (
            id,event_seq,repo,archive_id,claim_id,promoted_commit,reviewed_commit,
            expected_event_seq,candidate_receipt_sha256,comparison_receipt_sha256
         ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
        params![
            record.id,
            record.event_seq,
            record.repo,
            record.archive_id,
            record.claim_id,
            record.promoted_commit,
            record.reviewed_commit,
            record.expected_event_seq,
            record.candidate_receipt_sha256,
            record.comparison_receipt_sha256
        ],
    )?;
    Ok(record)
}

impl StateStore {
    /// Retain a successful native Lean assessment as advisory evidence only.
    /// The operator coordinator must validate the archive and Git provenance
    /// (including the current main) while holding the managed repository lock.
    pub fn retain_sediment_replay(
        &mut self,
        scope: SedimentReplayScope<'_>,
        candidate: &dyn CandidateEvidence,
        comparison: Option<&dyn LeanComparisonEvidence>,
    ) -> Result<RetainedSedimentReplay> {
        ensure!(
            Path::new(scope.repo).is_absolute() && !scope.repo.chars().any(char::is_control),
            "expected a canonical managed repository path"
        );
        ensure!(
            scope.archive_id.strip_prefix("SED-").is_some_and(valid_id),
            "invalid sediment archive ID; expected SED- followed by 64 lowercase hex digits"
        );
        ensure!(
            full_commit(scope.promoted_commit)
                && full_commit(scope.reviewed_commit)
                && scope.promoted_commit.len() == scope.reviewed_commit.len(),
            "expected full lowercase Git commits in the managed repository"
        );
        ensure!(
            scope.expected_event_seq >= 0,
            "invalid reviewed ledger event sequence"
        );
        let receipt = candidate.receipt();
        ensure!(
            candidate.is_assessed_t3()
                && receipt.tier() == ReceiptTier::T3TaintRebuild
                && receipt.engine() == Engine::Lean4,
            "sediment replay requires a natively assessed Lean T3 candidate"
        );

        self.write(|tx| {
            validate_binding(tx, scope, candidate, comparison)?;
            retain_bound_replay(
                tx,
                scope,
                receipt.receipt_sha256(),
                comparison.map(|comparison| comparison.receipt_sha256()),
            )
        })
    }

    /// Read historical advisory evidence by lowercase SHA-256 observation ID.
    pub fn sediment_replay(&self, id: &str) -> Result<Option<RetainedSedimentReplay>> {
        ensure!(valid_id(id), "invalid retained sediment replay ID");
        reviewed_lean::read_optional_table(&self.db, TABLE, |db| read_replay(db, id))
    }
}

#[cfg(test)]
mod tests;
