//! Typed, evidence-gated Claim trust transitions.
//!
//! Raw trust assignment is intentionally not exposed to the control plane.
//! These transitions derive the target level from retained evidence and bind it
//! to the live Claim and managed Git context.

use super::reviewed_lean::ComparedSpec;
use super::{
    Change, ClaimState, StateStore, Trust, VerifiedReceipt, event, persist_receipt, recompute,
    require_claim, update_trust,
};
use ai4math_evidence::{CandidateEvidence, LeanComparisonEvidence, ReceiptTier};
use anyhow::{Context, Result, ensure};
use rusqlite::OptionalExtension;
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

#[derive(Debug)]
pub enum TrustTransitionError {
    ObservationNotFound,
    ContextMismatch,
    UnsupportedOutcome,
    SketchEvidenceMismatch,
}

impl std::fmt::Display for TrustTransitionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::ObservationNotFound => "retained Oracle observation not found",
            Self::ContextMismatch => "Oracle observation does not match the live Claim context",
            Self::UnsupportedOutcome => "Oracle outcome does not authorize a trust transition",
            Self::SketchEvidenceMismatch => {
                "formal sketch evidence does not match the live Claim context"
            }
        })
    }
}
impl std::error::Error for TrustTransitionError {}

#[derive(Debug, Serialize)]
pub struct FormalSketchEvidence {
    pub sketch_id: String,
    pub claim_id: String,
    pub git_commit: String,
    pub module_path: String,
    pub target_decl: String,
    pub candidate_receipt_sha256: String,
    pub comparison_receipt_sha256: String,
    pub probe_observation_id: String,
    pub proof_body_sha256: String,
    pub event_seq: i64,
}

