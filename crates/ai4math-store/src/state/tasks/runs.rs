use super::{
    Change, StateStore, TaskLease, TaskTrack, event, release_lease, valid_task_id, valid_token,
};
use anyhow::{Context, Result, ensure};
use rusqlite::{OptionalExtension, Row, params};
use serde::Serialize;
use serde_json::{Value, json};
use std::fmt;

const RESULT_LIMIT: usize = 32 * 1024;
const RUNNABLE_CLAIM: &str = "FROM scheduler_tasks t JOIN claims c ON c.claim_id=?3
    WHERE t.queue_seq=?1 AND c.engine=t.engine
    AND c.module_path=t.module_path AND c.formal_target=(t.module_path || '#' || t.target)
    AND c.stale=0 AND c.effective_stale=0
    AND c.effective_trust_level NOT IN ('L2b','L3')";

/// Coordinator-reported disposition of an ordinary worker run. Candidate data
/// is untrusted and never serves as a proof, receipt, or promotion approval.
pub enum TaskRunOutcome<'a> {
    Candidate {
        base_commit: &'a str,
        claim_id: &'a str,
        role: &'a str,
        executable_sha256: &'a str,
        result_json: &'a str,
    },
    ModelCandidate {
        base_commit: &'a str,
        claim_id: &'a str,
        role: &'a str,
        result_json: &'a str,
    },
    Failed,
}

/// How the coordinator reported a suggestion, not an authentication claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskCandidateSource {
    NativeWorker,
    DshModel,
}

impl TaskCandidateSource {
    fn is_native_worker(&self) -> bool {
        *self == Self::NativeWorker
    }
}

/// Operator-only readback of untrusted worker or model output. The ledger checks
/// that the Claim matches the task, but does not authenticate or verify the result.
#[derive(Clone, PartialEq, Eq, Serialize)]
pub struct TaskCandidate {
    pub task_id: String,
    pub track: TaskTrack,
    pub engine: String,
    pub module_path: String,
    pub target: String,
    pub base_commit: String,
    pub claim_id: String,
    pub role: String,
    #[serde(skip_serializing_if = "TaskCandidateSource::is_native_worker")]
    pub source: TaskCandidateSource,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub executable_sha256: Option<String>,
    pub result_json: String,
    pub recorded_at_ms: i64,
}

impl fmt::Debug for TaskCandidate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TaskCandidate")
            .field("task_id", &self.task_id)
            .field("recorded_at_ms", &self.recorded_at_ms)
            .finish_non_exhaustive()
    }
}

fn validate_candidate(base_commit: &str, claim_id: &str, result_json: &str) -> Result<()> {
    ensure!(
        matches!(base_commit.len(), 40 | 64)
            && base_commit.bytes().all(|ch| ch.is_ascii_hexdigit()),
        "invalid worker base commit"
    );
    ensure!(valid_task_id(claim_id), "invalid worker claim ID");
    ensure!(
        result_json.len() <= RESULT_LIMIT,
        "worker result exceeds 32 KiB"
    );
    ensure!(
        serde_json::from_str::<Value>(result_json).is_ok(),
        "invalid worker result JSON"
    );
    Ok(())
}

fn read_candidate(
    row: &Row<'_>,
    id: &str,
    source: TaskCandidateSource,
) -> rusqlite::Result<TaskCandidate> {
    Ok(TaskCandidate {
        task_id: id.to_owned(),
        track: row
            .get::<_, String>(0)?
            .parse::<TaskTrack>()
            .map_err(|_| rusqlite::Error::InvalidQuery)?,
        engine: row.get(1)?,
        module_path: row.get(2)?,
        target: row.get(3)?,
        recorded_at_ms: row.get(4)?,
        base_commit: row.get(5)?,
        claim_id: row.get(6)?,
        role: row.get(7)?,
        source,
        executable_sha256: row.get(8)?,
        result_json: row.get(9)?,
    })
}

