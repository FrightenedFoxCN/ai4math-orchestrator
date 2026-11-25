use super::*;

fn ready(store: &StateStore, limit: usize) -> Result<Vec<String>> {
    Ok(store
        .list_ready_model_dispatches(REPO, BASE, MODEL, limit)?
        .into_iter()
        .map(|r| r.task_id)
        .collect())
}

#[test]
fn ready_snapshot_is_readonly_scoped_ordered_and_rechecks_at_dispatch() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("ready.sqlite3");
    let mut store = StateStore::open(&path)?;
    fixture(&mut store)?;
    for id in [
        "alpha",
        "zulu",
        "other-base",
        "other-model",
        "blocked",
        "cancelled",
        "retrying",
    ] {
        enroll(
            &mut store,
            id,
            TaskTrack::Infrastructure,
            "lean4",
            if id == "blocked" { &[ID] } else { &[] },
        )?;
    }
    store.approve_model_dispatch(new("blocked"), ACTOR)?;
    store.approve_model_dispatch(new("retrying"), ACTOR)?;
    let retry = store.get_model_dispatch("retrying")?.unwrap();
    store.db.execute("UPDATE scheduler_task_retries SET state='backoff',attempt_count=1,
        retry_at_ms=?1,last_failure_event_seq=?2 WHERE task_seq=(SELECT queue_seq FROM scheduler_tasks WHERE task_id='retrying')",
        params![now_ms(&store.db)? + 60_000, retry.event_seq])?;
    store.approve_model_dispatch(
        NewModelDispatchApproval {
            base_commit: OTHER_BASE,
            ..new("other-base")
        },
        ACTOR,
    )?;
    store.approve_model_dispatch(
        NewModelDispatchApproval {
            model: OTHER_MODEL,
            ..new("other-model")
        },
        ACTOR,
    )?;
    store.approve_model_dispatch(new("cancelled"), ACTOR)?;
    store.cancel_model_dispatch("cancelled", ACTOR)?;
    store.approve_model_dispatch(new("zulu"), ACTOR)?;
    store.approve_model_dispatch(new("alpha"), ACTOR)?;
    let ro = StateStore::open_readonly(&path)?;
    let at = ro.status()?.last_event_seq;
    assert_eq!(ready(&ro, 1)?, ["zulu"]);
    assert_eq!(ready(&ro, 64)?, ["zulu", "alpha"]);
    assert_eq!(ready(&ro, 64)?, ["zulu", "alpha"]);
    assert_eq!(ro.status()?.last_event_seq, at);
    assert_eq!(ro.task_scheduler_status()?.infrastructure.active, 0);
    assert!(ready(&ro, 0).is_err());
    assert!(ready(&ro, 65).is_err());
    assert!(
        ro.list_ready_model_dispatches("/other", BASE, MODEL, 3)?
            .is_empty()
    );
    assert_eq!(
        ro.list_ready_model_dispatches(REPO, OTHER_BASE, MODEL, 3)?[0].task_id,
        "other-base"
    );
    assert_eq!(
        ro.list_ready_model_dispatches(REPO, BASE, OTHER_MODEL, 3)?[0].task_id,
        "other-model"
    );

    store.db.execute(
        "INSERT INTO promotion_intent VALUES(1,?1,?2,?3,?4,?5,?6,?7)",
        params![REPO, CLAIM, BASE, OTHER_BASE, MODULE, "demo", "fixture"],
    )?;
    assert!(ready(&ro, 5)?.is_empty());
    assert!(
        store
            .lease_and_dispatch_model_once("zulu", REPO, BASE, GRACE, ACTOR)
            .is_err()
    );
    store.db.execute("DELETE FROM promotion_intent", [])?;
    store.set_local_trust(CLAIM, Trust::L3, ACTOR)?;
    assert!(ready(&ro, 5)?.is_empty());
    assert!(
        store
            .lease_and_dispatch_model_once("zulu", REPO, BASE, GRACE, ACTOR)
            .is_err()
    );
    store.set_local_trust(CLAIM, Trust::L0, ACTOR)?;

    for id in ["native-1", "native-2", "native-3", "native-4"] {
        enroll(&mut store, id, TaskTrack::Infrastructure, "lean4", &[])?;
    }
    // Fill infrastructure capacity with ordinary tasks, including the unapproved fixture.
    let leases: Vec<_> = (0..4)
        .map(|_| {
            store
                .claim_task(Some(TaskTrack::Infrastructure), GRACE, ACTOR)
                .map(Option::unwrap)
        })
        .collect::<Result<_>>()?;
    assert!(ready(&ro, 5)?.is_empty());
    assert!(
        store
            .lease_and_dispatch_model_once("zulu", REPO, BASE, GRACE, ACTOR)
            .is_err()
    );
    store.complete_task(&leases[3], ACTOR)?;
    assert_eq!(ready(&ro, 5)?, ["zulu", "alpha"]);
    store.cancel_model_dispatch("zulu", ACTOR)?;
    assert!(
        store
            .lease_and_dispatch_model_once("zulu", REPO, BASE, GRACE, ACTOR)
            .is_err()
    );
    assert_eq!(ready(&ro, 5)?, ["alpha"]);
    Ok(())
}

