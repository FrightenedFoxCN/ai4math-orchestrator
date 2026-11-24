use super::*;
use crate::{NewClaim, NewTask, TaskTrack::*};
use std::time::Duration;

const ACTOR: Change<'static> = Change {
    actor: "test",
    git_commit: None,
};
const REPO: &str = "/managed/project";
const COMMIT: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const GATED_MODULE: &str = "Proofs/Gated.lean";
const INFRA_MODULE: &str = "Proofs/Infra.lean";

fn tick(store: &mut StateStore, now: i64, reset_pending: bool) -> Result<Option<i64>> {
    store.write(|tx| reconcile_at(tx, reset_pending, now))
}

fn episodes(store: &StateStore) -> Result<Vec<i64>> {
    Ok(store
        .events_after(0, 100)?
        .into_iter()
        .filter(|e| e.event_type == ALL_TRACKS_BLOCKED)
        .map(|e| e.event_seq)
        .collect())
}

fn rows(store: &StateStore) -> Result<i64> {
    Ok(store
        .db
        .query_row("SELECT count(*) FROM scheduler_starvation", [], |r| {
            r.get(0)
        })?)
}

fn trip(store: &mut StateStore, id: &str) -> Result<()> {
    store.register_tripped_breaker(
        REPO,
        COMMIT,
        id,
        "Gated.root",
        GATED_MODULE,
        "lean4",
        "definition incident",
        false,
    )?;
    Ok(())
}

fn seed_exploration(store: &mut StateStore) -> Result<()> {
    store.register_task(
        NewTask {
            id: "explore",
            track: Exploration,
            engine: "lean4",
            module_path: GATED_MODULE,
            target: "Gated.root",
            dependencies: &[],
        },
        ACTOR,
    )?;
    trip(store, "gate")
}

fn seed_starvation(store: &mut StateStore) -> Result<()> {
    seed_exploration(store)?;
    store.register_task(
        NewTask {
            id: "infra",
            track: Infrastructure,
            engine: "lean4",
            module_path: INFRA_MODULE,
            target: "Infra.work",
            dependencies: &["explore"],
        },
        ACTOR,
    )
}

#[test]
fn empty_and_one_track_idle_do_not_write_or_alarm() -> Result<()> {
    let mut store = StateStore::open(":memory:")?;
    let changes = |s: &StateStore| {
        s.db.query_row("SELECT total_changes()", [], |r| r.get::<_, i64>(0))
    };
    let before = changes(&store)?;
    assert_eq!(store.reconcile_scheduler_starvation(true)?, None);
    assert_eq!(tick(&mut store, 4000, false)?, None);
    assert_eq!(changes(&store)?, before);
    seed_exploration(&mut store)?;
    let before = changes(&store)?;
    assert_eq!(tick(&mut store, 8000, false)?, None);
    assert_eq!(tick(&mut store, 12000, false)?, None);
    assert_eq!(changes(&store)?, before);
    assert_eq!(rows(&store)?, 0);
    assert!(episodes(&store)?.is_empty());
    Ok(())
}

#[test]
fn either_ready_or_active_task_prevents_structural_starvation() -> Result<()> {
    let mut store = StateStore::open(":memory:")?;
    seed_starvation(&mut store)?;
    assert_eq!(tick(&mut store, 1000, false)?, None);
    assert_eq!(rows(&store)?, 1);
    store.register_task(
        NewTask {
            id: "progress",
            track: Infrastructure,
            engine: "lean4",
            module_path: INFRA_MODULE,
            target: "Infra.progress",
            dependencies: &[],
        },
        ACTOR,
    )?;
    assert_eq!(tick(&mut store, 4000, false)?, None);
    assert_eq!(rows(&store)?, 0);
    let live = store
        .claim_task(Some(Infrastructure), Duration::from_secs(60), ACTOR)?
        .unwrap();
    let live_at = live.expires_at_ms - 60000;
    assert_eq!(tick(&mut store, live_at, false)?, None);
    assert_eq!(tick(&mut store, live_at + 3000, false)?, None);
    assert_eq!(rows(&store)?, 0);
    store.complete_task(&live, ACTOR)?;
    assert_eq!(tick(&mut store, live_at + 3001, false)?, None);
    assert!(tick(&mut store, live_at + 6001, false)?.is_some());
    Ok(())
}