impl StateStore {
    /// Consume a live, unfrozen lease and atomically record the run disposition.
    /// A candidate completes scheduler bookkeeping only. An explicit failure
    /// releases the lease for ordinary retry; a crash leaves TTL recovery intact.
    /// The coordinator must validate Git and the output protocol before calling;
    /// Change is attribution, not authorization.
    pub fn finish_task_run(
        &mut self,
        lease: &TaskLease,
        run: TaskRunOutcome<'_>,
        change: Change<'_>,
    ) -> Result<()> {
        let candidate = match run {
            TaskRunOutcome::Candidate {
                base_commit,
                claim_id,
                role,
                executable_sha256,
                result_json,
            } => {
                validate_candidate(base_commit, claim_id, result_json)?;
                ensure!(valid_token(role, 64), "invalid worker role");
                ensure!(
                    executable_sha256.len() == 64
                        && executable_sha256.bytes().all(|ch| ch.is_ascii_hexdigit()),
                    "invalid worker executable SHA-256"
                );
                Some((
                    base_commit,
                    claim_id,
                    role,
                    Some(executable_sha256),
                    result_json,
                ))
            }
            TaskRunOutcome::ModelCandidate {
                base_commit,
                claim_id,
                role,
                result_json,
            } => {
                validate_candidate(base_commit, claim_id, result_json)?;
                ensure!(
                    matches!(role, "prover_worker" | "independent_reviewer"),
                    "invalid model candidate role"
                );
                Some((base_commit, claim_id, role, None, result_json))
            }
            TaskRunOutcome::Failed => None,
        };

        self.write(|tx| {
            let seq = release_lease(tx, lease, candidate.is_some())?
                .context("task lease is no longer valid for worker outcome")?;
            if let Some((base_commit, claim_id, role, executable_sha256, result_json)) = candidate {
                let recorded = match executable_sha256 {
                    Some(sha) => tx.execute(
                        &format!(
                            "INSERT INTO scheduler_task_candidates
                            (task_seq,base_commit,claim_id,role,executable_sha256,result_json)
                            SELECT t.queue_seq,?2,c.claim_id,?4,?5,?6 {RUNNABLE_CLAIM}"
                        ),
                        params![seq, base_commit, claim_id, role, sha, result_json],
                    )?,
                    None => tx.execute(
                        &format!(
                            "INSERT INTO scheduler_model_task_candidates
                            (task_seq,base_commit,claim_id,role,result_json)
                            SELECT t.queue_seq,?2,c.claim_id,?4,?5 {RUNNABLE_CLAIM}"
                        ),
                        params![seq, base_commit, claim_id, role, result_json],
                    )?,
                };
                ensure!(recorded == 1, "worker Claim is not runnable for this task");
                event(
                    tx,
                    change,
                    "TASK_COMPLETED",
                    "task",
                    &lease.id,
                    json!({"untrusted_candidate": true}),
                )?;
            } else {
                event(tx, change, "TASK_RUN_FAILED", "task", &lease.id, json!({}))?;
            }
            Ok(())
        })
    }

    /// Operator-only. Do not expose this readback to workers or ordinary status
    /// consumers. In particular, treat the JSON as inert, untrusted data.
    pub fn task_candidate(&self, id: &str) -> Result<Option<TaskCandidate>> {
        ensure!(valid_task_id(id), "invalid task ID");
        let native = self
            .db
            .query_row(
                "SELECT t.track,t.engine,t.module_path,t.target,t.completed_at_ms,
                    c.base_commit,c.claim_id,c.role,c.executable_sha256,c.result_json
             FROM scheduler_task_candidates c JOIN scheduler_tasks t ON t.queue_seq=c.task_seq
             WHERE t.task_id=?1",
                [id],
                |row| read_candidate(row, id, TaskCandidateSource::NativeWorker),
            )
            .optional()?;
        if native.is_some() {
            return Ok(native);
        }
        // A read-only open never runs DDL. Old ledgers may lack the additive table.
        let model_table_exists = self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema
                WHERE type='table' AND name='scheduler_model_task_candidates')",
            [],
            |row| row.get::<_, bool>(0),
        )?;
        if !model_table_exists {
            return Ok(None);
        }
        Ok(self
            .db
            .query_row(
                "SELECT t.track,t.engine,t.module_path,t.target,t.completed_at_ms,
                    c.base_commit,c.claim_id,c.role,NULL,c.result_json
             FROM scheduler_model_task_candidates c JOIN scheduler_tasks t ON t.queue_seq=c.task_seq
             WHERE t.task_id=?1",
                [id],
                |row| read_candidate(row, id, TaskCandidateSource::DshModel),
            )
            .optional()?)
    }
}
