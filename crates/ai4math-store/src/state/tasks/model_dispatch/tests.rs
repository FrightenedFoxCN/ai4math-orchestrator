use super::*;
use crate::state::{NewClaim, NewTask, TaskCandidateSource, TaskRunOutcome, Trust};
use std::sync::{Arc, Barrier};

const ACTOR: Change<'static> = Change {
    actor: "operator:test-model",
    git_commit: None,
};
const BASE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const OTHER_BASE: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const REPO: &str = "/managed/repo.git";
const MODULE: &str = "Fixture.lean";
const ID: &str = "model-fixture";
const CLAIM: &str = "fixture-claim";
const GRACE: Duration = Duration::from_secs(30);
const OUTPUT: &str = "{\"private_suggestion\":\"local fake output\"}";

fn new(task_id: &str) -> NewModelDispatchApproval<'_> {
    NewModelDispatchApproval {
        task_id,
        claim_id: CLAIM,
        role: "prover_worker",
        repo: REPO,
        base_commit: BASE,
        model: "deepseek:deepseek-test",
        max_wall_seconds: 60,
        max_model_calls: 8,
    }
}

fn enroll(
    store: &mut StateStore,
    id: &str,
    track: TaskTrack,
    engine: &str,
    deps: &[&str],
) -> Result<()> {
    store.register_task(
        NewTask {
            id,
            track,
            engine,
            module_path: MODULE,
            target: "demo",
            dependencies: deps,
        },
        ACTOR,
    )
}

fn fixture(store: &mut StateStore) -> Result<()> {
    enroll(store, ID, TaskTrack::Infrastructure, "lean4", &[])?;
    store.create_claim(
        NewClaim {
            id: CLAIM,
            title: "Fixture",
            informal_statement: "Not a proof",
            engine: "lean4",
            module_path: Some(MODULE),
            formal_target: Some("Fixture.lean#demo"),
            critical: false,
        },
        ACTOR,
    )?;
    store.bind_task_repository(REPO, BASE)
}

fn send(store: &mut StateStore) -> Result<(ModelDispatchApproval, TaskLease)> {
    store.lease_and_dispatch_model_once(ID, REPO, BASE, GRACE, ACTOR)
}

fn outcome(base: &str) -> TaskRunOutcome<'_> {
    TaskRunOutcome::ModelCandidate {
        base_commit: base,
        claim_id: CLAIM,
        role: "prover_worker",
        result_json: OUTPUT,
    }
}