#[test]
fn debounce_restarts_on_startup_new_ledger_evidence_and_clock_rewind() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("state.sqlite");
    let mut store = StateStore::open(&path)?;
    seed_starvation(&mut store)?;
    assert_eq!(tick(&mut store, 1000, true)?, None);
    assert_eq!(tick(&mut store, 1000, false)?, None);
    assert_eq!(tick(&mut store, 3999, false)?, None);
    drop(store);

    let mut store = StateStore::open(&path)?;
    assert_eq!(rows(&store)?, 1, "pending anchor persisted");
    assert_eq!(
        tick(&mut store, 8000, true)?,
        None,
        "downtime is not debounce"
    );
    store.append_event(ACTOR, "OTHER_EVENT", "test", "watermark", json!({}))?;
    assert_eq!(
        tick(&mut store, 11000, false)?,
        None,
        "even unchanged classifications require a quiet ledger"
    );
    assert_eq!(
        tick(&mut store, 10999, false)?,
        None,
        "clock rewound behind the anchor"
    );
    assert_eq!(tick(&mut store, 13998, false)?, None);
    let seq = tick(&mut store, 13999, false)?.unwrap();
    assert_eq!(episodes(&store)?, [seq]);
    let facts = store.briefing_for_alerts()?;
    let [alarm] = facts.alert_sources.as_slice() else {
        panic!("expected one P0 source")
    };
    assert_eq!(alarm.event_seq, seq);
    assert_eq!(alarm.entity_type, "scheduler");
    assert_eq!(alarm.entity_id, "scheduler");
    assert_eq!(alarm.actor, "daemon:scheduler");
    assert_eq!(
        alarm.payload,
        json!({"exploration_blocked":1,"infrastructure_blocked":1})
    );
    assert_eq!(
        facts.inbox.items.len(),
        1,
        "only the breaker's own item, no extra inbox mutation"
    );
    Ok(())
}

#[test]
fn emitted_latch_persists_until_observed_recovery() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("state.sqlite");
    let mut store = StateStore::open(&path)?;
    seed_starvation(&mut store)?;
    assert_eq!(tick(&mut store, 1000, true)?, None);
    let first = tick(&mut store, 4000, false)?.unwrap();
    drop(store);
    let mut store = StateStore::open(&path)?;
    assert_eq!(tick(&mut store, 8000, true)?, None);
    // An intervening healthy state not observed by this producer cannot rearm it.
    store
        .db
        .execute("UPDATE circuit_breakers SET state='CLOSED'", [])?;
    trip(&mut store, "unobserved_retrip")?;
    assert_eq!(tick(&mut store, 12000, false)?, None);
    assert_eq!(episodes(&store)?, [first]);
    store
        .db
        .execute("UPDATE circuit_breakers SET state='CLOSED'", [])?;
    assert_eq!(tick(&mut store, 16000, false)?, None);
    assert_eq!(rows(&store)?, 0);
    trip(&mut store, "observed_retrip")?;
    assert_eq!(tick(&mut store, 20000, false)?, None);
    let second = tick(&mut store, 23000, false)?.unwrap();
    assert_eq!(episodes(&store)?, [first, second]);
    Ok(())
}

#[test]
fn pending_git_drops_debounce_but_does_not_rearm_latch() -> Result<()> {
    let mut store = StateStore::open(":memory:")?;
    seed_starvation(&mut store)?;
    store.create_claim(
        NewClaim {
            id: "claim",
            title: "Claim",
            informal_statement: "Claim holds.",
            engine: "lean4",
            formal_target: None,
            module_path: None,
            critical: false,
        },
        ACTOR,
    )?;
    assert_eq!(tick(&mut store, 1000, false)?, None);
    store.db.execute(
        "INSERT INTO promotion_intent VALUES (1,?1,'claim',?2,?2,?3,'Gated.root','test')",
        params![REPO, COMMIT, GATED_MODULE],
    )?;
    assert_eq!(tick(&mut store, 4000, false)?, None);
    assert_eq!(rows(&store)?, 0);
    store.db.execute("DELETE FROM promotion_intent", [])?;
    assert_eq!(tick(&mut store, 5000, false)?, None);
    store.db.execute(
        "INSERT INTO pivot_resume_intent VALUES (1,'adr','{}',?1)",
        [COMMIT],
    )?;
    assert_eq!(tick(&mut store, 8000, false)?, None);
    assert_eq!(rows(&store)?, 0);
    store.db.execute("DELETE FROM pivot_resume_intent", [])?;
    assert_eq!(tick(&mut store, 9000, false)?, None);
    let seq = tick(&mut store, 12000, false)?.unwrap();
    store.db.execute(
        "INSERT INTO pivot_resume_intent VALUES (1,'adr','{}',?1)",
        [COMMIT],
    )?;
    store
        .db
        .execute("UPDATE circuit_breakers SET state='CLOSED'", [])?;
    assert_eq!(tick(&mut store, 15000, true)?, None);
    assert_eq!(
        rows(&store)?,
        1,
        "suppressed recovery cannot clear the latch"
    );
    store.db.execute("DELETE FROM pivot_resume_intent", [])?;
    trip(&mut store, "retrip")?;
    assert_eq!(tick(&mut store, 18000, false)?, None);
    assert_eq!(episodes(&store)?, [seq]);
    Ok(())
}

