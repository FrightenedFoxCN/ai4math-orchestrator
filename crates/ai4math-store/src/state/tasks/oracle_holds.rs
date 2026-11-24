//! Bounded scheduler-only action on explicitly reviewed advisory evidence.
use super::{now_ms, valid_task_id};
use crate::retained::valid_id;
use crate::state::{Change, StateStore, event, oracle};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use serde_json::json;

pub(in crate::state) const HELD: &str = "ORACLE_TASK_HELD";
pub(in crate::state) const RELEASED: &str = "ORACLE_TASK_HOLD_RELEASED";

// The generation latest at this snapshot is the only one that can act. In
// particular, an older expired generation cannot take over if the clock moves
// backwards after a newer generation was created and released.
pub(super) const HELD_FOR_ORACLE: &str = "(t.last_fence=0 AND coalesce((SELECT h.released_at_ms IS NULL AND h.expires_at_ms > ?1
    FROM scheduler_oracle_holds h WHERE h.task_seq=t.queue_seq ORDER BY h.event_seq DESC LIMIT 1), 0))";

/// Operator readback of one soft scheduling hold. No proof trust is conferred.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OracleTaskHold {
    pub id: String,
    pub task_id: String,
    pub observation_id: String,
    pub repo: String,
    pub commit: String,
    /// Unix milliseconds on the scheduler clock.
    pub created_at: i64,
    /// Unix milliseconds on the scheduler clock.
    pub expires_at: i64,
    /// Unix milliseconds; absent if not explicitly released before expiry.
    pub released_at: Option<i64>,
    pub active: bool,
}

pub(super) fn hold_table_exists(db: &Connection) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='scheduler_oracle_holds')",
        [], |r| r.get(0),
    )?)
}

fn read_hold(
    db: &Connection,
    task_id: &str,
    observation_id: Option<&str>,
    now: i64,
) -> Result<Option<OracleTaskHold>> {
    Ok(db.query_row(
        "SELECT h.id,h.observation_id,o.repo,o.git_commit,h.created_at_ms,h.expires_at_ms,h.released_at_ms,
                t.last_fence=0 AND h.released_at_ms IS NULL AND h.expires_at_ms>?3 AND NOT EXISTS(
                    SELECT 1 FROM scheduler_oracle_holds newer WHERE newer.task_seq=h.task_seq
                    AND newer.event_seq>h.event_seq)
         FROM scheduler_oracle_holds h JOIN scheduler_tasks t ON t.queue_seq=h.task_seq
         JOIN oracle_observations o ON o.id=h.observation_id
         WHERE t.task_id=?1 AND (?2 IS NULL OR h.observation_id=?2)
         ORDER BY h.event_seq DESC LIMIT 1",
        params![task_id, observation_id, now],
        |r| Ok(OracleTaskHold {
            id: r.get(0)?,
            task_id: task_id.into(),
            observation_id: r.get(1)?,
            repo: r.get(2)?,
            commit: r.get(3)?,
            created_at: r.get(4)?,
            expires_at: r.get(5)?,
            released_at: r.get(6)?,
            active: r.get(7)?,
        }),
    ).optional()?)
}

