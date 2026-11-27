//! Typed, evidence-gated Claim trust transitions.
//!
//! Raw trust assignment is intentionally not exposed to the control plane.
//! These transitions derive the target level from retained evidence and bind it
//! to the live Claim and managed Git context.

use super::{Change, StateStore, Trust, event, recompute, require_claim, update_trust};
use anyhow::Result;
use rusqlite::OptionalExtension;
use serde_json::{Value, json};

#[derive(Debug)]
pub enum TrustTransitionError {
    ObservationNotFound,
    ContextMismatch,
    UnsupportedOutcome,
}

impl std::fmt::Display for TrustTransitionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::ObservationNotFound => "retained Oracle observation not found",
            Self::ContextMismatch => "Oracle observation does not match the live Claim context",
            Self::UnsupportedOutcome => "Oracle outcome does not authorize a trust transition",
        })
    }
}
impl std::error::Error for TrustTransitionError {}

impl StateStore {
    /// Apply the only trust transitions currently justified by retained bounded
    /// Oracle evidence. L3/L4a/L4b/L5 require their own typed evidence paths and
    /// cannot be requested here.
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NewClaim;
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
