//! Redacted promotion control-plane projections.
//!
//! These reads never return proof bodies, repository paths, receipts, or
//! authorization. They describe what an operator could review; they do not
//! prepare, publish, recover, or promote anything.

use super::{Change, ClaimState, StateStore};
use anyhow::{Context, Result, ensure};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PromotionIntentSummary {
    pub claim_id: String,
    pub base_commit: String,
    pub new_commit: String,
    pub file_path: String,
    pub target_decl: String,
    pub proof_body_sha256: String,
}

#[derive(Debug, Serialize)]
pub struct PromotionClaimSummary {
    pub stale: bool,
    pub effective_stale: bool,
    pub local_trust: String,
    pub effective_trust: String,
}

#[derive(Debug, Serialize)]
pub struct RecordedPromotionSummary {
    pub claim_id: String,
    pub base_commit: String,
    pub promoted_commit: String,
    pub file_path: Option<String>,
    pub target_decl: Option<String>,
    pub proof_body_sha256: Option<String>,
    pub source_sha256: String,
    pub event_seq: i64,
    pub recipe_retained: bool,
    pub on_main: Option<bool>,
}

#[derive(Debug, Serialize)]
pub struct PromotionPreview {
    pub schema: &'static str,
    pub advisory_only: bool,
    pub proof_verified: bool,
    pub authorization_granted: bool,
    pub task_id: String,
    pub claim_id: String,
    pub engine: String,
    pub module_path: String,
    pub target: String,
    pub base_commit: String,
    pub current_head: String,
    pub ledger_event_seq: i64,
    pub candidate_sha256: String,
    pub preview_sha256: String,
    pub proof_body_bytes: usize,
    pub head_matches_base: bool,
    pub claim: PromotionClaimSummary,
    pub pending_promotion: bool,
}

#[derive(Debug, Serialize)]
pub struct PromotionRecoverySummary {
    pub required: bool,
    pub kind: &'static str,
    pub operator_action: &'static str,
    pub automatic: bool,
}