#[test]
fn reopen_migrates_additively_and_retains_exact_binding_and_candidate() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("state.sqlite3");
    let mut store = StateStore::open(&path)?;
    fixture(&mut store)?;
    // Model a pre-v4 ledger with pre-existing tasks, Claim and audit sequence.
    store.db.execute_batch(
        "DROP TABLE scheduler_model_dispatch_approvals;
        DELETE FROM state_migrations WHERE version=4;",
    )?;
    let pre_migration = store.status()?.last_event_seq;
    drop(store);
    let ro = StateStore::open_readonly(&path)?;
    assert_eq!(ro.get_model_dispatch(ID)?, None);
    assert!(!model_dispatch_table_exists(&ro.db)?);
    assert_eq!(ro.task_scheduler_status()?.infrastructure.ready, 1);
    drop(ro);
    let mut store = StateStore::open(&path)?;
    assert_eq!(store.status()?.last_event_seq, pre_migration);
    let migrations: i64 = store.db.query_row(
        "SELECT count(*) FROM state_migrations WHERE version=4",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(migrations, 1);
    let approved = store.approve_model_dispatch(new(ID), ACTOR)?;
    assert_eq!(approved.state, ModelDispatchState::Approved);
    assert_eq!(approved.event_seq, pre_migration + 1);
    assert_eq!(store.approve_model_dispatch(new(ID), ACTOR)?, approved);
    assert_eq!(store.reconcile_model_dispatch(ID, ACTOR)?, approved);
    let (sent, lease) = send(&mut store)?;
    assert_eq!(store.reconcile_model_dispatch(ID, ACTOR)?, sent);
    assert!(
        store
            .finish_task_run(&lease, outcome(OTHER_BASE), ACTOR)
            .is_err()
    );
    assert!(
        store
            .finish_task_run(&lease, TaskRunOutcome::Failed, ACTOR)
            .is_err()
    );
    assert!(store.complete_task(&lease, ACTOR).is_err());
    store.finish_task_run(&lease, outcome(BASE), ACTOR)?;
    drop(store);
    let ro = StateStore::open_readonly(&path)?;
    assert_eq!(ro.get_model_dispatch(ID)?, Some(sent.clone()));
    assert_eq!(
        ro.task_candidate(ID)?.unwrap().source,
        TaskCandidateSource::DshModel
    );
    drop(ro);
    let mut store = StateStore::open(&path)?;
    assert!(send(&mut store).is_err());
    assert!(store.cancel_model_dispatch(ID, ACTOR).is_err());
    let recorded = store.reconcile_model_dispatch(ID, ACTOR)?;
    assert_eq!(recorded.state, ModelDispatchState::Recorded);
    assert_eq!(store.reconcile_model_dispatch(ID, ACTOR)?, recorded);
    assert_eq!(store.approve_model_dispatch(new(ID), ACTOR)?, recorded);
    let events = store.events_after(pre_migration, 100)?;
    assert_eq!(
        events
            .iter()
            .map(|e| e.event_type.as_str())
            .collect::<Vec<_>>(),
        [
            APPROVED,
            "TASK_LEASED",
            DISPATCHED,
            "TASK_COMPLETED",
            RECORDED
        ]
    );
    assert_eq!(events[2].event_seq, sent.event_seq);
    assert_eq!(events[2].git_commit.as_deref(), Some(BASE));
    assert!(
        !events
            .iter()
            .any(|e| e.payload.to_string().contains("private_suggestion"))
    );
    assert!(!serde_json::to_string(&recorded)?.contains("private_suggestion"));
    let ro = StateStore::open_readonly(&path)?;
    assert_eq!(ro.get_model_dispatch(ID)?, Some(recorded));
    store
        .db
        .execute("DELETE FROM scheduler_model_task_candidates", [])?;
    assert!(ro.get_model_dispatch(ID).is_err());
    assert!(store.reconcile_model_dispatch(ID, ACTOR).is_err());
    Ok(())
}

#[test]
fn only_one_dispatch_wins_and_lease_capacity_expires_without_requeue() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("race.sqlite3");
    let mut store = StateStore::open(&path)?;
    fixture(&mut store)?;
    store.approve_model_dispatch(new(ID), ACTOR)?;
    let stores = [StateStore::open(&path)?, StateStore::open(&path)?];
    let gate = Arc::new(Barrier::new(stores.len()));
    let threads = stores
        .into_iter()
        .map(|mut connection| {
            let gate = gate.clone();
            std::thread::spawn(move || {
                gate.wait();
                send(&mut connection)
            })
        })
        .collect::<Vec<_>>();
    let wins = threads
        .into_iter()
        .filter_map(|h| h.join().unwrap().ok())
        .collect::<Vec<_>>();
    assert_eq!(wins.len(), 1);
    let (sent, lease) = &wins[0];
    assert_eq!(store.get_model_dispatch(ID)?.as_ref(), Some(sent));
    assert_eq!(store.task_scheduler_status()?.infrastructure.active, 1);
    assert_eq!(
        store
            .events_after(0, 100)?
            .iter()
            .filter(|e| e.event_type == DISPATCHED)
            .count(),
        1
    );
    store.db.execute(
        "UPDATE scheduler_tasks SET lease_expires_at_ms=1 WHERE task_id=?1",
        [ID],
    )?;
    assert!(!store.release_model_dispatch_lease(lease)?);
    assert_eq!(store.task_scheduler_status()?.infrastructure.active, 0);
    assert_eq!(store.task_scheduler_status()?.infrastructure.ready, 0);
    assert!(store.claim_task(None, GRACE, ACTOR)?.is_none());
    assert_eq!(store.reconcile_model_dispatch(ID, ACTOR)?, *sent);
    Ok(())
}

