//! Single-use, operator-approved DSH model dispatch bookkeeping. This store
//! does not authorize access to Git, invoke a Host or validate model output.
use super::{
    ACTIVE_LIMIT, ADAPTATION_LINKED, Change, DEPENDENCY_BLOCKED, HELD_FOR_ORACLE, MODULE_GATED,
    RETRY_READY, StateStore, TRACK_ACTIVE_LIMIT, TaskLease, TaskTrack, active_counts, event,
    issue_lease, lease_duration_with_submission_grace, now_ms, release_lease, valid_token,
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, Row, Transaction, params};
use serde::Serialize;
use serde_json::json;
use std::{path::Path, time::Duration};

pub(super) const MODEL_RESERVED: &str = "EXISTS(SELECT 1 FROM scheduler_model_dispatch_approvals a
    WHERE a.task_id=t.task_id)";

const APPROVED: &str = "MODEL_DISPATCH_APPROVED";
const DISPATCHED: &str = "MODEL_DISPATCHED";
const CANCELLED: &str = "MODEL_DISPATCH_CANCELLED";
const RECORDED: &str = "MODEL_DISPATCH_RECORDED";

pub(in crate::state) fn is_model_dispatch_event(kind: &str) -> bool {
    matches!(kind, APPROVED | DISPATCHED | CANCELLED | RECORDED)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelDispatchState {
    Approved,
    Dispatched,
    Cancelled,
    Recorded,
}

impl ModelDispatchState {
    fn sql(self) -> &'static str {
        match self {
            Self::Approved => "APPROVED",
            Self::Dispatched => "DISPATCHED",
            Self::Cancelled => "CANCELLED",
            Self::Recorded => "RECORDED",
        }
    }

    fn parse(value: &str) -> rusqlite::Result<Self> {
        match value {
            "APPROVED" => Ok(Self::Approved),
            "DISPATCHED" => Ok(Self::Dispatched),
            "CANCELLED" => Ok(Self::Cancelled),
            "RECORDED" => Ok(Self::Recorded),
            _ => Err(rusqlite::Error::InvalidQuery),
        }
    }
}

/// The trusted operator must review the canonical Git repository, exact base and
/// model settings before calling. `Change` is audit attribution, not authorization.
#[derive(Debug, Clone, Copy)]
pub struct NewModelDispatchApproval<'a> {
    pub task_id: &'a str,
    pub claim_id: &'a str,
    pub role: &'a str,
    pub repo: &'a str,
    pub base_commit: &'a str,
    pub model: &'a str,
    pub max_wall_seconds: u64,
    pub max_model_calls: u64,
}

/// Durable operator metadata only; neither a result nor a proof receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ModelDispatchApproval {
    pub task_id: String,
    pub claim_id: String,
    pub role: String,
    pub repo: String,
    pub base_commit: String,
    pub model: String,
    pub max_wall_seconds: u64,
    pub max_model_calls: u64,
    pub state: ModelDispatchState,
    pub event_seq: i64,
}

impl ModelDispatchApproval {
    fn matches(&self, new: NewModelDispatchApproval<'_>) -> bool {
        self.task_id == new.task_id
            && self.claim_id == new.claim_id
            && self.role == new.role
            && self.repo == new.repo
            && self.base_commit == new.base_commit
            && self.model == new.model
            && self.max_wall_seconds == new.max_wall_seconds
            && self.max_model_calls == new.max_model_calls
    }
}

pub(super) fn model_dispatch_table_exists(db: &Connection) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema
          WHERE type='table' AND name='scheduler_model_dispatch_approvals')",
        [],
        |r| r.get(0),
    )?)
}

fn model_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 100
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
}

const APPROVAL_COLUMNS: &str = "a.task_id,a.claim_id,a.role,a.repo,a.base_commit,a.model,
    a.max_wall_seconds,a.max_model_calls,a.state,a.event_seq";
const MODEL_DISPATCH_BATCH_LIMIT: usize = 64;

