//! Metadata-only scheduling. Only trusted operator/coordinator code may call the
//! mutators; Change is audit attribution, not authorization. These routines do
//! not dispatch work, change proof trust, or authorize a promotion.
use super::{Change, StateStore, event};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::Serialize;
use serde_json::json;
use std::{collections::BTreeSet, fmt, str::FromStr, time::Duration};

mod oracle_holds;
mod runs;
mod starvation;
pub use oracle_holds::OracleTaskHold;
pub(super) use oracle_holds::{HELD, RELEASED};
use oracle_holds::{HELD_FOR_ORACLE, hold_table_exists};
pub use runs::{TaskCandidate, TaskCandidateSource, TaskRunOutcome};
pub(super) use starvation::ALL_TRACKS_BLOCKED;

const TASK_LIMIT: i64 = 4096;
const DEPENDENCY_LIMIT: usize = 32;
const ACTIVE_LIMIT: i64 = 6;
const TRACK_ACTIVE_LIMIT: i64 = 4;
const MAX_LEASE: Duration = Duration::from_secs(3600);
const CLOCK_MS: &str = "CAST(strftime('%s','now') AS INTEGER)*1000 +
    CAST(substr(strftime('%f','now'),4,3) AS INTEGER)";

// An archived (abandoned) breaker isn't a checked recovery. All matching rows
// must be CLOSED, not only the most recent, and no task status is consulted.
const MODULE_GATED: &str = "EXISTS(SELECT 1 FROM circuit_breakers b
    WHERE b.engine=t.engine AND b.module_path=t.module_path AND b.state<>'CLOSED')";
const DEPENDENCY_BLOCKED: &str = "EXISTS(SELECT 1 FROM scheduler_task_dependencies d
    LEFT JOIN scheduler_tasks parent ON parent.queue_seq=d.dep_seq
    WHERE d.task_seq=t.queue_seq AND parent.completed_at_ms IS NULL)";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskTrack {
    Exploration,
    Infrastructure,
}

impl TaskTrack {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Exploration => "exploration",
            Self::Infrastructure => "infrastructure",
        }
    }
}

impl FromStr for TaskTrack {
    type Err = anyhow::Error;

    fn from_str(track: &str) -> Result<Self> {
        match track {
            "exploration" => Ok(Self::Exploration),
            "infrastructure" => Ok(Self::Infrastructure),
            _ => anyhow::bail!("unknown task track"),
        }
    }
}

#[derive(Debug)]
pub struct NewTask<'a> {
    pub id: &'a str,
    pub track: TaskTrack,
    pub engine: &'a str,
    /// Canonical repository-relative module identity, matching breaker rows exactly.
    pub module_path: &'a str,
    pub target: &'a str,
    /// Already-enrolled task IDs in this ledger; may cross tracks.
    pub dependencies: &'a [&'a str],
}

/// Coordinator-local lease. The private, database-scoped generation is never
/// serialized or logged. Workers should not receive this object.
#[derive(Debug)]
pub struct TaskLease {
    pub id: String,
    pub track: TaskTrack,
    pub engine: String,
    pub module_path: String,
    pub target: String,
    pub expires_at_ms: i64,
    fence: TaskFence,
}

struct TaskFence {
    scope_id: Vec<u8>,
    generation: i64,
}

impl fmt::Debug for TaskFence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<coordinator fence>")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct TaskTrackStatus {
    /// Eligible disregarding lease capacity. Expired leases can be ready again.
    pub ready: i64,
    /// Unexpired scheduler leases, not operating-system process count.
    pub active: i64,
    pub completed: i64,
    /// Overlapping diagnostic: deadline passed, including tasks now blocked.
    pub expired: i64,
    /// Unleased or expired, not held, and blocked by a matching non-CLOSED breaker.
    pub breaker_blocked: i64,
    /// Unleased or expired, not held or breaker-gated, and awaiting a dependency.
    pub dependency_blocked: i64,
    /// Unstarted tasks with a currently active Oracle hold. Zero is omitted
    /// from JSON to preserve the legacy facts projection for unheld queues.
    #[serde(skip_serializing_if = "is_zero")]
    pub held_for_oracle: i64,
    pub active_limit: i64,
}

fn is_zero(count: &i64) -> bool {
    *count == 0
}

impl Default for TaskTrackStatus {
    fn default() -> Self {
        Self {
            ready: 0,
            active: 0,
            completed: 0,
            expired: 0,
            breaker_blocked: 0,
            dependency_blocked: 0,
            held_for_oracle: 0,
            active_limit: TRACK_ACTIVE_LIMIT,
        }
    }
}