#[test]
fn dispatch_and_cancel_have_one_winner_and_cancel_does_not_reset_approval() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("cancel.sqlite3");
    let mut store = StateStore::open(&path)?;
    fixture(&mut store)?;
    store.approve_model_dispatch(new(ID), ACTOR)?;
    let mut second = StateStore::open(&path)?;
    let gate = Arc::new(Barrier::new(2));
    let thread = {
        let gate = gate.clone();
        std::thread::spawn(move || {
            gate.wait();
            second.cancel_model_dispatch(ID, ACTOR)
        })
    };
    gate.wait();
    let sent = send(&mut store);
    let cancelled = thread.join().unwrap();
    assert_ne!(sent.is_ok(), cancelled.is_ok());
    if let Ok(cancelled) = cancelled {
        assert_eq!(store.cancel_model_dispatch(ID, ACTOR)?, cancelled);
        assert_eq!(store.approve_model_dispatch(new(ID), ACTOR)?, cancelled);
        assert!(store.claim_expected_task(None, ID, GRACE, ACTOR)?.is_none());
    } else {
        let (row, lease) = sent?;
        assert!(store.release_model_dispatch_lease(&lease)?);
        assert!(!store.release_model_dispatch_lease(&lease)?);
        assert_eq!(store.get_model_dispatch(ID)?, Some(row));
    }
    assert!(send(&mut store).is_err());
    assert!(
        store
            .approve_model_dispatch(
                NewModelDispatchApproval {
                    max_model_calls: 3,
                    ..new(ID)
                },
                ACTOR
            )
            .is_err()
    );
    assert_eq!(
        store
            .events_after(0, 100)?
            .iter()
            .filter(|e| matches!(e.event_type.as_str(), DISPATCHED | CANCELLED))
            .count(),
        1
    );
    Ok(())
}

#[test]
fn cancellation_is_a_durable_tombstone_and_live_ordinary_leases_cannot_be_approved() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("tombstone.sqlite3");
    let mut store = StateStore::open(&path)?;
    fixture(&mut store)?;
    let ordinary = store.claim_task(None, GRACE, ACTOR)?.unwrap();
    assert!(store.approve_model_dispatch(new(ID), ACTOR).is_err());
    store.db.execute(
        "UPDATE scheduler_tasks SET lease_expires_at_ms=1 WHERE task_id=?1",
        [ID],
    )?;
    store.approve_model_dispatch(
        NewModelDispatchApproval {
            max_wall_seconds: 1,
            max_model_calls: 1,
            ..new(ID)
        },
        ACTOR,
    )?;
    assert!(
        store
            .finish_task_run(&ordinary, outcome(BASE), ACTOR)
            .is_err()
    );
    let cancelled = store.cancel_model_dispatch(ID, ACTOR)?;
    assert_eq!(cancelled.state, ModelDispatchState::Cancelled);
    assert!(
        store
            .db
            .execute(
                "DELETE FROM scheduler_model_dispatch_approvals WHERE task_id=?1",
                [ID]
            )
            .is_err()
    );
    drop(store);
    let mut store = StateStore::open(&path)?;
    assert_eq!(store.get_model_dispatch(ID)?, Some(cancelled.clone()));
    assert_eq!(store.cancel_model_dispatch(ID, ACTOR)?, cancelled);
    assert_eq!(store.reconcile_model_dispatch(ID, ACTOR)?, cancelled);
    assert!(send(&mut store).is_err());
    assert!(store.claim_task(None, GRACE, ACTOR)?.is_none());
    Ok(())
}

#[test]
fn reconcile_resists_native_or_other_model_binding_and_scheduler_completion_only() -> Result<()> {
    for (base, claim, role, native) in [
        (BASE, CLAIM, "prover_worker", true),
        (OTHER_BASE, CLAIM, "prover_worker", false),
        (BASE, "other", "prover_worker", false),
        (BASE, CLAIM, "independent_reviewer", false),
    ] {
        let mut store = StateStore::open(":memory:")?;
        fixture(&mut store)?;
        store.approve_model_dispatch(new(ID), ACTOR)?;
        let (sent, _) = send(&mut store)?;
        // Simulate an out-of-protocol historical/manual write that bypassed the
        // typed outcome gate, to test the independent reconciliation check.
        store.db.execute("UPDATE scheduler_tasks SET lease_expires_at_ms=NULL,completed_at_ms=1 WHERE task_id=?1", [ID])?;
        assert_eq!(store.reconcile_model_dispatch(ID, ACTOR)?, sent);
        if native {
            store.db.execute(
                "INSERT INTO scheduler_task_candidates
                SELECT queue_seq,?2,?3,?4,?5,?6 FROM scheduler_tasks WHERE task_id=?1",
                params![ID, base, claim, role, "c".repeat(64), OUTPUT],
            )?;
        } else {
            store.db.execute(
                "INSERT INTO scheduler_model_task_candidates
                SELECT queue_seq,?2,?3,?4,?5 FROM scheduler_tasks WHERE task_id=?1",
                params![ID, base, claim, role, OUTPUT],
            )?;
        }
        assert_eq!(store.reconcile_model_dispatch(ID, ACTOR)?, sent);
        assert_eq!(store.status()?.last_event_seq, sent.event_seq);
    }
    Ok(())
}

