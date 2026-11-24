use super::{
    ClaimState, RecheckOutcome, StateStore, T3BatchReport, T3PlanApproval, last_seq,
    require_applied_t3_plan,
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum T3SweepState {
    NotStarted,
    Running,
    /// A full pass through the worklist finished; individual claims may still be stale.
    Complete,
}

/// Last successful page checkpoint for an applied T3 plan. This is navigation, not evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct T3SweepStatus {
    pub plan_sha256: String,
    pub reviewed_commit: String,
    pub status: T3SweepState,
    pub next_after_claim: Option<String>,
    pub checkpoint_event_seq: Option<i64>,
    pub revision: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct T3SweepStep {
    pub progress: T3SweepStatus,
    /// An existing checkpoint was discarded in favor of starting from the first page.
    pub rewound: bool,
    /// None only when completion was already latched and no restart was requested.
    pub batch: Option<T3BatchReport>,
}

struct Checkpoint {
    next_after_claim: Option<String>,
    event_seq: i64,
    revision: i64,
}

impl T3SweepStatus {
    fn new(plan: T3PlanApproval, checkpoint: Option<Checkpoint>) -> Self {
        let (status, next_after_claim, checkpoint_event_seq, revision) = match checkpoint {
            Some(c) => (
                if c.next_after_claim.is_some() {
                    T3SweepState::Running
                } else {
                    T3SweepState::Complete
                },
                c.next_after_claim,
                Some(c.event_seq),
                Some(c.revision),
            ),
            None => (T3SweepState::NotStarted, None, None, None),
        };
        Self {
            plan_sha256: plan.plan_sha256,
            reviewed_commit: plan.reviewed_commit,
            status,
            next_after_claim,
            checkpoint_event_seq,
            revision,
        }
    }
}

impl StateStore {
    /// Read an applied plan's last sweep checkpoint, without migrating older databases.
    /// An unfinished checkpoint can be stale; the next step will check its generation.
    pub fn t3_sweep_status(&self, hash: &str) -> Result<T3SweepStatus> {
        let snapshot = self.db.unchecked_transaction()?;
        let plan = require_applied_t3_plan(&snapshot, hash)?.approval;
        let has_table: bool = snapshot.query_row(
            "SELECT EXISTS(SELECT 1 FROM main.sqlite_schema
             WHERE type='table' AND name='t3_sweep_progress')",
            [],
            |r| r.get(0),
        )?;
        let checkpoint = if has_table {
            read_checkpoint(&snapshot, hash)?
        } else {
            None
        };
        snapshot.commit()?;
        Ok(T3SweepStatus::new(plan, checkpoint))
    }

    /// Run at most one page of the existing applied-plan recheck. A completed pass
    /// latches until explicit restart. Unfinished passes rewind on ledger changes,
    /// including ones made by independently committed proofs on an interrupted page.
    pub fn t3_sweep_step(
        &mut self,
        hash: &str,
        limit: usize,
        restart: bool,
        actor: &str,
        check: impl FnMut(&ClaimState) -> Result<RecheckOutcome>,
    ) -> Result<T3SweepStep> {
        ensure!((1..=8).contains(&limit), "T3 sweep limit must be 1..=8");
        let (plan, checkpoint, seq) = {
            let snapshot = self.db.transaction()?;
            let plan = require_applied_t3_plan(&snapshot, hash)?.approval;
            let checkpoint = read_checkpoint(&snapshot, hash)?;
            let seq = last_seq(&snapshot)?;
            snapshot.commit()?;
            (plan, checkpoint, seq)
        };
        if !restart
            && checkpoint
                .as_ref()
                .is_some_and(|c| c.next_after_claim.is_none())
        {
            return Ok(T3SweepStep {
                progress: T3SweepStatus::new(plan, checkpoint),
                rewound: false,
                batch: None,
            });
        }

        let rewound = checkpoint
            .as_ref()
            .is_some_and(|c| restart || c.event_seq != seq);
        let after = checkpoint.as_ref().filter(|_| !rewound).and_then(|c| {
            c.next_after_claim
                .as_deref()
                .map(|cursor| (cursor, c.event_seq))
        });
        // Only the existing batch path can recheck eligible claims or commit proof evidence.
        let batch = self.recheck_page_stale_for_plan(hash, limit, after, actor, check)?;
        let next = self.write(|tx| {
            ensure!(
                last_seq(tx)? == batch.last_event_seq,
                "ledger changed before saving the T3 sweep checkpoint; rerun sweep step"
            );
            write_checkpoint(tx, hash, checkpoint.as_ref(), &batch)
        })?;
        Ok(T3SweepStep {
            progress: T3SweepStatus::new(plan, Some(next)),
            rewound,
            batch: Some(batch),
        })
    }
}

fn read_checkpoint(db: &Connection, hash: &str) -> Result<Option<Checkpoint>> {
    Ok(db
        .query_row(
            "SELECT next_after_claim, checkpoint_event_seq, revision FROM main.t3_sweep_progress
         WHERE plan_sha256=?1",
            [hash],
            |r| {
                Ok(Checkpoint {
                    next_after_claim: r.get(0)?,
                    event_seq: r.get(1)?,
                    revision: r.get(2)?,
                })
            },
        )
        .optional()?)
}

// The caller holds the immediate write transaction for both the ledger check and CAS.
// Do not append an event: checkpoints neither change the worklist nor confer trust.
fn write_checkpoint(
    tx: &rusqlite::Transaction<'_>,
    hash: &str,
    previous: Option<&Checkpoint>,
    batch: &T3BatchReport,
) -> Result<Checkpoint> {
    let next = &batch.next_after_claim;
    let seq = batch.last_event_seq;
    let (updated, revision) = match previous {
        Some(old) => {
            let revision = old.revision.checked_add(1).context("T3 sweep revision overflow")?;
            let updated = tx.execute(
                "UPDATE main.t3_sweep_progress
                 SET next_after_claim=?2, checkpoint_event_seq=?3, revision=?4
                 WHERE plan_sha256=?1 AND revision=?5 AND checkpoint_event_seq=?6 AND next_after_claim IS ?7",
                params![hash, next, seq, revision, old.revision, old.event_seq, old.next_after_claim],
            )?;
            (updated, revision)
        }
        None => (
            tx.execute(
                "INSERT INTO main.t3_sweep_progress (plan_sha256, next_after_claim, checkpoint_event_seq, revision)
                 VALUES (?1,?2,?3,1) ON CONFLICT(plan_sha256) DO NOTHING",
                params![hash, next, seq],
            )?,
            1,
        ),
    };
    ensure!(
        updated == 1,
        "T3 sweep checkpoint changed concurrently; rerun sweep step"
    );
    Ok(Checkpoint {
        next_after_claim: next.clone(),
        event_seq: seq,
        revision,
    })
}