impl PromotionRecoverySummary {
    pub fn none() -> Self {
        Self {
            required: false,
            kind: "none",
            operator_action: "none",
            automatic: false,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct PromotionStatus {
    pub schema: &'static str,
    pub authorization_granted: bool,
    pub repo_bound: bool,
    pub current_head: String,
    pub ledger_event_seq: i64,
    pub state: &'static str,
    pub pending_intent: Option<PromotionIntentSummary>,
    pub latest_recorded: Option<RecordedPromotionSummary>,
    pub recovery: PromotionRecoverySummary,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum CandidateProof {
    ProofBody { proof_body: String },
}

fn sha(value: &Value) -> Result<String> {
    Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(value)?)))
}

fn intent_summary(intent: &super::PromotionIntent) -> PromotionIntentSummary {
    PromotionIntentSummary {
        claim_id: intent.claim_id.clone(),
        base_commit: intent.base_commit.clone(),
        new_commit: intent.new_commit.clone(),
        file_path: intent.file_path.clone(),
        target_decl: intent.target_decl.clone(),
        proof_body_sha256: format!("{:x}", Sha256::digest(intent.proof_body.as_bytes())),
    }
}

fn claim_summary(claim: &ClaimState) -> PromotionClaimSummary {
    PromotionClaimSummary {
        stale: claim.stale,
        effective_stale: claim.effective_stale,
        local_trust: claim.local_trust.as_str().to_owned(),
        effective_trust: claim.effective_trust.as_str().to_owned(),
    }
}

fn latest_recorded(store: &StateStore) -> Result<Option<RecordedPromotionSummary>> {
    Ok(store
        .db
        .query_row(
            "SELECT p.claim_id,p.base_commit,p.git_commit,m.file_path,m.target_decl,m.proof_body,
                    r.patch_sha256,p.event_seq
             FROM recorded_promotions p
             LEFT JOIN managed_proof_recipes m USING(git_commit)
             JOIN verification_receipts r ON r.receipt_id=p.receipt_id
             ORDER BY p.event_seq DESC LIMIT 1",
            [],
            |r| {
                let file_path: Option<String> = r.get(3)?;
                let target_decl: Option<String> = r.get(4)?;
                let proof_body: Option<String> = r.get(5)?;
                Ok(RecordedPromotionSummary {
                    claim_id: r.get(0)?,
                    base_commit: r.get(1)?,
                    promoted_commit: r.get(2)?,
                    recipe_retained: proof_body.is_some(),
                    proof_body_sha256: proof_body
                        .map(|body| format!("{:x}", Sha256::digest(body.as_bytes()))),
                    file_path,
                    target_decl,
                    source_sha256: r.get(6)?,
                    event_seq: r.get(7)?,
                    on_main: None,
                })
            },
        )
        .optional()?)
}

impl StateStore {
    pub fn promotion_preview(&self, task_id: &str, current_head: &str) -> Result<PromotionPreview> {
        let tx = self.db.unchecked_transaction()?;
        let candidate = self
            .task_candidate(task_id)?
            .context("no retained suggestion for this task")?;
        ensure!(
            candidate.role == "prover_worker",
            "only retained proof-body suggestions have a promotion preview"
        );
        let CandidateProof::ProofBody { proof_body } =
            serde_json::from_str(&candidate.result_json)?;
        ensure!(
            !proof_body.trim().is_empty() && proof_body.len() <= 8192 && !proof_body.contains('\0'),
            "invalid proof-body size or content"
        );
        let digest = sha(&json!({
            "schema":"ai4math-promotion-candidate-v1",
            "task_id":candidate.task_id,
            "claim_id":candidate.claim_id,
            "engine":candidate.engine,
            "module_path":candidate.module_path,
            "target":candidate.target,
            "base_commit":candidate.base_commit,
            "proof_body":proof_body,
        }))?;
        let claim = self
            .claim(&candidate.claim_id)?
            .context("candidate claim disappeared")?;
        let pending = self.promotion_intent()?.is_some();
        let ledger_event_seq = super::last_seq(&self.db)?;
        let repo = self.managed_repo()?;
        let preview_digest = sha(&json!({
            "schema":"ai4math-promotion-preview-v1",
            "candidate_sha256":digest,
            "current_head":current_head,
            "ledger_event_seq":ledger_event_seq,
            "repo":repo,
        }))?;
        let head_matches_base = current_head == candidate.base_commit;
        let preview = PromotionPreview {
            schema: "ai4math-promotion-preview-v1",
            advisory_only: true,
            proof_verified: false,
            authorization_granted: false,
            task_id: candidate.task_id,
            claim_id: candidate.claim_id,
            engine: candidate.engine,
            module_path: candidate.module_path,
            target: candidate.target,
            base_commit: candidate.base_commit,
            current_head: current_head.to_owned(),
            ledger_event_seq,
            candidate_sha256: digest,
            preview_sha256: preview_digest,
            proof_body_bytes: proof_body.len(),
            head_matches_base,
            claim: claim_summary(&claim),
            pending_promotion: pending,
        };
        tx.commit()?;
        Ok(preview)
    }

