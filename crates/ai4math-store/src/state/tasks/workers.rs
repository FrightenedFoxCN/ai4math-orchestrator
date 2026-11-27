//! Durable RPC worker attempts layered on the ordinary scheduler.
//!
//! Acquire atomically consumes one scheduler lease and creates one request-ID
//! bound attempt. Submit/cancel consume the private database fence in the same
//! transaction as the terminal attempt state. Agent-facing packets never carry
//! the fence, repository path, credentials, or candidate readback.

use super::runs::finish_model_task_run_in_tx;
use super::{
    Change, IdempotencyConflict, StateStore, TaskFence, TaskLease, TaskTrack, claim_task_in_tx,
    event, now_ms, release_lease,
};
use anyhow::{Context, Result, ensure};
use rusqlite::{OptionalExtension, Transaction, params};
use serde::Serialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::time::Duration;

const RESULT_LIMIT: usize = 32 * 1024;

#[derive(Debug)]
pub enum WorkerAttemptError {
    NotFound,
    Forbidden,
    Terminal,
}

impl std::fmt::Display for WorkerAttemptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::NotFound => "worker attempt not found",
            Self::Forbidden => "worker attempt belongs to another principal",
            Self::Terminal => "worker attempt is not running",
        })
    }
}
impl std::error::Error for WorkerAttemptError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum WorkerAttemptStatus {
    Running,
    Submitted,
    Cancelled,
    Expired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WorkerAttempt {
    pub attempt_id: String,
    pub request_id: String,
    pub task_id: String,
    pub claim_id: String,
    pub engine: String,
    pub module_path: String,
    pub target: String,
    pub role: String,
    pub base_commit: String,
    pub expires_at_ms: i64,
    pub status: WorkerAttemptStatus,
}

pub enum WorkerSuggestion<'a> {
    ProofBody(&'a str),
    Review {
        verdict: &'a str,
        rationale: &'a str,
    },
}

#[derive(Debug)]
struct AttemptRow {
    attempt: WorkerAttempt,
    principal: String,
    wall_seconds: i64,
    request_sha256: String,
    suggestion_sha256: Option<String>,
    lease_fence: i64,
    stored_status: String,
}

fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b':'))
}

fn valid_commit(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && value.bytes().any(|b| b != b'0')
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn role_for(principal: &str) -> Result<&'static str> {
    let (role, binding) = principal
        .split_once(':')
        .map(|(role, binding)| (role, Some(binding)))
        .unwrap_or((principal, None));
    if binding.is_some_and(|id| id.is_empty() || id.len() > 128) {
        anyhow::bail!("invalid worker principal binding");
    }
    match role {
        "prover" => Ok("prover_worker"),
        "reviewer" => Ok("independent_reviewer"),
        _ => anyhow::bail!("principal cannot acquire ordinary worker attempts"),
    }
}

fn request_hash(
    request_id: &str,
    principal: &str,
    role: &str,
    base_commit: &str,
    wall_seconds: i64,
) -> String {
    format!(
        "{:x}",
        Sha256::digest(
            format!("{request_id}\0{principal}\0{role}\0{base_commit}\0{wall_seconds}").as_bytes(),
        )
    )
}

fn attempt_id(request_id: &str, principal: &str) -> String {
    format!(
        "wa-{:x}",
        Sha256::digest(format!("{principal}\0{request_id}").as_bytes())
    )
}