#[test]
fn bounded_restart_reconcile_skips_unmatched_rows_before_limit_and_keeps_old_base() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("reconcile.sqlite3");
    let mut store = StateStore::open(&path)?;
    fixture(&mut store)?;
    for id in ["invalid", "other-model", "old-base", "valid"] {
        enroll(&mut store, id, TaskTrack::Infrastructure, "lean4", &[])?;
    }
    store.approve_model_dispatch(new(ID), ACTOR)?;
    let (unmatched, abandoned) = send(&mut store)?;
    assert!(store.release_model_dispatch_lease(&abandoned)?);
    for (id, base, model) in [
        ("invalid", BASE, MODEL),
        ("other-model", BASE, OTHER_MODEL),
        ("old-base", OTHER_BASE, MODEL),
        ("valid", BASE, MODEL),
    ] {
        store.approve_model_dispatch(
            NewModelDispatchApproval {
                base_commit: base,
                model,
                ..new(id)
            },
            ACTOR,
        )?;
        let (_, lease) = store.lease_and_dispatch_model_once(id, REPO, base, GRACE, ACTOR)?;
        store.finish_task_run(&lease, outcome(base), ACTOR)?;
        let durable = store.task_candidate(id)?.unwrap();
        let before = store.status()?.last_event_seq;
        assert!(store.cancel_running_model_dispatch(id, ACTOR).is_err());
        assert_eq!(store.task_candidate(id)?, Some(durable));
        assert_eq!(store.status()?.last_event_seq, before);
    }
    store.db.execute(
        "UPDATE scheduler_model_task_candidates SET base_commit=?1
        WHERE task_seq=(SELECT queue_seq FROM scheduler_tasks WHERE task_id='invalid')",
        [OTHER_BASE],
    )?;
    drop(store);
    let mut store = StateStore::open(&path)?;
    let before = store.status()?.last_event_seq;
    assert!(
        store
            .reconcile_model_dispatches("/wrong", MODEL, 1, ACTOR)?
            .is_empty()
    );
    assert!(
        store
            .reconcile_model_dispatches(REPO, MODEL, 65, ACTOR)
            .is_err()
    );
    assert_eq!(store.status()?.last_event_seq, before);
    let first = store.reconcile_model_dispatches(REPO, MODEL, 1, ACTOR)?;
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].task_id, "old-base");
    assert_eq!(first[0].base_commit, OTHER_BASE);
    assert_eq!(first[0].state, ModelDispatchState::Recorded);
    let second = store.reconcile_model_dispatches(REPO, MODEL, 1, ACTOR)?;
    assert_eq!(second.len(), 1);
    assert_eq!(second[0].task_id, "valid");
    assert_eq!(second[0].event_seq, before + 2);
    assert!(
        store
            .reconcile_model_dispatches(REPO, MODEL, 1, ACTOR)?
            .is_empty()
    );
    assert_eq!(store.get_model_dispatch(ID)?, Some(unmatched));
    assert_eq!(
        store.get_model_dispatch("invalid")?.unwrap().state,
        ModelDispatchState::Dispatched
    );
    assert_eq!(
        store.get_model_dispatch("other-model")?.unwrap().state,
        ModelDispatchState::Dispatched
    );
    assert!(store.claim_task(None, GRACE, ACTOR)?.is_none());
    assert_eq!(store.status()?.last_event_seq, before + 2);
    Ok(())
}