impl StateStore {
    /// Under the coordinator Git lock, pass the canonical bound repository and
    /// current full commit observed there. The ledger can check the exact
    /// retained commit and live Claim/repository binding, but cannot observe Git.
    /// Repeating a task/observation pair never creates or extends a hold.
    pub fn hold_task_for_oracle(
        &mut self,
        repo: &str,
        commit: &str,
        task_id: &str,
        observation_id: &str,
        seconds: u64,
    ) -> Result<OracleTaskHold> {
        ensure!(
            (1..=3600).contains(&seconds),
            "Oracle task hold must be between 1 and 3600 seconds"
        );
        ensure!(valid_task_id(task_id), "invalid task ID");
        ensure!(
            valid_id(observation_id),
            "invalid retained Oracle observation ID"
        );
        self.write(|tx| {
            let retained = oracle::read_observation(tx, observation_id)?
                .context("unknown retained Oracle observation")?;
            let approval = &retained.approval;
            ensure!(approval.repo == repo && approval.commit == commit,
                "Oracle observation was approved for a different repository or commit");
            let live_approval = oracle::read_approval(tx, repo, &approval.claim_id, commit,
                &approval.query_sha256, approval.solver_sha256s.each_ref().map(String::as_str))?;
            ensure!(live_approval == *approval, "Oracle observation no longer matches the live Claim context");
            oracle::require_corroborated_counterexample(&retained)?;

            let now = now_ms(tx)?;
            if let Some(previous) = read_hold(tx, task_id, Some(observation_id), now)? {
                return Ok(previous);
            }
            let seq: i64 = tx.query_row(
                "SELECT t.queue_seq FROM scheduler_tasks t JOIN claims c ON c.claim_id=?2
                 WHERE t.task_id=?1 AND t.track='exploration'
                   AND t.completed_at_ms IS NULL AND t.last_fence=0 AND t.lease_expires_at_ms IS NULL
                   AND t.engine=c.engine AND t.module_path=c.module_path
                   AND c.formal_target=(t.module_path || '#' || t.target)
                   AND c.stale=0 AND c.effective_stale=0 AND c.receipt_id IS NULL
                   AND c.local_trust_level NOT IN ('L2b','L5')
                   AND c.effective_trust_level NOT IN ('L2b','L5')",
                params![task_id, approval.claim_id], |r| r.get(0),
            ).optional()?.context("Oracle hold requires an unstarted exploration task matching a non-stale, unrefuted and unproved Claim")?;
            ensure!(!read_hold(tx, task_id, None, now)?.is_some_and(|hold| hold.active),
                "task already has an active Oracle hold");
            let created_at = now_ms(tx)?;
            let expires_at = created_at + seconds as i64 * 1000;
            let id: String = tx.query_row("SELECT lower(hex(randomblob(32)))", [], |r| r.get(0))?;
            let event_seq = event(tx, Change { actor: "operator:oracle", git_commit: Some(commit) },
                HELD, "task", task_id,
                json!({"id":id,"observation_id":observation_id,"expires_at":expires_at}))?;
            tx.execute("INSERT INTO scheduler_oracle_holds(id,task_seq,observation_id,created_at_ms,expires_at_ms,event_seq)
                        VALUES (?1,?2,?3,?4,?5,?6)",
                params![id, seq, observation_id, created_at, expires_at, event_seq])?;
            Ok(OracleTaskHold { id, task_id: task_id.into(), observation_id: observation_id.into(),
                repo: repo.into(), commit: commit.into(), created_at, expires_at, released_at: None, active: true })
        })
    }

    /// Only the exact latest generation may be released. Replaying a release of
    /// that generation, or releasing it after expiry, performs no write.
    pub fn release_oracle_task_hold(
        &mut self,
        task_id: &str,
        hold_id: &str,
    ) -> Result<OracleTaskHold> {
        ensure!(valid_task_id(task_id), "invalid task ID");
        ensure!(valid_id(hold_id), "invalid Oracle task hold ID");
        self.write(|tx| {
            let now = now_ms(tx)?;
            let mut hold = read_hold(tx, task_id, None, now)?.context("task has no Oracle hold")?;
            ensure!(hold.id == hold_id, "Oracle task hold generation is not the latest for this task");
            if !hold.active {
                return Ok(hold);
            }
            let released_at = now.max(hold.created_at);
            tx.execute("UPDATE scheduler_oracle_holds SET released_at_ms=?2 WHERE id=?1",
                params![hold_id, released_at])?;
            event(tx, Change { actor: "operator:oracle", git_commit: None }, RELEASED, "task", task_id,
                json!({"id":hold_id,"observation_id":hold.observation_id,"released_at":released_at}))?;
            hold.released_at = Some(released_at);
            hold.active = false;
            Ok(hold)
        })
    }

    /// Read back the latest generation, including released and expired history.
    /// Legacy readonly ledgers without the additive table have no holds.
    pub fn oracle_task_hold(&self, task_id: &str) -> Result<Option<OracleTaskHold>> {
        ensure!(valid_task_id(task_id), "invalid task ID");
        if !hold_table_exists(&self.db)? {
            return Ok(None);
        }
        read_hold(&self.db, task_id, None, now_ms(&self.db)?)
    }
}