fn decode_approval(r: &Row<'_>) -> rusqlite::Result<ModelDispatchApproval> {
    Ok(ModelDispatchApproval {
        task_id: r.get(0)?,
        claim_id: r.get(1)?,
        role: r.get(2)?,
        repo: r.get(3)?,
        base_commit: r.get(4)?,
        model: r.get(5)?,
        max_wall_seconds: r.get(6)?,
        max_model_calls: r.get(7)?,
        state: ModelDispatchState::parse(&r.get::<_, String>(8)?)?,
        event_seq: r.get(9)?,
    })
}

fn read(db: &Connection, id: &str) -> Result<Option<ModelDispatchApproval>> {
    Ok(db.query_row(
        &format!("SELECT {APPROVAL_COLUMNS} FROM scheduler_model_dispatch_approvals a WHERE a.task_id=?1"),
        [id], decode_approval,
    ).optional()?)
}

fn validate_scope(repo: &str, model: &str) -> Result<()> {
    ensure!(
        Path::new(repo).is_absolute() && !repo.chars().any(char::is_control),
        "expected canonical managed repository path"
    );
    ensure!(valid_token(model, 100), "invalid DSH model identifier");
    Ok(())
}

fn validate_base(base: &str) -> Result<()> {
    ensure!(
        matches!(base.len(), 40 | 64)
            && base.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
        "expected full lowercase Git base commit"
    );
    Ok(())
}

fn validate_batch_limit(limit: usize) -> Result<i64> {
    ensure!(
        (1..=MODEL_DISPATCH_BATCH_LIMIT).contains(&limit),
        "model dispatch batch limit must be in 1..={MODEL_DISPATCH_BATCH_LIMIT}"
    );
    Ok(limit as i64)
}

fn require(db: &Connection, id: &str) -> Result<ModelDispatchApproval> {
    read(db, id)?.context("no model dispatch approval for this task")
}

const EXACT_MODEL_CANDIDATE: &str = "EXISTS(SELECT 1 FROM scheduler_tasks t
    JOIN scheduler_model_task_candidates m ON m.task_seq=t.queue_seq
    JOIN claims c ON c.claim_id=m.claim_id
    JOIN managed_repo b ON b.singleton=1
    WHERE t.task_id=a.task_id AND m.base_commit=a.base_commit AND m.claim_id=a.claim_id
    AND m.role=a.role AND b.repo=a.repo
    AND t.completed_at_ms IS NOT NULL AND t.track='infrastructure' AND t.engine='lean4'
    AND c.engine=t.engine AND c.module_path=t.module_path
    AND c.formal_target=(t.module_path || '#' || t.target)
    AND NOT EXISTS(SELECT 1 FROM scheduler_task_candidates n WHERE n.task_seq=t.queue_seq))";

fn has_exact_candidate(db: &Connection, row: &ModelDispatchApproval) -> Result<bool> {
    Ok(db.query_row(
        &format!("SELECT {EXACT_MODEL_CANDIDATE} FROM scheduler_model_dispatch_approvals a WHERE a.task_id=?1"),
        [&row.task_id], |r| r.get(0),
    )?)
}

fn check_recorded_binding(db: &Connection, row: &ModelDispatchApproval) -> Result<()> {
    ensure!(
        row.state != ModelDispatchState::Recorded || has_exact_candidate(db, row)?,
        "RECORDED model dispatch has no exact durable model candidate"
    );
    Ok(())
}

fn repo_matches(db: &Connection, repo: &str) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM managed_repo WHERE singleton=1 AND repo=?1)",
        [repo],
        |r| r.get(0),
    )?)
}

fn git_available(db: &Connection) -> Result<bool> {
    Ok(
        crate::state::read_intent(db)?.is_none()
            && crate::state::pivot::read_pending(db)?.is_none(),
    )
}

fn require_available(db: &Connection, repo: &str) -> Result<()> {
    ensure!(
        repo_matches(db, repo)?,
        "model dispatch managed repository binding mismatch"
    );
    ensure!(
        git_available(db)?,
        "recover the pending Git transaction before model dispatch"
    );
    Ok(())
}

fn lease_capacity(db: &Connection, now: i64) -> Result<bool> {
    let (exploration, infrastructure) = active_counts(db, now)?;
    Ok(exploration + infrastructure < ACTIVE_LIMIT && infrastructure < TRACK_ACTIVE_LIMIT)
}