#[test]
fn ready_and_dispatch_apply_the_same_unstarted_oracle_hold_gate() -> Result<()> {
    let mut store = StateStore::open(":memory:")?;
    fixture(&mut store)?;
    store.approve_model_dispatch(new(ID), ACTOR)?;
    assert_eq!(ready(&store, 1)?, [ID]);

    // The public Oracle API only places holds on exploration tasks. Model a
    // retained hold in SQL to check the shared scheduler predicate directly.
    let observation = "b".repeat(64);
    let hold = "c".repeat(64);
    store.write(|tx| {
        let recorded = event(tx, ACTOR, "ORACLE_OBSERVATION_RECORDED", "claim", CLAIM, json!({}))?;
        tx.execute("INSERT INTO oracle_observations VALUES
            (?1,?1,?2,?3,?4,'lean4','Fixture.lean#demo','test',?1,?1,?1,?1,'{}',?5)",
            params![observation, CLAIM, BASE, REPO, recorded])?;
        let held = event(tx, ACTOR, "ORACLE_TASK_HELD", "task", ID, json!({}))?;
        let now = now_ms(tx)?;
        tx.execute("INSERT INTO scheduler_oracle_holds(id,task_seq,observation_id,created_at_ms,expires_at_ms,event_seq)
            SELECT ?1,queue_seq,?2,?3,?4,?5 FROM scheduler_tasks WHERE task_id=?6",
            params![hold, observation, now, now + 30_000, held, ID])?;
        Ok(())
    })?;
    assert!(ready(&store, 1)?.is_empty());
    assert!(send(&mut store).is_err());
    store.release_oracle_task_hold(ID, &hold)?;
    assert_eq!(ready(&store, 1)?, [ID]);
    let (_, lease) = send(&mut store)?;
    assert!(store.model_dispatch_lease_is_live(&lease)?);
    Ok(())
}

#[test]
fn liveness_checks_scope_generation_module_gate_expiry_and_release() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("live.sqlite3");
    let mut store = StateStore::open(&path)?;
    fixture(&mut store)?;
    store.approve_model_dispatch(new(ID), ACTOR)?;
    let (_, lease) = send(&mut store)?;
    let ro = StateStore::open_readonly(&path)?;
    let mut foreign = StateStore::open(":memory:")?;
    fixture(&mut foreign)?;
    foreign.approve_model_dispatch(new(ID), ACTOR)?;
    let (_, foreign_lease) = send(&mut foreign)?;
    let before = ro.status()?.last_event_seq;
    assert!(ro.model_dispatch_lease_is_live(&lease)?);
    assert!(!ro.model_dispatch_lease_is_live(&foreign_lease)?);
    assert!(!foreign.model_dispatch_lease_is_live(&lease)?);
    store.db.execute(
        "UPDATE scheduler_tasks SET last_fence=last_fence+1 WHERE task_id=?1",
        [ID],
    )?;
    assert!(!ro.model_dispatch_lease_is_live(&lease)?);
    store.db.execute(
        "UPDATE scheduler_tasks SET last_fence=last_fence-1 WHERE task_id=?1",
        [ID],
    )?;
    store.db.execute("INSERT INTO circuit_breakers(breaker_id,target_definition_id,module_path,engine,state,updated_at)
        VALUES('fixture','Fixture.demo',?1,'lean4','TRIPPED','now')", [MODULE])?;
    assert!(!ro.model_dispatch_lease_is_live(&lease)?);
    store
        .db
        .execute("UPDATE circuit_breakers SET state='CLOSED'", [])?;
    store.db.execute(
        "UPDATE scheduler_tasks SET lease_expires_at_ms=1 WHERE task_id=?1",
        [ID],
    )?;
    assert!(!ro.model_dispatch_lease_is_live(&lease)?);
    store.db.execute(
        "UPDATE scheduler_tasks SET lease_expires_at_ms=?2 WHERE task_id=?1",
        params![ID, lease.expires_at_ms],
    )?;
    assert!(store.release_model_dispatch_lease(&lease)?);
    assert!(!ro.model_dispatch_lease_is_live(&lease)?);
    assert_eq!(ro.status()?.last_event_seq, before);
    Ok(())
}