impl StateStore {
    /// Apply trust transitions justified by retained bounded Oracle evidence.
    /// L4a/L4b/L5 require their own typed evidence paths and cannot be requested here.
    pub fn apply_oracle_trust_transition(
        &mut self,
        claim_id: &str,
        observation_id: &str,
        current_head: &str,
        change: Change<'_>,
    ) -> Result<super::ClaimState> {
        let retained = self
            .oracle_observation(observation_id)?
            .ok_or(TrustTransitionError::ObservationNotFound)?;
        let outcome = retained
            .observation
            .get("outcome")
            .and_then(Value::as_str)
            .ok_or(TrustTransitionError::UnsupportedOutcome)?;
        let target = match outcome {
            "no_counterexample_within_bounds" => Trust::L1,
            "counterexample" => Trust::L2a,
            _ => return Err(TrustTransitionError::UnsupportedOutcome.into()),
        };
        self.write(|tx| {
            let claim = require_claim(tx, claim_id)?;
            let (engine, formal_target, informal_statement): (String, Option<String>, String) = tx
                .query_row(
                    "SELECT engine,formal_target,informal_statement FROM claims WHERE claim_id=?1",
                    [claim_id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )?;
            let repo: Option<String> = tx
                .query_row("SELECT repo FROM managed_repo WHERE singleton=1", [], |r| {
                    r.get(0)
                })
                .optional()?;
            let approval = &retained.approval;
            if approval.claim_id != claim_id
                || approval.commit != current_head
                || approval.repo != repo.unwrap_or_default()
                || approval.engine != engine
                || approval.formal_target != formal_target.unwrap_or_default()
                || approval.informal_statement != informal_statement
            {
                return Err(TrustTransitionError::ContextMismatch.into());
            }
            let replayed: Option<i64> = tx
                .query_row(
                    "SELECT event_seq FROM events
                 WHERE event_type='CLAIM_TRUST_CHANGED' AND entity_type='claim' AND entity_id=?1
                   AND json_extract(payload_json,'$.detail.oracle_observation_id')=?2
                 ORDER BY event_seq DESC LIMIT 1",
                    [claim_id, observation_id],
                    |r| r.get(0),
                )
                .optional()?;
            if replayed.is_some() {
                return Ok(claim);
            }
            update_trust(
                tx,
                &claim,
                target,
                None,
                change,
                json!({
                    "transition":"oracle_bounded_observation",
                    "oracle_observation_id":observation_id,
                    "approval_sha256":approval.approval_sha256,
                    "outcome":outcome,
                }),
            )?;
            event(
                tx,
                change,
                "CLAIM_ORACLE_TRUST_APPLIED",
                "claim",
                claim_id,
                json!({
                    "oracle_observation_id":observation_id,
                    "target":target,
                    "proof_verified":false,
                }),
            )?;
            recompute(tx, change)?;
            require_claim(tx, claim_id)
        })
    }

    /// Apply L3 only when a sealed T3 candidate, sealed reviewed-spec comparison
    /// and retained no-counterexample probe all describe the same live Claim.
    pub fn apply_formal_sketch_evidence(
        &mut self,
        claim_id: &str,
        current_head: &str,
        candidate: &dyn CandidateEvidence,
        comparison: &dyn LeanComparisonEvidence,
        probe_observation_id: &str,
        change: Change<'_>,
    ) -> Result<ClaimState> {
        ensure!(
            candidate.receipt().tier() == ReceiptTier::T3TaintRebuild && candidate.is_assessed_t3(),
            "formal sketch requires an assessed T3 candidate"
        );
        let receipt = VerifiedReceipt::from_compared_recheck(candidate, comparison)?;
        let compared = receipt
            .lean_comparison
            .as_ref()
            .context("formal sketch comparison was not retained")?;
        self.apply_formal_sketch_receipt(
            claim_id,
            current_head,
            &receipt,
            compared,
            probe_observation_id,
            change,
        )
    }

    /// Store-local entry used after sealed evidence has been projected. The
    /// receipt fields are private, so external JSON cannot mint this evidence.
    pub(super) fn apply_formal_sketch_receipt(
        &mut self,
        claim_id: &str,
        current_head: &str,
        receipt: &VerifiedReceipt,
        comparison: &ComparedSpec,
        probe_observation_id: &str,
        change: Change<'_>,
    ) -> Result<ClaimState> {
        let retained = self
            .oracle_observation(probe_observation_id)?
            .ok_or(TrustTransitionError::ObservationNotFound)?;
        if retained.observation.get("outcome").and_then(Value::as_str)
            != Some("no_counterexample_within_bounds")
        {
            return Err(TrustTransitionError::UnsupportedOutcome.into());
        }
        let spec = self
            .lean_spec(claim_id)?
            .ok_or(TrustTransitionError::SketchEvidenceMismatch)?;
        let sketch_id = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&(
                "ai4math:formal-sketch:v1",
                claim_id,
                current_head,
                &receipt.receipt_sha256,
                &comparison.receipt_sha256,
                probe_observation_id,
            ))?)
        );
        self.write(|tx| {
            let claim = require_claim(tx, claim_id)?;
            if claim.stale
                || !matches!(claim.local_trust, Trust::L0 | Trust::L1 | Trust::L3)
            {
                return Err(TrustTransitionError::SketchEvidenceMismatch.into());
            }
            super::validate_target(&claim, receipt)?;
            if receipt.tier != "T3_TAINT_REBUILD"
                || !receipt.broker_assessed_t3
                || !receipt.axioms_used.iter().any(|a| a == "sorryAx")
                || receipt.base_commit != current_head
                || comparison.candidate_receipt_sha256 != receipt.receipt_sha256
                || comparison.spec_file != spec.spec_file
                || comparison.spec_decl != spec.spec_decl
                || comparison.spec_prefix_sha256 != spec.spec_prefix_sha256
            {
                return Err(TrustTransitionError::SketchEvidenceMismatch.into());
            }
            let (engine, formal_target, informal_statement): (String, Option<String>, String) = tx.query_row(
                "SELECT engine,formal_target,informal_statement FROM claims WHERE claim_id=?1",
                [claim_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?;
            let repo: Option<String> = tx.query_row("SELECT repo FROM managed_repo WHERE singleton=1", [], |r| r.get(0)).optional()?;
            let approval = &retained.approval;
            if engine != "lean4"
                || approval.claim_id != claim_id
                || approval.commit != current_head
                || approval.repo != repo.unwrap_or_default()
                || approval.engine != engine
                || approval.formal_target != formal_target.unwrap_or_default()
                || approval.informal_statement != informal_statement
                || spec.repo != approval.repo
            {
                return Err(TrustTransitionError::SketchEvidenceMismatch.into());
            }
            let replayed: Option<i64> = tx.query_row(
                "SELECT event_seq FROM formal_sketch_evidence WHERE sketch_id=?1",
                [&sketch_id],
                |r| r.get(0),
            ).optional()?;
            if replayed.is_some() {
                return Ok(claim);
            }
            let receipt_exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM verification_receipts WHERE receipt_sha256=?1)",
                [&receipt.receipt_sha256],
                |r| r.get(0),
            )?;
            if !receipt_exists {
                persist_receipt(tx, &claim, receipt, change)?;
            }
            let seq = event(
                tx,
                change,
                "FORMAL_SKETCH_EVIDENCE_RECORDED",
                "formal_sketch",
                &sketch_id,
                json!({
                    "claim_id":claim_id,
                    "candidate_receipt_sha256":receipt.receipt_sha256,
                    "comparison_receipt_sha256":comparison.receipt_sha256,
                    "probe_observation_id":probe_observation_id,
                }),
            )?;
            tx.execute(
                "INSERT INTO formal_sketch_evidence(sketch_id,claim_id,git_commit,module_path,target_decl,
                    candidate_receipt_sha256,comparison_receipt_sha256,probe_observation_id,proof_body_sha256,event_seq)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
                rusqlite::params![
                    sketch_id, claim_id, current_head, receipt.file_path, receipt.target_decl,
                    receipt.receipt_sha256, comparison.receipt_sha256, probe_observation_id,
                    receipt.patch_sha256, seq,
                ],
            )?;
            update_trust(
                tx,
                &claim,
                Trust::L3,
                None,
                change,
                json!({
                    "transition":"formal_sketch_evidence",
                    "sketch_id":sketch_id,
                    "candidate_receipt_sha256":receipt.receipt_sha256,
                    "comparison_receipt_sha256":comparison.receipt_sha256,
                    "probe_observation_id":probe_observation_id,
                    "proof_verified":false,
                }),
            )?;
            event(
                tx,
                change,
                "CLAIM_FORMAL_SKETCH_TRUST_APPLIED",
                "claim",
                claim_id,
                json!({"sketch_id":sketch_id,"target":"L3","proof_verified":false}),
            )?;
            recompute(tx, change)?;
            require_claim(tx, claim_id)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::reviewed_lean::ComparedSpec;
    use crate::{NewClaim, ReviewedLeanSpec, VerifiedReceipt};
    use ai4math_evidence::{OracleObservationEvidence, OracleOutcome};
    use anyhow::Context;
    use sha2::{Digest, Sha256};

    const REPO: &str = "/managed/trust";
    const HEAD: &str = "0123456789abcdef0123456789abcdef01234567";
    const QUERY: &str = "1111111111111111111111111111111111111111111111111111111111111111";
    const Z3: &str = "2222222222222222222222222222222222222222222222222222222222222222";
    const CVC5: &str = "3333333333333333333333333333333333333333333333333333333333333333";

    struct Evidence {
        raw: Value,
        kind: OracleOutcome,
        id: String,
    }
    impl Evidence {
        fn new(outcome: &'static str, kind: OracleOutcome) -> Result<Self> {
            let mut raw = json!({"outcome":outcome});
            let id = format!("{:x}", Sha256::digest(serde_json::to_vec(&raw)?));
            raw["observation_sha256"] = json!(id);
            Ok(Self { raw, kind, id })
        }
    }
    unsafe impl OracleObservationEvidence for Evidence {
        fn id(&self) -> &str {
            &self.id
        }
        fn query_sha256(&self) -> &str {
            QUERY
        }
        fn solver_sha256s(&self) -> [&str; 2] {
            [Z3, CVC5]
        }
        fn outcome(&self) -> OracleOutcome {
            self.kind
        }
        fn raw_json(&self) -> Result<Value> {
            Ok(self.raw.clone())
        }
    }

    fn fixture(outcome: &'static str, kind: OracleOutcome) -> Result<(StateStore, String)> {
        let mut store = StateStore::open(":memory:")?;
        store.bind_task_repository(REPO, HEAD)?;
        store.create_claim(
            NewClaim {
                id: "C1",
                title: "Claim",
                informal_statement: "Statement",
                engine: "lean4",
                formal_target: Some("Demo.lean#demo"),
                module_path: Some("Demo.lean"),
                critical: false,
            },
            Change {
                actor: "test",
                git_commit: None,
            },
        )?;
        let approval = store.oracle_approval(REPO, "C1", HEAD, QUERY, [Z3, CVC5])?;
        let retained =
            store.record_oracle_observation(&approval, &Evidence::new(outcome, kind)?)?;
        Ok((store, retained.id))
    }

    #[test]
    fn bounded_oracle_outcomes_map_only_to_l1_or_l2a() -> Result<()> {
        let change = Change {
            actor: "agent:chief",
            git_commit: None,
        };
        let (mut store, id) = fixture(
            "no_counterexample_within_bounds",
            OracleOutcome::NoCounterexampleWithinBounds,
        )?;
        let claim = store.apply_oracle_trust_transition("C1", &id, HEAD, change)?;
        assert_eq!(claim.local_trust, Trust::L1);
        let before = store.status()?.last_event_seq;
        let replay = store.apply_oracle_trust_transition("C1", &id, HEAD, change)?;
        assert_eq!(replay.local_trust, claim.local_trust);
        assert_eq!(replay.effective_trust, claim.effective_trust);
        assert_eq!(store.status()?.last_event_seq, before);

        let (mut store, id) = fixture("counterexample", OracleOutcome::Counterexample)?;
        let claim = store.apply_oracle_trust_transition("C1", &id, HEAD, change)?;
        assert_eq!(claim.local_trust, Trust::L2a);
        assert!(
            store
                .apply_oracle_trust_transition("C1", &id, &"f".repeat(40), change)
                .is_err()
        );
        Ok(())
    }

    fn sketch_fixture() -> Result<(StateStore, String, VerifiedReceipt, ComparedSpec)> {
        let (mut store, observation_id) = fixture(
            "no_counterexample_within_bounds",
            OracleOutcome::NoCounterexampleWithinBounds,
        )?;
        store.bind_lean_spec(&ReviewedLeanSpec {
            repo: REPO.into(),
            claim_id: "C1".into(),
            approved_git_commit: HEAD.into(),
            spec_file: "Spec.lean".into(),
            spec_decl: "spec".into(),
            spec_prefix_sha256: "d".repeat(64),
        })?;
        let receipt_sha256 = "a".repeat(64);
        let receipt = VerifiedReceipt {
            id: receipt_sha256.clone(),
            issued_at: "2026-01-01T00:00:00Z".into(),
            tier: "T3_TAINT_REBUILD".into(),
            engine: "lean4".into(),
            base_commit: HEAD.into(),
            patch_sha256: "b".repeat(64),
            target_decl: "demo".into(),
            file_path: "Demo.lean".into(),
            allowlist_passed: true,
            axioms_used: vec!["sorryAx".into()],
            external_checker_passed: None,
            compile_seconds: 1.0,
            max_heartbeats: None,
            receipt_sha256: receipt_sha256.clone(),
            raw: json!({"file_path":"Demo.lean","target_decl":"demo"}),
            regression_checked: false,
            broker_assessed_t3: true,
            lean_comparison: Some(ComparedSpec {
                spec_file: "Spec.lean".into(),
                spec_decl: "spec".into(),
                spec_prefix_sha256: "d".repeat(64),
                candidate_receipt_sha256: receipt_sha256,
                receipt_sha256: "c".repeat(64),
            }),
        };
        let comparison = ComparedSpec {
            spec_file: "Spec.lean".into(),
            spec_decl: "spec".into(),
            spec_prefix_sha256: "d".repeat(64),
            candidate_receipt_sha256: receipt.receipt_sha256.clone(),
            receipt_sha256: "c".repeat(64),
        };
        Ok((store, observation_id, receipt, comparison))
    }

    #[test]
    fn formal_sketch_requires_t3_comparison_and_probe_and_is_idempotent() -> Result<()> {
        let (mut store, observation_id, receipt, comparison) = sketch_fixture()?;
        let change = Change {
            actor: "agent:builder",
            git_commit: Some(HEAD),
        };
        let claim = store.apply_formal_sketch_receipt(
            "C1",
            HEAD,
            &receipt,
            &comparison,
            &observation_id,
            change,
        )?;
        assert_eq!(claim.local_trust, Trust::L3);
        let before = store.status()?.last_event_seq;
        let replay = store.apply_formal_sketch_receipt(
            "C1",
            HEAD,
            &receipt,
            &comparison,
            &observation_id,
            change,
        )?;
        assert_eq!(replay.local_trust, Trust::L3);
        assert_eq!(store.status()?.last_event_seq, before);
        assert!(
            store
                .apply_formal_sketch_receipt(
                    "C1",
                    HEAD,
                    &receipt,
                    &comparison,
                    "f".repeat(64).as_str(),
                    change,
                )
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn formal_sketch_rejects_mismatched_spec_or_non_t3_receipt() -> Result<()> {
        let (mut store, observation_id, receipt, mut comparison) = sketch_fixture()?;
        let change = Change {
            actor: "agent:builder",
            git_commit: Some(HEAD),
        };
        comparison.spec_file = "Other.lean".into();
        assert!(
            store
                .apply_formal_sketch_receipt(
                    "C1",
                    HEAD,
                    &receipt,
                    &comparison,
                    &observation_id,
                    change,
                )
                .is_err()
        );
        assert_eq!(store.claim("C1")?.context("claim")?.local_trust, Trust::L0);
        let mut receipt = receipt;
        receipt.tier = "T1_PROMOTION".into();
        assert!(
            store
                .apply_formal_sketch_receipt(
                    "C1",
                    HEAD,
                    &receipt,
                    &comparison,
                    &observation_id,
                    change,
                )
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn unavailable_or_disagreeing_oracles_do_not_change_trust() -> Result<()> {
        let change = Change {
            actor: "agent:chief",
            git_commit: None,
        };
        for (outcome, kind) in [
            ("unavailable", OracleOutcome::Unavailable),
            ("disagreement", OracleOutcome::Disagreement),
        ] {
            let (mut store, id) = fixture(outcome, kind)?;
            let before = store.status()?.last_event_seq;
            assert!(
                store
                    .apply_oracle_trust_transition("C1", &id, HEAD, change)
                    .is_err()
            );
            assert_eq!(store.claim("C1")?.context("claim")?.local_trust, Trust::L0);
            assert_eq!(store.status()?.last_event_seq, before);
        }
        Ok(())
    }
}