// The Claim must identify the same target as the immutable task enrollment;
// recheck trust and identity under the dispatch writer lock as well as on approval.
const MODEL_SUBJECT: &str = "t.track='infrastructure' AND t.engine='lean4'
    AND c.engine=t.engine AND c.module_path=t.module_path
    AND c.formal_target=(t.module_path || '#' || t.target)
    AND c.stale=0 AND c.effective_stale=0 AND c.effective_trust_level NOT IN ('L2b','L3')
    AND t.completed_at_ms IS NULL";
const MODEL_SLOT_FREE: &str = "(t.lease_expires_at_ms IS NULL OR t.lease_expires_at_ms<=?1)";

// Use the same eligibility predicate for a read hint and dispatch under the writer lock.
fn model_dispatch_gate() -> String {
    format!(
        "{MODEL_SUBJECT} AND {MODEL_SLOT_FREE} AND NOT {ADAPTATION_LINKED}
        AND NOT {MODULE_GATED} AND NOT {HELD_FOR_ORACLE} AND NOT {DEPENDENCY_BLOCKED}
        AND {RETRY_READY}"
    )
}

fn transition(
    tx: &Transaction<'_>,
    mut row: ModelDispatchApproval,
    state: ModelDispatchState,
    kind: &str,
    change: Change<'_>,
) -> Result<ModelDispatchApproval> {
    let seq = event(
        tx,
        Change {
            actor: change.actor,
            git_commit: Some(&row.base_commit),
        },
        kind,
        "task",
        &row.task_id,
        json!({"claim_id":row.claim_id,"role":row.role,"previous_event_seq":row.event_seq}),
    )?;
    let changed = tx.execute(
        "UPDATE scheduler_model_dispatch_approvals SET state=?2,event_seq=?3 WHERE task_id=?1 AND state=?4",
        params![row.task_id, state.sql(), seq, row.state.sql()],
    )?;
    ensure!(changed == 1, "model dispatch approval changed concurrently");
    row.state = state;
    row.event_seq = seq;
    Ok(row)
}

impl StateStore {
    /// Read one ID without migrations, execution or reconciliation.
    pub fn get_model_dispatch(&self, task_id: &str) -> Result<Option<ModelDispatchApproval>> {
        ensure!(model_id(task_id), "invalid DSH model task ID");
        if !model_dispatch_table_exists(&self.db)? {
            return Ok(None);
        }
        let snapshot = self.db.unchecked_transaction()?;
        let row = read(&snapshot, task_id)?;
        if let Some(approval) = &row {
            check_recorded_binding(&snapshot, approval)?;
        }
        snapshot.commit()?;
        Ok(row)
    }