#[test]
fn validates_approval_inputs_dispatch_gates_and_rolls_back_rejected_event() -> Result<()> {
    let mut store = StateStore::open(":memory:")?;
    fixture(&mut store)?;
    enroll(
        &mut store,
        "engineering",
        TaskTrack::Infrastructure,
        "agda",
        &[],
    )?;
    enroll(&mut store, "explore", TaskTrack::Exploration, "lean4", &[])?;
    enroll(
        &mut store,
        "dependent",
        TaskTrack::Infrastructure,
        "lean4",
        &["engineering"],
    )?;
    let top = store.status()?.last_event_seq;
    for bad in [
        new("missing"),
        new("engineering"),
        new("explore"),
        NewModelDispatchApproval {
            claim_id: "unknown",
            ..new(ID)
        },
        NewModelDispatchApproval {
            role: "explorer",
            ..new(ID)
        },
        NewModelDispatchApproval {
            repo: "/different/repo.git",
            ..new(ID)
        },
        NewModelDispatchApproval {
            base_commit: "abcdef",
            ..new(ID)
        },
        NewModelDispatchApproval {
            model: "malformed name",
            ..new(ID)
        },
        NewModelDispatchApproval {
            max_wall_seconds: 0,
            ..new(ID)
        },
        NewModelDispatchApproval {
            max_wall_seconds: 61,
            ..new(ID)
        },
        NewModelDispatchApproval {
            max_model_calls: 0,
            ..new(ID)
        },
        NewModelDispatchApproval {
            max_model_calls: 9,
            ..new(ID)
        },
    ] {
        assert!(store.approve_model_dispatch(bad, ACTOR).is_err());
    }
    assert_eq!(store.status()?.last_event_seq, top);
    assert!(
        store
            .append_event(ACTOR, DISPATCHED, "task", ID, json!({}))
            .is_err()
    );
    let dep = store.approve_model_dispatch(new("dependent"), ACTOR)?;
    assert!(
        store
            .lease_and_dispatch_model_once("dependent", REPO, BASE, GRACE, ACTOR)
            .is_err()
    );
    assert_eq!(store.get_model_dispatch("dependent")?, Some(dep));
    let approved = store.approve_model_dispatch(new(ID), ACTOR)?;
    assert!(
        store
            .lease_and_dispatch_model_once(ID, REPO, OTHER_BASE, GRACE, ACTOR)
            .is_err()
    );
    store.set_local_trust(CLAIM, Trust::L3, ACTOR)?;
    assert!(send(&mut store).is_err());
    store.set_local_trust(CLAIM, Trust::L0, ACTOR)?;
    store.db.execute("INSERT INTO circuit_breakers(breaker_id,target_definition_id,module_path,engine,state,updated_at)
        VALUES('fixture','Fixture.demo',?1,'lean4','TRIPPED','now')", [MODULE])?;
    assert!(send(&mut store).is_err());
    store.db.execute(
        "UPDATE circuit_breakers SET state='CLOSED' WHERE breaker_id='fixture'",
        [],
    )?;
    let before = store.status()?.last_event_seq;
    store.db.execute_batch(
        "CREATE TRIGGER reject_model_dispatch_audit BEFORE INSERT ON events
      WHEN NEW.event_type='MODEL_DISPATCHED' BEGIN SELECT RAISE(ABORT,'fixture rollback'); END;",
    )?;
    assert!(send(&mut store).is_err());
    assert_eq!(store.status()?.last_event_seq, before);
    assert_eq!(store.task_scheduler_status()?.infrastructure.active, 0);
    assert_eq!(store.get_model_dispatch(ID)?, Some(approved));
    store
        .db
        .execute_batch("DROP TRIGGER reject_model_dispatch_audit;")?;
    let (_, lease) = send(&mut store)?;
    assert!(store.release_model_dispatch_lease(&lease)?);
    // The older engineering task neither blocks explicit model dispatch nor gets leased by it.
    assert_eq!(
        store
            .claim_task(Some(TaskTrack::Infrastructure), GRACE, ACTOR)?
            .unwrap()
            .id,
        "engineering"
    );
    Ok(())
}