#[test]
fn even_an_otherwise_breaker_blocked_task_with_active_oracle_hold_excludes_both_tracks()
-> Result<()> {
    let mut store = StateStore::open(":memory:")?;
    seed_starvation(&mut store)?;
    store.register_task(
        NewTask {
            id: "held",
            track: Exploration,
            engine: "lean4",
            module_path: GATED_MODULE,
            target: "Gated.other",
            dependencies: &[],
        },
        ACTOR,
    )?;
    assert_eq!(tick(&mut store, 1000, true)?, None);
    // Unit fixture: the Oracle subsystem already verifies approval and retained evidence.
    let id = "b".repeat(64);
    let hold = "c".repeat(64);
    store.write(|tx| {
        let recorded = event(tx, ACTOR, "ORACLE_OBSERVATION_RECORDED", "claim", "test", json!({}))?;
        tx.execute("INSERT INTO oracle_observations VALUES (?1,?1,'test',?2,?3,'lean4','Proofs/Gated.lean#Gated.other','test',?1,?1,?1,?1,'{}',?4)",
            params![id, COMMIT, REPO, recorded])?;
        let held = event(tx, ACTOR, "ORACLE_TASK_HELD", "task", "held", json!({}))?;
        tx.execute("INSERT INTO scheduler_oracle_holds(id,task_seq,observation_id,created_at_ms,expires_at_ms,event_seq)
                    SELECT ?1,queue_seq,?2,1100,7500,?3 FROM scheduler_tasks WHERE task_id='held'",
            params![hold, id, held])?;
        Ok(())
    })?;
    assert_eq!(tick(&mut store, 4000, false)?, None);
    assert_eq!(rows(&store)?, 0);
    assert_eq!(tick(&mut store, 7000, false)?, None);
    assert_eq!(rows(&store)?, 0);
    assert_eq!(tick(&mut store, 7500, false)?, None);
    assert_eq!(tick(&mut store, 10499, false)?, None);
    assert!(tick(&mut store, 10500, false)?.is_some());
    Ok(())
}

#[test]
fn generic_append_rejected_legacy_payload_still_projected_and_latch_is_atomic() -> Result<()> {
    let mut store = StateStore::open(":memory:")?;
    let legacy =
        json!({"exploration_blocked":9,"infrastructure_blocked":"untyped","detail":"not public"});
    assert!(
        store
            .append_event(
                ACTOR,
                ALL_TRACKS_BLOCKED,
                "project",
                "legacy",
                legacy.clone()
            )
            .is_err()
    );
    let legacy_seq =
        store.write(|tx| event(tx, ACTOR, ALL_TRACKS_BLOCKED, "project", "legacy", legacy))?;
    assert_eq!(
        store.briefing_for_alerts()?.alert_sources[0].payload,
        json!({"exploration_blocked":9,"payload_omitted":true})
    );

    seed_starvation(&mut store)?;
    assert_eq!(tick(&mut store, 1000, true)?, None);
    store.db.execute_batch(
        "CREATE TRIGGER reject_latch BEFORE UPDATE ON scheduler_starvation
       WHEN NEW.emitted_event_seq IS NOT NULL BEGIN SELECT RAISE(ABORT,'fixture'); END;",
    )?;
    assert!(tick(&mut store, 4000, false).is_err());
    assert_eq!(episodes(&store)?, [legacy_seq]);
    store.db.execute_batch("DROP TRIGGER reject_latch")?;
    let seq = tick(&mut store, 4000, false)?.unwrap();
    assert_eq!(episodes(&store)?, [legacy_seq, seq]);
    Ok(())
}