    /// Read up to `limit` (1..=64) eligible approvals in ascending approval
    /// event order at one snapshot. The caller supplies the canonical managed
    /// repository and observed current Git base; Store never consults Git.
    /// These are hints, not reservations or a guarantee of capacity for the
    /// entire batch. Dispatch must arbitrate again under the writer lock.
    pub fn list_ready_model_dispatches(
        &self,
        repo: &str,
        base_commit: &str,
        model: &str,
        limit: usize,
    ) -> Result<Vec<ModelDispatchApproval>> {
        validate_scope(repo, model)?;
        validate_base(base_commit)?;
        let limit = validate_batch_limit(limit)?;
        let snapshot = self.db.unchecked_transaction()?;
        let now = now_ms(&snapshot)?;
        if !model_dispatch_table_exists(&snapshot)?
            || !repo_matches(&snapshot, repo)?
            || !git_available(&snapshot)?
            || !lease_capacity(&snapshot, now)?
        {
            return Ok(Vec::new());
        }
        let rows = {
            let mut stmt = snapshot.prepare(&format!(
                "SELECT {APPROVAL_COLUMNS} FROM scheduler_model_dispatch_approvals a
                 JOIN scheduler_tasks t ON t.task_id=a.task_id
                 JOIN claims c ON c.claim_id=a.claim_id
                 LEFT JOIN scheduler_task_retries r ON r.task_seq=t.queue_seq
                 WHERE a.state='APPROVED' AND a.repo=?2 AND a.base_commit=?3 AND a.model=?4
                 AND {} ORDER BY a.event_seq LIMIT ?5",
                model_dispatch_gate()
            ))?;
            stmt.query_map(
                params![now, repo, base_commit, model, limit],
                decode_approval,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?
        };
        snapshot.commit()?;
        Ok(rows)
    }

    /// First approval reserves a free, enrolled task from ordinary FIFO. An exact
    /// retry returns the original row in its current state without reapproving it;
    /// any change to the saved inputs is rejected, including after cancellation.
    pub fn approve_model_dispatch(
        &mut self,
        input: NewModelDispatchApproval<'_>,
        change: Change<'_>,
    ) -> Result<ModelDispatchApproval> {
        ensure!(
            model_id(input.task_id) && model_id(input.claim_id),
            "invalid DSH model task or Claim ID"
        );
        ensure!(
            matches!(input.role, "prover_worker" | "independent_reviewer"),
            "unsupported DSH model role"
        );
        validate_scope(input.repo, input.model)?;
        validate_base(input.base_commit)?;
        ensure!(
            (1..=60).contains(&input.max_wall_seconds),
            "DSH model wall seconds must be in 1..=60"
        );
        ensure!(
            (1..=8).contains(&input.max_model_calls),
            "DSH model call limit must be in 1..=8"
        );
        self.write(|tx| {
            if let Some(row) = read(tx, input.task_id)? {
                ensure!(row.matches(input), "conflicting model dispatch approval for this task");
                check_recorded_binding(tx, &row)?;
                return Ok(row);
            }
            require_available(tx, input.repo)?;
            let runnable: bool = tx.query_row(
                &format!("SELECT EXISTS(SELECT 1 FROM scheduler_tasks t JOIN claims c ON c.claim_id=?3
                  WHERE t.task_id=?2 AND {MODEL_SUBJECT} AND {MODEL_SLOT_FREE} AND NOT {ADAPTATION_LINKED})"),
                params![now_ms(tx)?, input.task_id, input.claim_id],
                |r| r.get(0),
            )?;
            ensure!(runnable, "task and Claim are not an unleased Lean infrastructure model subject");
            let seq = event(
                tx, Change { actor: change.actor, git_commit: Some(input.base_commit) },
                APPROVED, "task", input.task_id,
                json!({"claim_id":input.claim_id,"role":input.role,"model":input.model,
                       "max_wall_seconds":input.max_wall_seconds,"max_model_calls":input.max_model_calls}),
            )?;
            tx.execute("INSERT INTO scheduler_model_dispatch_approvals
                (task_id,claim_id,role,repo,base_commit,model,max_wall_seconds,max_model_calls,state,event_seq)
                VALUES(?1,?2,?3,?4,?5,?6,?7,?8,'APPROVED',?9)", params![
                input.task_id, input.claim_id, input.role, input.repo, input.base_commit,
                input.model, input.max_wall_seconds, input.max_model_calls, seq])?;
            require(tx, input.task_id)
        })
    }