fn read_row(tx: &Transaction<'_>, attempt_id: &str) -> Result<Option<AttemptRow>> {
    Ok(tx
        .query_row(
            "SELECT a.attempt_id,a.request_id,a.task_id,a.claim_id,t.engine,t.module_path,t.target,
                    a.role,a.base_commit,a.expires_at_ms,a.status,a.principal,a.wall_seconds,
                    a.request_sha256,a.suggestion_sha256,a.lease_fence
             FROM worker_attempts a JOIN scheduler_tasks t ON t.queue_seq=a.task_seq
             WHERE a.attempt_id=?1",
            [attempt_id],
            |r| {
                let stored_status: String = r.get(10)?;
                let status = if stored_status == "RUNNING"
                    && r.get::<_, i64>(9)?
                        <= now_ms(tx).map_err(|_| rusqlite::Error::InvalidQuery)?
                {
                    WorkerAttemptStatus::Expired
                } else {
                    match stored_status.as_str() {
                        "RUNNING" => WorkerAttemptStatus::Running,
                        "SUBMITTED" => WorkerAttemptStatus::Submitted,
                        "CANCELLED" => WorkerAttemptStatus::Cancelled,
                        _ => return Err(rusqlite::Error::InvalidQuery),
                    }
                };
                Ok(AttemptRow {
                    attempt: WorkerAttempt {
                        attempt_id: r.get(0)?,
                        request_id: r.get(1)?,
                        task_id: r.get(2)?,
                        claim_id: r.get(3)?,
                        engine: r.get(4)?,
                        module_path: r.get(5)?,
                        target: r.get(6)?,
                        role: r.get(7)?,
                        base_commit: r.get(8)?,
                        expires_at_ms: r.get(9)?,
                        status,
                    },
                    principal: r.get(11)?,
                    wall_seconds: r.get(12)?,
                    request_sha256: r.get(13)?,
                    suggestion_sha256: r.get(14)?,
                    lease_fence: r.get(15)?,
                    stored_status,
                })
            },
        )
        .optional()?)
}

fn read_request(tx: &Transaction<'_>, request_id: &str) -> Result<Option<AttemptRow>> {
    let id: Option<String> = tx
        .query_row(
            "SELECT attempt_id FROM worker_attempts WHERE request_id=?1",
            [request_id],
            |r| r.get(0),
        )
        .optional()?;
    match id {
        Some(id) => read_row(tx, &id),
        None => Ok(None),
    }
}

fn lease_for(
    tx: &Transaction<'_>,
    task_id: &str,
    expected_expires_at_ms: i64,
    expected_fence: i64,
) -> Result<Option<TaskLease>> {
    let now = now_ms(tx)?;
    Ok(tx
        .query_row(
            "SELECT t.track,t.engine,t.module_path,t.target,t.last_fence,t.lease_expires_at_ms,
                    c.scope_id
             FROM scheduler_tasks t JOIN task_scheduler_control c ON c.singleton=1
             WHERE t.task_id=?1 AND t.lease_expires_at_ms=?3
               AND t.last_fence=?4 AND t.lease_expires_at_ms>?2",
            params![task_id, now, expected_expires_at_ms, expected_fence],
            |r| {
                Ok(TaskLease {
                    id: task_id.to_owned(),
                    track: r
                        .get::<_, String>(0)?
                        .parse()
                        .map_err(|_| rusqlite::Error::InvalidQuery)?,
                    engine: r.get(1)?,
                    module_path: r.get(2)?,
                    target: r.get(3)?,
                    expires_at_ms: r.get(5)?,
                    fence: TaskFence {
                        scope_id: r.get(6)?,
                        generation: r.get(4)?,
                    },
                })
            },
        )
        .optional()?)
}