/// Read-only scheduling estimate at one SQLite snapshot, not a reservation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct TaskSchedulerStatus {
    pub exploration: TaskTrackStatus,
    pub infrastructure: TaskTrackStatus,
    pub active_limit: i64,
    /// Includes completed rows, whose identities remain reserved for dependencies.
    pub task_limit: i64,
}

fn valid_task_id(id: &str) -> bool {
    valid_token(id, 100)
}

fn valid_token(token: &str, max_bytes: usize) -> bool {
    !token.is_empty()
        && token.len() <= max_bytes
        && token
            .bytes()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, b'.' | b'_' | b'-' | b':'))
}

fn valid_module_path(path: &str) -> bool {
    path.len() <= 256
        && path.trim() == path
        && !path
            .chars()
            .any(|ch| ch.is_control() || matches!(ch, '\\' | ':'))
        && path
            .split('/')
            .all(|segment| !matches!(segment, "" | "." | ".."))
}

fn validate(task: &NewTask<'_>) -> Result<()> {
    ensure!(valid_task_id(task.id), "invalid task ID");
    ensure!(
        matches!(
            task.engine,
            "lean4" | "agda" | "rocq" | "smt" | "atp" | "cas" | "informal"
        ),
        "unknown task engine"
    );
    ensure!(
        valid_module_path(task.module_path),
        "invalid task module path"
    );
    ensure!(
        !task.target.is_empty()
            && task.target.len() <= 256
            && !task
                .target
                .chars()
                .any(|ch| ch.is_control() || ch.is_whitespace()),
        "invalid task target identity"
    );
    ensure!(
        task.dependencies.len() <= DEPENDENCY_LIMIT,
        "too many task dependencies"
    );
    let mut seen = BTreeSet::new();
    for &dependency in task.dependencies {
        ensure!(
            valid_task_id(dependency) && dependency != task.id,
            "invalid task dependency ID"
        );
        ensure!(seen.insert(dependency), "duplicate task dependency ID");
    }
    Ok(())
}

pub(super) fn now_ms(db: &Connection) -> Result<i64> {
    Ok(db.query_row(&format!("SELECT {CLOCK_MS}"), [], |r| r.get(0))?)
}

impl StateStore {
    /// Enroll an ordinary task. Only the operator/coordinator may call this API.
    pub fn register_task(&mut self, task: NewTask<'_>, change: Change<'_>) -> Result<()> {
        validate(&task)?;
        self.write(|tx| {
            let count: i64 = tx.query_row("SELECT count(*) FROM scheduler_tasks", [], |r| r.get(0))?;
            ensure!(count < TASK_LIMIT, "task scheduler registry is full");
            let dep_seqs = task.dependencies.iter().map(|dependency| {
                tx.query_row("SELECT queue_seq FROM scheduler_tasks WHERE task_id=?1", [dependency], |r| r.get::<_, i64>(0))
                    .optional()?
                    .with_context(|| format!("unknown task dependency {dependency}"))
            }).collect::<Result<Vec<_>>>()?;
            tx.execute("INSERT INTO scheduler_tasks(task_id,track,engine,module_path,target) VALUES(?1,?2,?3,?4,?5)",
                params![task.id, task.track.as_str(), task.engine, task.module_path, task.target])?;
            let seq = tx.last_insert_rowid();
            for dep_seq in dep_seqs {
                tx.execute("INSERT INTO scheduler_task_dependencies(task_seq,dep_seq) VALUES(?1,?2)", [seq, dep_seq])?;
            }
            event(tx, change, "TASK_REGISTERED", "task", task.id,
                json!({"track":task.track, "engine":task.engine, "dependencies":task.dependencies.len()}))?;
            Ok(())
        })
    }

    /// Claim FIFO within a track. Without a track, prefer the other track from
    /// the last successful claim when both are eligible. Expired leases are
    /// reclaimed in FIFO order with a new generation. None means no capacity or
    /// no eligible task; it does not mean the queue is empty.
    pub fn claim_task(
        &mut self,
        track: Option<TaskTrack>,
        lease_for: Duration,
        change: Change<'_>,
    ) -> Result<Option<TaskLease>> {
        self.claim_task_inner(track, None, lease_for, change)
    }