    /// Atomically consume one Owner approval request. Exact replay returns false;
    /// reuse for another message, candidate or task is refused.
    pub fn consume_promotion_approval(
        &mut self,
        request_id: &str,
        message_sha256: &str,
        candidate_sha256: &str,
        task_id: &str,
        consumed_unix: u64,
        change: Change<'_>,
    ) -> Result<bool> {
        ensure!(
            !request_id.is_empty()
                && request_id.len() <= 128
                && request_id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-')),
            "invalid promotion approval request ID"
        );
        ensure!(
            message_sha256.len() == 64 && candidate_sha256.len() == 64,
            "invalid promotion approval digest"
        );
        self.write(|tx| {
            let existing: Option<(String, String, String)> = tx
                .query_row(
                    "SELECT message_sha256,candidate_sha256,task_id FROM promotion_approval_requests WHERE request_id=?1",
                    [request_id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?;
            if let Some((message, candidate, task)) = existing {
                ensure!(
                    message == message_sha256 && candidate == candidate_sha256 && task == task_id,
                    "promotion approval request ID already identifies different input"
                );
                return Ok(false);
            }
            let seq = super::event(
                tx,
                change,
                "PROMOTION_APPROVAL_CONSUMED",
                "promotion_approval",
                request_id,
                json!({"message_sha256":message_sha256,"candidate_sha256":candidate_sha256,"task_id":task_id}),
            )?;
            tx.execute(
                "INSERT INTO promotion_approval_requests(request_id,message_sha256,candidate_sha256,task_id,consumed_unix,event_seq)
                 VALUES(?1,?2,?3,?4,?5,?6)",
                params![request_id, message_sha256, candidate_sha256, task_id, consumed_unix, seq],
            )?;
            Ok(true)
        })
    }

    pub fn promotion_status(&self, current_head: &str) -> Result<PromotionStatus> {
        let tx = self.db.unchecked_transaction()?;
        let pending_intent = self
            .promotion_intent()?
            .map(|intent| intent_summary(&intent));
        let latest_recorded = latest_recorded(self)?;
        let state = if pending_intent.is_some() {
            "pending"
        } else if latest_recorded.is_some() {
            "recorded"
        } else {
            "idle"
        };
        let status = PromotionStatus {
            schema: "ai4math-promotion-status-v1",
            authorization_granted: false,
            repo_bound: self.managed_repo()?.is_some(),
            current_head: current_head.to_owned(),
            ledger_event_seq: super::last_seq(&self.db)?,
            state,
            pending_intent,
            latest_recorded,
            recovery: PromotionRecoverySummary::none(),
        };
        tx.commit()?;
        Ok(status)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Change, NewClaim, NewTask, TaskTrack};

    fn fixture() -> Result<(StateStore, String)> {
        let mut store = StateStore::open(":memory:")?;
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
        store.register_task(
            NewTask {
                id: "T1",
                track: TaskTrack::Infrastructure,
                engine: "lean4",
                module_path: "Demo.lean",
                target: "demo",
                dependencies: &[],
            },
            Change {
                actor: "test",
                git_commit: None,
            },
        )?;
        let lease = store
            .claim_task(
                Some(TaskTrack::Infrastructure),
                std::time::Duration::from_secs(60),
                Change {
                    actor: "test",
                    git_commit: None,
                },
            )?
            .context("lease")?;
        store.finish_task_run(
            &lease,
            crate::TaskRunOutcome::ModelCandidate {
                base_commit: &"a".repeat(40),
                claim_id: "C1",
                role: "prover_worker",
                result_json: r#"{"kind":"proof_body","proof_body":"by rfl"}"#,
            },
            Change {
                actor: "test",
                git_commit: None,
            },
        )?;
        Ok((store, "a".repeat(40)))
    }

    #[test]
    fn owner_approval_consumption_is_exactly_idempotent() -> Result<()> {
        let (mut store, _head) = fixture()?;
        let change = Change {
            actor: "owner",
            git_commit: None,
        };
        let message = "a".repeat(64);
        let candidate = "b".repeat(64);
        assert!(store.consume_promotion_approval("REQ-1", &message, &candidate, "T1", 1, change)?);
        assert!(!store.consume_promotion_approval("REQ-1", &message, &candidate, "T1", 1, change)?);
        assert!(
            store
                .consume_promotion_approval("REQ-1", &"c".repeat(64), &candidate, "T1", 1, change)
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn preview_rejects_noncanonical_or_oversized_candidate_envelopes() -> Result<()> {
        let (store, head) = fixture()?;
        store.db.execute(
            "UPDATE scheduler_model_task_candidates SET result_json=?1",
            [r#"{"kind":"proof_body","proof_body":"by rfl","repo":"secret"}"#],
        )?;
        assert!(store.promotion_preview("T1", &head).is_err());
        let oversized = format!(
            "{{\"kind\":\"proof_body\",\"proof_body\":\"{}\"}}",
            "x".repeat(8193)
        );
        store.db.execute(
            "UPDATE scheduler_model_task_candidates SET result_json=?1",
            [oversized],
        )?;
        assert!(store.promotion_preview("T1", &head).is_err());
        Ok(())
    }

    #[test]
    fn preview_hashes_but_never_returns_the_proof_body() -> Result<()> {
        let (store, head) = fixture()?;
        let preview = store.promotion_preview("T1", &head)?;
        assert!(!preview.proof_verified);
        assert!(preview.head_matches_base);
        assert_eq!(preview.proof_body_bytes, 6);
        let raw = serde_json::to_string(&preview)?;
        assert!(!raw.contains("by rfl"));
        assert_eq!(store.promotion_status(&head)?.pending_intent, None);
        Ok(())
    }
}