fn claim_for_task(tx: &Transaction<'_>, task_id: &str) -> Result<Option<(i64, String)>> {
    let mut stmt = tx.prepare(
        "SELECT t.queue_seq,c.claim_id
         FROM scheduler_tasks t JOIN claims c
           ON c.engine=t.engine AND c.module_path=t.module_path
          AND c.formal_target=(t.module_path || '#' || t.target)
         WHERE t.task_id=?1 AND c.stale=0 AND c.effective_stale=0
           AND c.effective_trust_level NOT IN ('L2b','L3')
         ORDER BY c.claim_id LIMIT 2",
    )?;
    let rows: Vec<(i64, String)> = stmt
        .query_map([task_id], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    match rows.as_slice() {
        [] => Ok(None),
        [(seq, id)] => Ok(Some((*seq, id.clone()))),
        _ => anyhow::bail!("task target has multiple runnable claims"),
    }
}

fn suggestion_json(role: &str, suggestion: WorkerSuggestion<'_>) -> Result<(String, String)> {
    let value = match (role, suggestion) {
        ("prover_worker", WorkerSuggestion::ProofBody(body)) => {
            ensure!(
                !body.trim().is_empty() && body.len() <= 8192 && !body.contains('\0'),
                "invalid proof body"
            );
            json!({"kind":"proof_body","proof_body":body})
        }
        ("independent_reviewer", WorkerSuggestion::Review { verdict, rationale }) => {
            ensure!(
                matches!(
                    verdict,
                    "looks_consistent" | "needs_changes" | "insufficient_context"
                ),
                "invalid review verdict"
            );
            ensure!(
                !rationale.trim().is_empty()
                    && rationale.len() <= 8192
                    && !rationale.contains('\0'),
                "invalid review rationale"
            );
            json!({"kind":"review","verdict":verdict,"rationale":rationale})
        }
        _ => anyhow::bail!("suggestion does not match worker role"),
    };
    let raw = serde_json::to_string(&value)?;
    ensure!(
        raw.len() <= RESULT_LIMIT,
        "worker suggestion exceeds 32 KiB"
    );
    let digest = format!("{:x}", Sha256::digest(raw.as_bytes()));
    Ok((raw, digest))
}

impl StateStore {
    pub fn acquire_worker_attempt(
        &mut self,
        request_id: &str,
        principal: &str,
        base_commit: &str,
        wall_seconds: i64,
        change: Change<'_>,
    ) -> Result<Option<WorkerAttempt>> {
        ensure!(valid_id(request_id), "invalid worker request ID");
        ensure!(valid_commit(base_commit), "invalid worker base commit");
        ensure!(
            (10..=600).contains(&wall_seconds),
            "worker wall seconds must be in 10..=600"
        );
        let role = role_for(principal)?;
        let hash = request_hash(request_id, principal, role, base_commit, wall_seconds);
        self.write(|tx| {
            if let Some(existing) = read_request(tx, request_id)? {
                if existing.principal != principal
                    || existing.attempt.role != role
                    || existing.wall_seconds != wall_seconds
                    || existing.request_sha256 != hash
                {
                    return Err(IdempotencyConflict.into());
                }
                return Ok(Some(existing.attempt));
            }
            let lease = claim_task_in_tx(
                tx,
                Some(TaskTrack::Infrastructure),
                None,
                true,
                Duration::from_secs(wall_seconds as u64),
                change,
            )?;
            let Some(lease) = lease else {
                return Ok(None);
            };
            let Some((task_seq, claim_id)) = claim_for_task(tx, &lease.id)? else {
                release_lease(tx, &lease, false)?;
                return Ok(None);
            };
            let id = attempt_id(request_id, principal);
            let now = now_ms(tx)?;
            tx.execute(
                "INSERT INTO worker_attempts
                 (attempt_id,request_id,request_sha256,principal,role,task_seq,task_id,claim_id,
                  base_commit,wall_seconds,expires_at_ms,lease_fence,status,created_at_ms,updated_at_ms)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,'RUNNING',?13,?13)",
                params![
                    id,
                    request_id,
                    hash,
                    principal,
                    role,
                    task_seq,
                    lease.id,
                    claim_id,
                    base_commit,
                    wall_seconds,
                    lease.expires_at_ms,
                    lease.fence.generation,
                    now
                ],
            )?;
            event(
                tx,
                change,
                "WORKER_ATTEMPT_ACQUIRED",
                "worker_attempt",
                &id,
                json!({"task_id":lease.id,"claim_id":claim_id,"role":role}),
            )?;
            read_row(tx, &id)?
                .map(|row| Some(row.attempt))
                .context("acquired worker attempt disappeared")
        })
    }

    pub fn submit_worker_attempt(
        &mut self,
        attempt_id: &str,
        principal: &str,
        suggestion: WorkerSuggestion<'_>,
        change: Change<'_>,
    ) -> Result<WorkerAttempt> {
        let role = role_for(principal)?;
        let (result_json, digest) = suggestion_json(role, suggestion)?;
        self.write(|tx| {
            let row = read_row(tx, attempt_id)?.ok_or(WorkerAttemptError::NotFound)?;
            if row.principal != principal {
                return Err(WorkerAttemptError::Forbidden.into());
            }
            match row.stored_status.as_str() {
                "SUBMITTED" => {
                    if row.suggestion_sha256.as_deref() != Some(digest.as_str()) {
                        return Err(IdempotencyConflict.into());
                    }
                    return Ok(row.attempt);
                }
                "CANCELLED" => return Err(WorkerAttemptError::Terminal.into()),
                "RUNNING" => {}
                _ => return Err(WorkerAttemptError::Terminal.into()),
            }
            if row.attempt.status != WorkerAttemptStatus::Running {
                return Err(WorkerAttemptError::Terminal.into());
            }
            let lease = lease_for(
                tx,
                &row.attempt.task_id,
                row.attempt.expires_at_ms,
                row.lease_fence,
            )?
            .ok_or(WorkerAttemptError::Terminal)?;
            finish_model_task_run_in_tx(
                tx,
                &lease,
                &row.attempt.base_commit,
                &row.attempt.claim_id,
                &row.attempt.role,
                &result_json,
                change,
            )?;
            let seq = event(
                tx,
                change,
                "WORKER_ATTEMPT_SUBMITTED",
                "worker_attempt",
                attempt_id,
                json!({"suggestion_sha256":digest}),
            )?;
            tx.execute(
                "UPDATE worker_attempts SET status='SUBMITTED',suggestion_sha256=?2,result_json=?3,
                    event_seq=?4,updated_at_ms=?5 WHERE attempt_id=?1 AND status='RUNNING'",
                params![attempt_id, digest, result_json, seq, now_ms(tx)?],
            )?;
            read_row(tx, attempt_id)?
                .map(|row| row.attempt)
                .context("submitted worker attempt disappeared")
        })
    }

    pub fn cancel_worker_attempt(
        &mut self,
        attempt_id: &str,
        principal: &str,
        change: Change<'_>,
    ) -> Result<WorkerAttempt> {
        self.write(|tx| {
            let row = read_row(tx, attempt_id)?.ok_or(WorkerAttemptError::NotFound)?;
            if row.principal != principal {
                return Err(WorkerAttemptError::Forbidden.into());
            }
            match row.stored_status.as_str() {
                "CANCELLED" => return Ok(row.attempt),
                "SUBMITTED" => return Err(WorkerAttemptError::Terminal.into()),
                "RUNNING" => {}
                _ => return Err(WorkerAttemptError::Terminal.into()),
            }
            if row.attempt.status != WorkerAttemptStatus::Running {
                return Err(WorkerAttemptError::Terminal.into());
            }
            let lease = lease_for(
                tx,
                &row.attempt.task_id,
                row.attempt.expires_at_ms,
                row.lease_fence,
            )?
            .ok_or(WorkerAttemptError::Terminal)?;
            ensure!(
                release_lease(tx, &lease, false)?.is_some(),
                "worker lease is no longer valid for cancellation"
            );
            let seq = event(
                tx,
                change,
                "WORKER_ATTEMPT_CANCELLED",
                "worker_attempt",
                attempt_id,
                json!({}),
            )?;
            tx.execute(
                "UPDATE worker_attempts SET status='CANCELLED',event_seq=?2,updated_at_ms=?3
                 WHERE attempt_id=?1 AND status='RUNNING'",
                params![attempt_id, seq, now_ms(tx)?],
            )?;
            read_row(tx, attempt_id)?
                .map(|row| row.attempt)
                .context("cancelled worker attempt disappeared")
        })
    }

    pub fn worker_attempt(
        &self,
        attempt_id: &str,
        principal: &str,
    ) -> Result<Option<WorkerAttempt>> {
        let tx = self.db.unchecked_transaction()?;
        let row = read_row(&tx, attempt_id)?;
        tx.commit()?;
        Ok(row.and_then(|row| (row.principal == principal).then_some(row.attempt)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Result<StateStore> {
        let mut store = StateStore::open(":memory:")?;
        store.create_claim(
            crate::NewClaim {
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
            super::super::NewTask {
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
        Ok(store)
    }

    #[test]
    fn acquire_submit_and_replay_are_idempotent() -> Result<()> {
        let mut store = store()?;
        let change = Change {
            actor: "agent:prover",
            git_commit: None,
        };
        let attempt = store
            .acquire_worker_attempt("REQ-1", "prover", &"a".repeat(40), 60, change)?
            .context("attempt")?;
        assert_eq!(attempt.status, WorkerAttemptStatus::Running);
        let replay = store
            .acquire_worker_attempt("REQ-1", "prover", &"a".repeat(40), 60, change)?
            .context("replay")?;
        assert_eq!(attempt, replay);
        assert!(
            store
                .acquire_worker_attempt("REQ-1", "prover", &"c".repeat(40), 60, change)
                .is_err()
        );
        let submitted = store.submit_worker_attempt(
            &attempt.attempt_id,
            "prover",
            WorkerSuggestion::ProofBody("by rfl"),
            change,
        )?;
        assert_eq!(submitted.status, WorkerAttemptStatus::Submitted);
        let replay = store.submit_worker_attempt(
            &attempt.attempt_id,
            "prover",
            WorkerSuggestion::ProofBody("by rfl"),
            change,
        )?;
        assert_eq!(submitted, replay);
        assert!(
            store
                .submit_worker_attempt(
                    &attempt.attempt_id,
                    "prover",
                    WorkerSuggestion::ProofBody("by simp"),
                    change
                )
                .is_err()
        );
        assert!(store.task_candidate("T1")?.is_some());
        Ok(())
    }

    #[test]
    fn expired_attempt_cannot_touch_a_replacement_lease() -> Result<()> {
        let mut store = store()?;
        let change = Change {
            actor: "agent:prover",
            git_commit: None,
        };
        let old = store
            .acquire_worker_attempt("REQ-old", "prover", &"e".repeat(40), 60, change)?
            .context("old attempt")?;
        store.db.execute(
            "UPDATE scheduler_tasks SET lease_expires_at_ms=0 WHERE task_id='T1'",
            [],
        )?;
        let new = store
            .acquire_worker_attempt("REQ-new", "prover", &"e".repeat(40), 60, change)?
            .context("new attempt")?;
        assert!(
            store
                .submit_worker_attempt(
                    &old.attempt_id,
                    "prover",
                    WorkerSuggestion::ProofBody("by rfl"),
                    change
                )
                .is_err()
        );
        assert!(
            store
                .cancel_worker_attempt(&old.attempt_id, "prover", change)
                .is_err()
        );
        assert_eq!(
            store
                .worker_attempt(&new.attempt_id, "prover")?
                .context("new read")?
                .status,
            WorkerAttemptStatus::Running
        );
        assert_eq!(store.task_scheduler_status()?.infrastructure.active, 1);
        Ok(())
    }

    #[test]
    fn review_replay_hashes_the_full_verdict() -> Result<()> {
        let mut store = store()?;
        let change = Change {
            actor: "agent:reviewer",
            git_commit: None,
        };
        let attempt = store
            .acquire_worker_attempt("REQ-3", "reviewer", &"d".repeat(40), 60, change)?
            .context("attempt")?;
        let first = store.submit_worker_attempt(
            &attempt.attempt_id,
            "reviewer",
            WorkerSuggestion::Review {
                verdict: "needs_changes",
                rationale: "same rationale",
            },
            change,
        )?;
        assert_eq!(first.status, WorkerAttemptStatus::Submitted);
        assert!(
            store
                .submit_worker_attempt(
                    &attempt.attempt_id,
                    "reviewer",
                    WorkerSuggestion::Review {
                        verdict: "looks_consistent",
                        rationale: "same rationale"
                    },
                    change,
                )
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn cancel_releases_the_lease_without_completing_the_task() -> Result<()> {
        let mut store = store()?;
        let change = Change {
            actor: "agent:reviewer",
            git_commit: None,
        };
        let attempt = store
            .acquire_worker_attempt("REQ-2", "reviewer", &"b".repeat(40), 60, change)?
            .context("attempt")?;
        let cancelled = store.cancel_worker_attempt(&attempt.attempt_id, "reviewer", change)?;
        assert_eq!(cancelled.status, WorkerAttemptStatus::Cancelled);
        assert_eq!(
            store.cancel_worker_attempt(&attempt.attempt_id, "reviewer", change)?,
            cancelled
        );
        assert_eq!(store.task_scheduler_status()?.infrastructure.ready, 1);
        assert!(store.task_candidate("T1")?.is_none());
        Ok(())
    }
}