    /// Claim only if the ordinary scheduler would choose exactly `expected_id`.
    /// A mismatch returns None without changing a lease, event, or alternation state.
    pub fn claim_expected_task(
        &mut self,
        track: Option<TaskTrack>,
        expected_id: &str,
        lease_for: Duration,
        change: Change<'_>,
    ) -> Result<Option<TaskLease>> {
        self.claim_task_inner(track, Some(expected_id), lease_for, change)
    }

    fn claim_task_inner(
        &mut self,
        track: Option<TaskTrack>,
        expected_id: Option<&str>,
        lease_for: Duration,
        change: Change<'_>,
    ) -> Result<Option<TaskLease>> {
        ensure!(
            lease_for.as_millis() > 0 && lease_for <= MAX_LEASE,
            "task lease duration must be between 1 ms and 1 hour"
        );
        self.write(|tx| {
            let now = now_ms(tx)?;
            let (exploration, infrastructure): (i64, i64) = tx.query_row(
                "SELECT count(*) FILTER (WHERE track='exploration'),
                        count(*) FILTER (WHERE track='infrastructure')
                 FROM scheduler_tasks WHERE lease_expires_at_ms > ?1", [now], |r| Ok((r.get(0)?, r.get(1)?)))?;
            if exploration + infrastructure >= ACTIVE_LIMIT {
                return Ok(None);
            }
            let can_explore = exploration < TRACK_ACTIVE_LIMIT && track != Some(TaskTrack::Infrastructure);
            let can_build = infrastructure < TRACK_ACTIVE_LIMIT && track != Some(TaskTrack::Exploration);
            if !can_explore && !can_build {
                return Ok(None);
            }
            let last_track: Option<String> = tx.query_row("SELECT last_track FROM task_scheduler_control WHERE singleton=1", [], |r| r.get(0))?;
            let candidate = tx.query_row(&format!("SELECT t.queue_seq,t.task_id,t.track,t.engine,t.module_path,t.target
                FROM scheduler_tasks t WHERE t.completed_at_ms IS NULL
                AND (t.lease_expires_at_ms IS NULL OR t.lease_expires_at_ms <= ?1)
                AND ((?2 AND t.track='exploration') OR (?3 AND t.track='infrastructure'))
                AND NOT {HELD_FOR_ORACLE} AND NOT {MODULE_GATED} AND NOT {DEPENDENCY_BLOCKED}
                ORDER BY CASE WHEN t.track=?4 THEN 1 ELSE 0 END,t.queue_seq LIMIT 1"),
                params![now, can_explore, can_build, last_track], |r| Ok((
                    r.get::<_, i64>(0)?,r.get::<_, String>(1)?,r.get::<_, String>(2)?,r.get(3)?,r.get(4)?,r.get(5)?
                ))).optional()?;
            let Some((seq, id, track, engine, module_path, target)) = candidate else { return Ok(None); };
            if expected_id.is_some_and(|expected| expected != id) {
                return Ok(None);
            }
            let track: TaskTrack = track.parse()?;
            let fence = tx.query_row("UPDATE task_scheduler_control SET last_track=?1,last_fence=last_fence+1
                WHERE singleton=1 AND last_fence<9223372036854775807 RETURNING scope_id,last_fence", [track.as_str()],
                |r| Ok(TaskFence { scope_id: r.get(0)?, generation: r.get(1)? }))?;
            let expires_at_ms = now_ms(tx)? + lease_for.as_millis() as i64;
            tx.execute("UPDATE scheduler_tasks SET last_fence=?2,lease_expires_at_ms=?3 WHERE queue_seq=?1",
                params![seq, fence.generation, expires_at_ms])?;
            event(tx, change, "TASK_LEASED", "task", &id, json!({"track":track, "expires_at_ms":expires_at_ms}))?;
            Ok(Some(TaskLease { id, track, engine, module_path, target, expires_at_ms, fence }))
        })
    }

    /// Complete scheduler bookkeeping only. This confirms neither a proof nor a
    /// worker output. Expired, reclaimed, frozen, foreign, and replayed leases fail.
    pub fn complete_task(&mut self, lease: &TaskLease, change: Change<'_>) -> Result<()> {
        self.write(|tx| {
            ensure!(
                release_lease(tx, lease, true)?.is_some(),
                "task lease is no longer valid for completion"
            );
            event(tx, change, "TASK_COMPLETED", "task", &lease.id, json!({}))?;
            Ok(())
        })
    }

    pub fn task_scheduler_status(&self) -> Result<TaskSchedulerStatus> {
        scheduler_snapshot(&self.db)
    }
}

// All terminalization paths consume exactly one live database-scoped fence.
// SQLite evaluates 'now' consistently throughout the statement.
fn release_lease(tx: &Transaction<'_>, lease: &TaskLease, complete: bool) -> Result<Option<i64>> {
    Ok(tx
        .query_row(
            &format!(
                "UPDATE scheduler_tasks AS t
                 SET completed_at_ms=CASE WHEN ?3 THEN ({CLOCK_MS}) END, lease_expires_at_ms=NULL
                 WHERE t.task_id=?1 AND t.last_fence=?2 AND t.lease_expires_at_ms>({CLOCK_MS})
                 AND NOT {MODULE_GATED}
                 AND EXISTS(SELECT 1 FROM task_scheduler_control WHERE singleton=1 AND scope_id=?4)
                 RETURNING queue_seq"
            ),
            params![
                lease.id,
                lease.fence.generation,
                complete,
                lease.fence.scope_id
            ],
            |row| row.get(0),
        )
        .optional()?)
}

pub(crate) fn scheduler_snapshot(db: &Connection) -> Result<TaskSchedulerStatus> {
    scheduler_snapshot_at(db, now_ms(db)?)
}

fn scheduler_snapshot_at(db: &Connection, now: i64) -> Result<TaskSchedulerStatus> {
    let held = if hold_table_exists(db)? {
        HELD_FOR_ORACLE
    } else {
        "0"
    };
    let mut stmt = db.prepare(&format!(
        "WITH task_flags AS (
        SELECT t.track, t.completed_at_ms IS NOT NULL AS completed,
            coalesce(t.lease_expires_at_ms > ?1,0) AS active,
            coalesce(t.lease_expires_at_ms <= ?1,0) AS expired,
            {MODULE_GATED} AS gated, {DEPENDENCY_BLOCKED} AS waiting, {held} AS held
        FROM scheduler_tasks t
    ) SELECT track,
        count(*) FILTER (WHERE NOT completed AND NOT active AND NOT held AND NOT gated AND NOT waiting),
        count(*) FILTER (WHERE active), count(*) FILTER (WHERE completed),
        count(*) FILTER (WHERE expired),
        count(*) FILTER (WHERE NOT completed AND NOT active AND NOT held AND gated),
        count(*) FILTER (WHERE NOT completed AND NOT active AND NOT held AND NOT gated AND waiting),
        count(*) FILTER (WHERE NOT completed AND NOT active AND held)
    FROM task_flags GROUP BY track"
    ))?;
    let by_track = stmt.query_map([now], |r| {
        Ok((
            r.get::<_, String>(0)?,
            TaskTrackStatus {
                ready: r.get(1)?,
                active: r.get(2)?,
                completed: r.get(3)?,
                expired: r.get(4)?,
                breaker_blocked: r.get(5)?,
                dependency_blocked: r.get(6)?,
                held_for_oracle: r.get(7)?,
                active_limit: TRACK_ACTIVE_LIMIT,
            },
        ))
    })?;
    let mut status = TaskSchedulerStatus {
        exploration: TaskTrackStatus::default(),
        infrastructure: TaskTrackStatus::default(),
        active_limit: ACTIVE_LIMIT,
        task_limit: TASK_LIMIT,
    };
    for track_status in by_track {
        let (track, estimate) = track_status?;
        match track.parse()? {
            TaskTrack::Exploration => status.exploration = estimate,
            TaskTrack::Infrastructure => status.infrastructure = estimate,
        }
    }
    Ok(status)
}

/// Trip hook: invoke after changing the breaker to TRIPPED, within the same
/// transaction. This fences outstanding leases, without cancelling a process.
pub(super) fn freeze_module_leases(
    tx: &Transaction<'_>,
    engine: &str,
    module_path: &str,
    change: Change<'_>,
) -> Result<usize> {
    let invalidated = tx.execute(&format!("UPDATE scheduler_tasks AS t SET lease_expires_at_ms=NULL
        WHERE t.engine=?1 AND t.module_path=?2 AND t.lease_expires_at_ms IS NOT NULL AND {MODULE_GATED}"),
        params![engine, module_path])?;
    if invalidated != 0 {
        event(
            tx,
            change,
            "TASK_LEASES_FROZEN",
            "module",
            module_path,
            json!({"engine":engine, "count":invalidated}),
        )?;
    }
    Ok(invalidated)
}

#[cfg(test)]
mod run_tests;
#[cfg(test)]
mod tests;
