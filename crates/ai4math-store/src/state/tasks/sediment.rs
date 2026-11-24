//! Explicit leases for a reviewed sediment adaptation. A scheduler row is
//! metadata and its immutable link carries no trust, receipt, or promotion.
use super::{
    ACTIVE_LIMIT, Change, DEPENDENCY_BLOCKED, HELD_FOR_ORACLE, MODULE_GATED, StateStore,
    TASK_LIMIT, TRACK_ACTIVE_LIMIT, TaskLease, TaskTrack, active_counts, event, issue_lease,
    now_ms, valid_task_id, validate_lease_duration,
};
use crate::{
    retained::valid_id,
    state::{
        last_seq,
        sediment_adaptation::{read_approval, validate_for_append},
    },
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::json;
use std::time::Duration;

pub(super) const ADAPTATION_TASK_PREFIX: &str = "adapt-";
pub(super) const ADAPTATION_LINKED: &str =
    "EXISTS(SELECT 1 FROM scheduler_sediment_adaptations link
    WHERE link.task_seq=t.queue_seq)";
const TRACK: TaskTrack = TaskTrack::Infrastructure;
const ENGINE: &str = "lean4";
const ACTOR: &str = "operator:sediment-adaptation";

pub(super) fn link_table_exists(db: &Connection) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema
            WHERE type='table' AND name='scheduler_sediment_adaptations')",
        [],
        |r| r.get(0),
    )?)
}

impl StateStore {
    /// The entire lowercase approval digest identifies the scoped scheduler row.
    /// Computing the ID does not consult or mutate the ledger.
    pub fn sediment_adaptation_task_id(approval: &str) -> Result<String> {
        ensure!(
            valid_id(approval),
            "invalid sediment adaptation approval SHA-256"
        );
        Ok(format!("{ADAPTATION_TASK_PREFIX}{approval}"))
    }