    /// The trusted coordinator holds the Git lock and checks the physical base
    /// first. Only a successful commit authorizes one launch; all repeated calls
    /// fail. The ID-specific lease and DISPATCHED fence commit together, without
    /// selecting any other task or changing ordinary FIFO track preference.
    pub fn lease_and_dispatch_model_once(
        &mut self,
        task_id: &str,
        expected_repo: &str,
        expected_base_commit: &str,
        submission_grace: Duration,
        change: Change<'_>,
    ) -> Result<(ModelDispatchApproval, TaskLease)> {
        ensure!(model_id(task_id), "invalid DSH model task ID");
        self.write(|tx| {
            let row = require(tx, task_id)?;
            ensure!(
                row.state == ModelDispatchState::Approved,
                "model dispatch is no longer APPROVED"
            );
            ensure!(
                row.repo == expected_repo && row.base_commit == expected_base_commit,
                "model dispatch approval does not match reviewed repository and base"
            );
            require_available(tx, expected_repo)?;
            let now = now_ms(tx)?;
            ensure!(
                lease_capacity(tx, now)?,
                "no scheduler lease capacity for model dispatch"
            );
            let (seq, engine, module_path, target) = tx
                .query_row(
                    &format!(
                        "SELECT t.queue_seq,t.engine,t.module_path,t.target
                  FROM scheduler_tasks t JOIN claims c ON c.claim_id=?3
                  LEFT JOIN scheduler_task_retries r ON r.task_seq=t.queue_seq
                  WHERE t.task_id=?2 AND {}",
                        model_dispatch_gate()
                    ),
                    params![now, task_id, row.claim_id],
                    |r| Ok((r.get::<_, i64>(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .optional()?
                .context("approved model dispatch subject is not currently eligible")?;
            let lease_for = lease_duration_with_submission_grace(
                Duration::from_secs(row.max_wall_seconds),
                submission_grace,
            )?;
            let (fence, expires_at_ms) = issue_lease(tx, seq, None, lease_for)?;
            let track = TaskTrack::Infrastructure;
            event(
                tx,
                change,
                "TASK_LEASED",
                "task",
                task_id,
                json!({"track":track,"expires_at_ms":expires_at_ms}),
            )?;
            let row = transition(tx, row, ModelDispatchState::Dispatched, DISPATCHED, change)?;
            Ok((
                row,
                TaskLease {
                    id: task_id.into(),
                    track,
                    engine,
                    module_path,
                    target,
                    expires_at_ms,
                    fence,
                },
            ))
        })
    }

    /// Read-only observation of the exact coordinator-local model lease at one
    /// snapshot, without renewal, release, or reconciliation. A live result
    /// does not reserve the fence against cancellation or expiry after return.
    pub fn model_dispatch_lease_is_live(&self, lease: &TaskLease) -> Result<bool> {
        let snapshot = self.db.unchecked_transaction()?;
        if !model_dispatch_table_exists(&snapshot)? {
            return Ok(false);
        }
        let now = now_ms(&snapshot)?;
        let live = snapshot.query_row(
            &format!("SELECT EXISTS(SELECT 1 FROM scheduler_tasks t
                JOIN scheduler_model_dispatch_approvals a ON a.task_id=t.task_id
                WHERE a.state='DISPATCHED' AND t.task_id=?2 AND t.last_fence=?3
                AND t.completed_at_ms IS NULL AND t.lease_expires_at_ms>?1
                AND NOT {MODULE_GATED} AND NOT {HELD_FOR_ORACLE}
                AND EXISTS(SELECT 1 FROM task_scheduler_control WHERE singleton=1 AND scope_id=?4))"),
            params![now, lease.id, lease.fence.generation, lease.fence.scope_id],
            |r| r.get(0),
        )?;
        snapshot.commit()?;
        Ok(live)
    }

    /// Release an in-flight model lease without recording a failure or making the
    /// task eligible for another dispatch. An expired/frozen/released lease is a no-op.
    pub fn release_model_dispatch_lease(&mut self, lease: &TaskLease) -> Result<bool> {
        self.write(|tx| {
            let row = require(tx, &lease.id)?;
            Ok(row.state == ModelDispatchState::Dispatched
                && release_lease(tx, lease, false)?.is_some())
        })
    }

    /// Cancellation is idempotent but cannot race past a winning dispatch.
    /// Cancelled IDs cannot be approved again or returned to ordinary scheduling.
    pub fn cancel_model_dispatch(
        &mut self,
        task_id: &str,
        change: Change<'_>,
    ) -> Result<ModelDispatchApproval> {
        ensure!(model_id(task_id), "invalid DSH model task ID");
        self.write(|tx| {
            let row = require(tx, task_id)?;
            if row.state == ModelDispatchState::Cancelled {
                return Ok(row);
            }
            ensure!(
                row.state == ModelDispatchState::Approved,
                "only an APPROVED model dispatch can be cancelled"
            );
            transition(tx, row, ModelDispatchState::Cancelled, CANCELLED, change)
        })
    }

    /// Tombstone a dispatched ID and revoke its lease under the same writer
    /// lock as model candidate submission. A durable outcome which wins first
    /// is never altered, even if reconciliation has not marked it RECORDED.
    /// A repeated cancellation is a no-op; neither path permits redispatch.
    pub fn cancel_running_model_dispatch(
        &mut self,
        task_id: &str,
        change: Change<'_>,
    ) -> Result<ModelDispatchApproval> {
        ensure!(model_id(task_id), "invalid DSH model task ID");
        self.write(|tx| {
            let row = require(tx, task_id)?;
            if row.state == ModelDispatchState::Cancelled {
                return Ok(row);
            }
            ensure!(row.state == ModelDispatchState::Dispatched,
                "only a DISPATCHED model dispatch can be cancelled while running");
            let revoked = tx.execute(
                "UPDATE scheduler_tasks AS t SET lease_expires_at_ms=NULL
                 WHERE t.task_id=?1 AND t.completed_at_ms IS NULL
                 AND NOT EXISTS(SELECT 1 FROM scheduler_model_task_candidates m WHERE m.task_seq=t.queue_seq)
                 AND NOT EXISTS(SELECT 1 FROM scheduler_task_candidates n WHERE n.task_seq=t.queue_seq)",
                [task_id],
            )?;
            ensure!(revoked == 1, "cannot cancel a model dispatch with a durable outcome");
            transition(tx, row, ModelDispatchState::Cancelled, CANCELLED, change)
        })
    }

    /// Record up to `limit` (1..=64) already durable exact DshModel candidates
    /// in DISPATCHED event order. Scope is repository and model, deliberately
    /// not current Git HEAD: each candidate must match its approval's old base.
    /// Non-matches are filtered before LIMIT; no leases are issued or reset.
    /// The return value contains only approvals newly advanced to RECORDED.
    pub fn reconcile_model_dispatches(
        &mut self,
        repo: &str,
        model: &str,
        limit: usize,
        change: Change<'_>,
    ) -> Result<Vec<ModelDispatchApproval>> {
        validate_scope(repo, model)?;
        let limit = validate_batch_limit(limit)?;
        self.write(|tx| {
            let rows = {
                let mut stmt = tx.prepare(&format!(
                    "SELECT {APPROVAL_COLUMNS} FROM scheduler_model_dispatch_approvals a
                     WHERE a.state='DISPATCHED' AND a.repo=?1 AND a.model=?2
                     AND {EXACT_MODEL_CANDIDATE} ORDER BY a.event_seq LIMIT ?3"
                ))?;
                stmt.query_map(params![repo, model, limit], decode_approval)?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            };
            rows.into_iter()
                .map(|row| transition(tx, row, ModelDispatchState::Recorded, RECORDED, change))
                .collect()
        })
    }

    /// Observe an exact, already durable DshModel candidate by ID. Neither the
    /// candidate output nor scheduler completion alone can advance the ledger.
    /// No candidate (or a mismatched one) leaves DISPATCHED unchanged; repeated
    /// calls after RECORDED append nothing. This method never dispatches.
    pub fn reconcile_model_dispatch(
        &mut self,
        task_id: &str,
        change: Change<'_>,
    ) -> Result<ModelDispatchApproval> {
        ensure!(model_id(task_id), "invalid DSH model task ID");
        self.write(|tx| {
            let row = require(tx, task_id)?;
            if row.state != ModelDispatchState::Dispatched {
                check_recorded_binding(tx, &row)?;
                return Ok(row);
            }
            if !has_exact_candidate(tx, &row)? {
                return Ok(row);
            }
            transition(tx, row, ModelDispatchState::Recorded, RECORDED, change)
        })
    }
}

pub(super) fn check_model_dispatch_outcome(
    tx: &Transaction<'_>,
    id: &str,
    model_binding: Option<(&str, &str, &str)>,
) -> Result<()> {
    if let Some(row) = read(tx, id)? {
        match row.state {
            ModelDispatchState::Dispatched => {
                ensure!(
                    model_binding == Some((&row.base_commit, &row.claim_id, &row.role)),
                    "only an exactly bound model candidate can complete a dispatched approval"
                );
                require_available(tx, &row.repo)?;
            }
            _ => anyhow::bail!("model dispatch approval does not permit this task outcome"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
