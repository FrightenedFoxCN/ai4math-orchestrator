//! Typed, durable producer for a structurally stalled scheduler episode.
use super::{Change, StateStore, TaskTrackStatus, event, now_ms, scheduler_snapshot_at};
use anyhow::Result;
use rusqlite::{OptionalExtension, Transaction, params};
use serde_json::json;

pub(in crate::state) const ALL_TRACKS_BLOCKED: &str = "ALL_TRACKS_BLOCKED";
const DEBOUNCE_MS: i64 = 3000;
const ENTITY: &str = "scheduler";

impl StateStore {
    /// Observe the scheduler and, if both tracks remain structurally blocked for
    /// at least three seconds across separate observations of the same ledger
    /// state, append one starvation event and return its sequence. An observed
    /// recovery rearms the latch; unsafe Git intent or an active advisory hold
    /// prevents evaluation and invalidates only the pending debounce.
    ///
    /// Pass `reset_pending` on the daemon's first successful observation after
    /// startup so downtime cannot establish the debounce. This never clears a
    /// latched event; only an observed non-blocked scheduler does that.
    pub fn reconcile_scheduler_starvation(&mut self, reset_pending: bool) -> Result<Option<i64>> {
        self.write(|tx| reconcile_at(tx, reset_pending, now_ms(tx)?))
    }
}

fn blocked(track: TaskTrackStatus) -> Option<i64> {
    let count = track.breaker_blocked + track.dependency_blocked;
    (track.ready == 0 && track.active == 0 && count > 0).then_some(count)
}

fn clear_pending(tx: &Transaction<'_>) -> Result<Option<i64>> {
    // A skipped observation isn't evidence of recovery. In particular an
    // Oracle hold must not rearm a latched structural episode, even indirectly.
    tx.execute(
        "DELETE FROM scheduler_starvation WHERE singleton=1 AND emitted_event_seq IS NULL",
        [],
    )?;
    Ok(None)
}

fn reconcile_at(tx: &Transaction<'_>, reset_pending: bool, now: i64) -> Result<Option<i64>> {
    let unsafe_git: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM promotion_intent) OR EXISTS(SELECT 1 FROM pivot_resume_intent)",
        [],
        |r| r.get(0),
    )?;
    if unsafe_git {
        return clear_pending(tx);
    }
    let status = scheduler_snapshot_at(tx, now)?;
    // Holding even an otherwise irrelevant task may block another task via
    // a cross-track dependency. Exclude the entire observation, not the row.
    if status.exploration.held_for_oracle != 0 || status.infrastructure.held_for_oracle != 0 {
        return clear_pending(tx);
    }
    let (Some(exploration_blocked), Some(infrastructure_blocked)) =
        (blocked(status.exploration), blocked(status.infrastructure))
    else {
        tx.execute("DELETE FROM scheduler_starvation WHERE singleton=1", [])?;
        return Ok(None);
    };

    let episode: Option<(Option<i64>, Option<i64>, Option<i64>)> = tx
        .query_row(
            "SELECT pending_since_ms,pending_ledger_seq,emitted_event_seq
             FROM scheduler_starvation WHERE singleton=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    if matches!(episode, Some((_, _, Some(_)))) {
        return Ok(None);
    }
    // Before emission, any write to the event ledger conservatively breaks
    // continuity, even if the current counts happen to match the old ones.
    // Time-only expiry can remove active leases and holds, not restore them;
    // use the same clock for the snapshot and debounce, and restart on rewind
    // past the anchor. The writer lock covers classification through emission.
    let ledger_seq: i64 =
        tx.query_row("SELECT coalesce(max(event_seq),0) FROM events", [], |r| {
            r.get(0)
        })?;
    match episode {
        Some((Some(since), Some(seq), None))
            if !reset_pending && seq == ledger_seq && now >= since =>
        {
            if now - since < DEBOUNCE_MS {
                return Ok(None);
            }
        }
        _ => {
            tx.execute(
                "INSERT INTO scheduler_starvation(singleton,pending_since_ms,pending_ledger_seq)
                 VALUES (1,?1,?2) ON CONFLICT(singleton) DO UPDATE
                 SET pending_since_ms=excluded.pending_since_ms,pending_ledger_seq=excluded.pending_ledger_seq",
                params![now, ledger_seq],
            )?;
            return Ok(None);
        }
    }

    let event_seq = event(
        tx,
        Change {
            actor: "daemon:scheduler",
            git_commit: None,
        },
        ALL_TRACKS_BLOCKED,
        ENTITY,
        ENTITY,
        json!({"exploration_blocked":exploration_blocked,"infrastructure_blocked":infrastructure_blocked}),
    )?;
    tx.execute(
        "UPDATE scheduler_starvation SET pending_since_ms=NULL,pending_ledger_seq=NULL,emitted_event_seq=?1
         WHERE singleton=1",
        [event_seq],
    )?;
    Ok(Some(event_seq))
}

#[cfg(test)]
mod tests;