#[test]
fn running_cancel_and_candidate_commit_arbitrate_without_revival() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("running.sqlite3");
    let mut store = StateStore::open(&path)?;
    fixture(&mut store)?;
    store.approve_model_dispatch(new(ID), ACTOR)?;
    let (_, lease) = send(&mut store)?;
    let mut writer = StateStore::open(&path)?;
    let gate = Arc::new(Barrier::new(2));
    let submit = {
        let gate = gate.clone();
        std::thread::spawn(move || {
            gate.wait();
            (writer.finish_task_run(&lease, outcome(BASE), ACTOR), lease)
        })
    };
    gate.wait();
    let cancelled = store.cancel_running_model_dispatch(ID, ACTOR);
    let (submitted, lease) = submit.join().unwrap();
    assert_ne!(cancelled.is_ok(), submitted.is_ok());
    let top = store.status()?.last_event_seq;
    if let Ok(cancelled) = cancelled {
        assert_eq!(cancelled.state, ModelDispatchState::Cancelled);
        assert_eq!(store.cancel_running_model_dispatch(ID, ACTOR)?, cancelled);
        assert_eq!(store.approve_model_dispatch(new(ID), ACTOR)?, cancelled);
        assert!(store.task_candidate(ID)?.is_none());
        assert_eq!(store.task_scheduler_status()?.infrastructure.active, 0);
    } else {
        let candidate = store.task_candidate(ID)?.unwrap();
        assert!(store.cancel_running_model_dispatch(ID, ACTOR).is_err());
        assert_eq!(store.task_candidate(ID)?, Some(candidate));
    }
    assert_eq!(store.status()?.last_event_seq, top);
    assert!(!store.model_dispatch_lease_is_live(&lease)?);
    assert!(store.finish_task_run(&lease, outcome(BASE), ACTOR).is_err());
    assert!(send(&mut store).is_err());
    assert!(store.claim_task(None, GRACE, ACTOR)?.is_none());
    Ok(())
}

#[test]
fn v4_upgrade_preserves_dispatch_and_enables_idempotent_running_cancel() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("v4.sqlite3");
    let mut store = StateStore::open(&path)?;
    fixture(&mut store)?;
    store.approve_model_dispatch(new(ID), ACTOR)?;
    let (sent, lease) = send(&mut store)?;
    // Re-instate the v4 trigger on a v4-marked populated ledger.
    store.db.execute_batch(
        "DROP TRIGGER model_dispatch_approval_advance_only;
        DROP INDEX idx_model_dispatch_scope_state_event;
        DELETE FROM state_migrations WHERE version=5;",
    )?;
    store.db.execute_batch(include_str!(
        "../../../../../../../schemas/model-dispatch.sql"
    ))?;
    assert!(store.cancel_running_model_dispatch(ID, ACTOR).is_err());
    assert!(store.model_dispatch_lease_is_live(&lease)?);
    let before = store.status()?.last_event_seq;
    drop(store);
    let mut store = StateStore::open(&path)?;
    assert_eq!(store.status()?.last_event_seq, before);
    assert_eq!(store.get_model_dispatch(ID)?, Some(sent));
    let cancelled = store.cancel_running_model_dispatch(ID, ACTOR)?;
    assert_eq!(cancelled.state, ModelDispatchState::Cancelled);
    assert_eq!(cancelled.event_seq, before + 1);
    assert_eq!(store.cancel_running_model_dispatch(ID, ACTOR)?, cancelled);
    assert!(!store.model_dispatch_lease_is_live(&lease)?);
    assert!(store.finish_task_run(&lease, outcome(BASE), ACTOR).is_err());
    Ok(())
}