    /// Explicit Infrastructure lease. The coordinator must observe the exact
    /// managed repo and HEAD under its Git lock and authenticate the archive and
    /// frozen destination. The store rechecks ledger gates and a current CAS;
    /// it cannot observe Git or authorize anything beyond scheduler bookkeeping.
    /// Refusal for capacity, scheduling gates or an active/completed scope is a
    /// no-write None. Failed/expired leases require a fresh explicit request.
    pub fn lease_sediment_adaptation(
        &mut self,
        approval: &str,
        current_repo: &str,
        current_head: &str,
        expected_event_seq: i64,
        lease_for: Duration,
    ) -> Result<Option<TaskLease>> {
        let id = Self::sediment_adaptation_task_id(approval)?;
        validate_lease_duration(lease_for)?;
        self.write(|tx| {
            ensure!(
                last_seq(tx)? == expected_event_seq,
                "ledger event sequence changed while preparing sediment adaptation dispatch"
            );
            let record = read_approval(tx, approval)?.context("unknown sediment adaptation approval")?;
            let plan = record.plan;
            ensure!(
                plan.repo == current_repo && plan.reviewed_commit == current_head,
                "sediment adaptation approval was reviewed for a different repository or HEAD"
            );
            validate_for_append(tx, &plan)?;
            ensure!(valid_task_id(&plan.target_claim), "invalid worker Claim ID for sediment adaptation");

            let enrolled = tx.query_row(
                "SELECT link.task_seq, coalesce(t.task_id=?2 AND t.track=?3 AND t.engine=?4
                    AND t.module_path=?5 AND t.target=?6, 0)
                 FROM scheduler_sediment_adaptations link
                 LEFT JOIN scheduler_tasks t ON t.queue_seq=link.task_seq
                 WHERE link.approval_sha256=?1",
                params![approval, id, TRACK.as_str(), ENGINE, plan.target_module, plan.target_decl],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, bool>(1)?)),
            ).optional()?;
            let seq = if let Some((seq, matches)) = enrolled {
                ensure!(matches, "sediment adaptation scheduler link identity mismatch");
                Some(seq)
            } else {
                let collision: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM scheduler_tasks WHERE task_id=?1)",
                    [&id], |r| r.get(0),
                )?;
                ensure!(!collision, "sediment adaptation task ID collides with an existing task");
                None
            };

            let now = now_ms(tx)?;
            let (exploration, infrastructure) = active_counts(tx, now)?;
            if exploration + infrastructure >= ACTIVE_LIMIT || infrastructure >= TRACK_ACTIVE_LIMIT {
                return Ok(None);
            }
            // For first enrollment there is no queue position, so there cannot
            // yet be an Oracle hold or dependency. The module breaker still
            // applies. Overlapping approvals for one physical slot serialize.
            let eligible: bool = tx.query_row(&format!(
                "WITH scoped AS (
                    SELECT s.queue_seq,coalesce(s.last_fence,0) AS last_fence,s.completed_at_ms,
                           s.lease_expires_at_ms,?3 AS engine,?4 AS module_path
                    FROM (SELECT 1) seed LEFT JOIN scheduler_tasks s ON s.queue_seq=?2
                 ) SELECT t.completed_at_ms IS NULL
                     AND (t.lease_expires_at_ms IS NULL OR t.lease_expires_at_ms<=?1)
                     AND NOT {HELD_FOR_ORACLE} AND NOT {MODULE_GATED} AND NOT {DEPENDENCY_BLOCKED}
                     AND NOT EXISTS(SELECT 1 FROM scheduler_sediment_adaptations l
                         JOIN scheduler_tasks active ON active.queue_seq=l.task_seq
                         WHERE active.engine=t.engine AND active.module_path=t.module_path COLLATE NOCASE
                           AND active.target=?5 AND active.lease_expires_at_ms>?1)
                     FROM scoped t"),
                params![now, seq, ENGINE, plan.target_module, plan.target_decl], |r| r.get(0),
            )?;
            if !eligible {
                return Ok(None);
            }

            let change = Change { actor: ACTOR, git_commit: Some(current_head) };
            let seq = if let Some(seq) = seq {
                seq
            } else {
                let count: i64 = tx.query_row("SELECT count(*) FROM scheduler_tasks", [], |r| r.get(0))?;
                ensure!(count < TASK_LIMIT, "task scheduler registry is full");
                tx.execute("INSERT INTO scheduler_tasks(task_id,track,engine,module_path,target) VALUES(?1,?2,?3,?4,?5)",
                    params![id, TRACK.as_str(), ENGINE, plan.target_module, plan.target_decl])?;
                let seq = tx.last_insert_rowid();
                let registered = event(tx, change, "TASK_REGISTERED", "task", &id,
                    json!({"track":TRACK, "engine":ENGINE, "dependencies":0, "approval_sha256":approval}))?;
                tx.execute("INSERT INTO scheduler_sediment_adaptations(task_seq,approval_sha256,registered_event_seq) VALUES(?1,?2,?3)",
                    params![seq, approval, registered])?;
                seq
            };
            let (fence, expires_at_ms) = issue_lease(tx, seq, None, lease_for)?;
            event(tx, change, "TASK_LEASED", "task", &id,
                json!({"track":TRACK, "expires_at_ms":expires_at_ms, "approval_sha256":approval}))?;
            Ok(Some(TaskLease { id, track: TRACK, engine: ENGINE.to_owned(),
                module_path: plan.target_module, target: plan.target_decl, expires_at_ms, fence }))
        })
    }
}

pub(super) fn check_candidate_scope(
    db: &Connection,
    seq: i64,
    lease: &TaskLease,
    base_commit: &str,
    claim_id: &str,
    role: &str,
    native: bool,
) -> Result<()> {
    let approval: Option<String> = db
        .query_row(
            "SELECT approval_sha256 FROM scheduler_sediment_adaptations WHERE task_seq=?1",
            [seq],
            |r| r.get(0),
        )
        .optional()?;
    let Some(approval) = approval else {
        return Ok(());
    };
    let record =
        read_approval(db, &approval)?.context("missing linked sediment adaptation approval")?;
    let plan = record.plan;
    ensure!(
        native
            && role == "prover_worker"
            && base_commit == plan.reviewed_commit
            && claim_id == plan.target_claim
            && lease.id == StateStore::sediment_adaptation_task_id(&approval)?
            && lease.track == TRACK
            && lease.engine == ENGINE
            && lease.module_path == plan.target_module
            && lease.target == plan.target_decl,
        "sediment adaptation requires a native prover_worker candidate matching the approved base and Claim"
    );
    validate_for_append(db, &plan)
}

#[cfg(test)]
mod tests;
