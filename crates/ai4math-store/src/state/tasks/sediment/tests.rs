use super::*;
use crate::state::sediment_adaptation::tests::approved_fixture;
use crate::{NewClaim, NewTask, SedimentAdaptationApproval, TaskCandidateSource, TaskRunOutcome};
use std::sync::{Arc, Barrier};

const TTL: Duration = Duration::from_secs(60);
const CHANGE: Change<'_> = Change {
    actor: "test",
    git_commit: None,
};
const SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn lease(store: &mut StateStore, record: &SedimentAdaptationApproval) -> Result<Option<TaskLease>> {
    store.lease_sediment_adaptation(
        &record.approval_sha256,
        &record.plan.repo,
        &record.plan.reviewed_commit,
        last_seq(&store.db)?,
        TTL,
    )
}

fn task(id: &str, track: TaskTrack) -> NewTask<'_> {
    NewTask {
        id,
        track,
        engine: "lean4",
        module_path: "Other.lean",
        target: "other",
        dependencies: &[],
    }
}

fn counts(db: &Connection) -> Result<(i64, i64)> {
    Ok(db.query_row(
        "SELECT (SELECT count(*) FROM scheduler_tasks),
                            (SELECT count(*) FROM scheduler_sediment_adaptations)",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?)
}

#[test]
fn stale_and_wrong_context_fail_before_enrollment_and_link_failure_rolls_back() -> Result<()> {
    let mut store = StateStore::open(":memory:")?;
    let a = approved_fixture(&mut store)?;
    let id = StateStore::sediment_adaptation_task_id(&a.approval_sha256)?;
    assert_eq!(id, format!("adapt-{}", a.approval_sha256));
    assert!(StateStore::sediment_adaptation_task_id(&"A".repeat(64)).is_err());
    assert!(StateStore::sediment_adaptation_task_id("invalid").is_err());
    assert!(store.register_task(task(&id, TRACK), CHANGE).is_err());
    let mut dependent = task("dependent", TRACK);
    let dependencies = [id.as_str()];
    dependent.dependencies = &dependencies;
    assert!(store.register_task(dependent, CHANGE).is_err());

    let changed = store.append_event(CHANGE, "TEST_DRIFT", "task", "test", json!({}))?;
    for (repo, head, seq, error) in [
        (
            a.plan.repo.as_str(),
            a.plan.reviewed_commit.as_str(),
            a.event_seq,
            "ledger event sequence changed",
        ),
        (
            "/other",
            a.plan.reviewed_commit.as_str(),
            changed,
            "different repository or HEAD",
        ),
        (
            a.plan.repo.as_str(),
            &SHA[..40],
            changed,
            "different repository or HEAD",
        ),
    ] {
        let err = store
            .lease_sediment_adaptation(&a.approval_sha256, repo, head, seq, TTL)
            .unwrap_err();
        assert!(err.to_string().contains(error), "{err:#}");
        assert_eq!(counts(&store.db)?, (0, 0));
        assert_eq!(last_seq(&store.db)?, changed);
    }
    store.db.execute(
        "UPDATE claims SET stale=1 WHERE claim_id=?1",
        [&a.plan.target_claim],
    )?;
    assert!(
        lease(&mut store, &a)
            .unwrap_err()
            .to_string()
            .contains("stale, refuted or inactive")
    );
    store.db.execute(
        "UPDATE claims SET stale=0 WHERE claim_id=?1",
        [&a.plan.target_claim],
    )?;
    store.db.execute_batch(
        "CREATE TRIGGER reject_link BEFORE INSERT ON scheduler_sediment_adaptations
            BEGIN SELECT RAISE(ABORT, 'fixture link failure'); END;",
    )?;
    assert!(
        lease(&mut store, &a)
            .unwrap_err()
            .to_string()
            .contains("fixture link failure")
    );
    assert_eq!(counts(&store.db)?, (0, 0));
    assert_eq!(last_seq(&store.db)?, changed);
    store.db.execute_batch("DROP TRIGGER reject_link")?;

    // Old ledgers could have used this ID before the prefix was reserved.
    store.db.execute(
        "INSERT INTO scheduler_tasks(task_id,track,engine,module_path,target)
        VALUES (?1,'infrastructure','lean4',?2,?3)",
        params![id, a.plan.target_module, a.plan.target_decl],
    )?;
    assert!(
        lease(&mut store, &a)
            .unwrap_err()
            .to_string()
            .contains("collides with an existing task")
    );
    assert_eq!(counts(&store.db)?, (1, 0));
    assert_eq!(last_seq(&store.db)?, changed);
    Ok(())
}

#[test]
fn explicit_scope_reuses_fences_without_discovering_parked_rows() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let db = dir.path().join("state.sqlite");
    let mut store = StateStore::open(&db)?;
    let a = approved_fixture(&mut store)?;
    let l1 = lease(&mut store, &a)?.unwrap();
    assert_eq!(l1.track, TRACK);
    assert_eq!(l1.engine, ENGINE);
    assert_eq!(l1.module_path, a.plan.target_module);
    assert_eq!(l1.target, a.plan.target_decl);
    let registered = store.events_after(a.event_seq, 10)?;
    assert_eq!(registered.len(), 2);
    assert_eq!(registered[0].event_type, "TASK_REGISTERED");
    assert_eq!(registered[1].event_type, "TASK_LEASED");
    for e in registered {
        assert_eq!(e.entity_id, l1.id);
        assert_eq!(e.payload["approval_sha256"], a.approval_sha256);
    }
    let last_track: Option<String> =
        store
            .db
            .query_row("SELECT last_track FROM task_scheduler_control", [], |r| {
                r.get(0)
            })?;
    assert!(last_track.is_none());
    assert_eq!(store.task_scheduler_status()?.infrastructure.active, 1);
    let seq = last_seq(&store.db)?;
    let mut other = StateStore::open(&db)?;
    assert!(lease(&mut other, &a)?.is_none());
    assert_eq!(last_seq(&store.db)?, seq);

    let plan = crate::SedimentAdaptationPlan {
        expected_event_seq: seq,
        ..a.plan.clone()
    };
    let distinct = other.approve_sediment_adaptation(&plan, &plan.approval_sha256()?)?;
    assert_ne!(distinct.approval_sha256, a.approval_sha256);
    assert!(lease(&mut other, &distinct)?.is_none());
    assert_eq!(counts(&store.db)?, (1, 1));
    assert_eq!(last_seq(&store.db)?, distinct.event_seq);

    // Simulate an expired TTL without sleeping or relying on wall-time precision.
    store.db.execute(
        "UPDATE scheduler_tasks SET lease_expires_at_ms=0 WHERE task_id=?1",
        [&l1.id],
    )?;
    assert_eq!(
        store.task_scheduler_status()?.infrastructure,
        Default::default()
    );
    let read_only = StateStore::open_readonly(&db)?;
    assert_eq!(
        read_only.task_scheduler_status()?.infrastructure,
        Default::default()
    );
    store.register_task(task("ordinary", TRACK), CHANGE)?;
    assert!(
        store
            .claim_expected_task(Some(TRACK), &l1.id, TTL, CHANGE)?
            .is_none()
    );
    assert_eq!(
        store
            .claim_expected_task(Some(TRACK), "ordinary", TTL, CHANGE)?
            .unwrap()
            .id,
        "ordinary"
    );
    let l2 = lease(&mut store, &a)?.unwrap();
    assert_eq!(l2.id, l1.id);
    assert!(
        store
            .finish_task_run(&l1, TaskRunOutcome::Failed, CHANGE)
            .is_err()
    );
    store.finish_task_run(&l2, TaskRunOutcome::Failed, CHANGE)?;
    assert!(store.claim_task(None, TTL, CHANGE)?.is_none());
    assert_eq!(store.task_scheduler_status()?.infrastructure.ready, 0);
    let l3 = lease(&mut store, &a)?.unwrap();
    assert_eq!(l3.id, l1.id);
    assert!(
        store
            .finish_task_run(&l2, TaskRunOutcome::Failed, CHANGE)
            .is_err()
    );
    store.finish_task_run(&l3, TaskRunOutcome::Failed, CHANGE)?;
    assert_eq!(counts(&store.db)?, (2, 1));
    assert!(lease(&mut store, &distinct)?.is_some());
    assert_eq!(counts(&store.db)?, (3, 2));
    for sql in [
        "UPDATE scheduler_sediment_adaptations SET task_seq=task_seq",
        "DELETE FROM scheduler_sediment_adaptations",
        "INSERT OR REPLACE INTO scheduler_sediment_adaptations SELECT * FROM scheduler_sediment_adaptations",
    ] {
        assert!(
            store
                .db
                .execute(sql, [])
                .unwrap_err()
                .to_string()
                .contains("immutable")
        );
    }
    Ok(())
}

#[test]
fn concurrency_allows_one_cas_winner() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let db = dir.path().join("state.sqlite");
    let mut store = StateStore::open(&db)?;
    let a = approved_fixture(&mut store)?;
    let barrier = Arc::new(Barrier::new(2));
    let workers: Vec<_> = (0..2)
        .map(|_| {
            let mut conn = StateStore::open(&db).unwrap();
            let a = a.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                conn.lease_sediment_adaptation(
                    &a.approval_sha256,
                    &a.plan.repo,
                    &a.plan.reviewed_commit,
                    a.event_seq,
                    TTL,
                )
            })
        })
        .collect();
    let mut outcomes: Vec<_> = workers.into_iter().map(|w| w.join().unwrap()).collect();
    let winner = outcomes
        .iter()
        .position(|r| matches!(r, Ok(Some(_))))
        .unwrap();
    outcomes.remove(winner)?;
    assert!(
        outcomes
            .remove(0)
            .unwrap_err()
            .to_string()
            .contains("ledger event sequence changed")
    );
    assert_eq!(counts(&store.db)?, (1, 1));
    assert_eq!(store.events_after(a.event_seq, 10)?.len(), 2);
    Ok(())
}

#[test]
fn capacities_are_shared_and_legacy_readonly_status_still_works() -> Result<()> {
    for (exploration, infrastructure) in [(0, 4), (4, 2)] {
        let dir = tempfile::tempdir()?;
        let db = dir.path().join("state.sqlite");
        let mut store = StateStore::open(&db)?;
        let a = approved_fixture(&mut store)?;
        let mut ordinary = Vec::new();
        for (n, track) in [
            (exploration, TaskTrack::Exploration),
            (infrastructure, TRACK),
        ] {
            for i in 0..n {
                store.register_task(task(&format!("{}{i}", track.as_str()), track), CHANGE)?;
                ordinary.push(store.claim_task(Some(track), TTL, CHANGE)?.unwrap());
            }
        }
        let seq = last_seq(&store.db)?;
        assert!(lease(&mut store, &a)?.is_none());
        assert_eq!(counts(&store.db)?, (exploration + infrastructure, 0));
        assert_eq!(last_seq(&store.db)?, seq);
        let before = store.task_scheduler_status()?;
        store
            .db
            .execute_batch("DROP TABLE scheduler_sediment_adaptations")?;
        assert_eq!(
            StateStore::open_readonly(&db)?.task_scheduler_status()?,
            before
        );
        let mut store = StateStore::open(&db)?;
        assert!(link_table_exists(&store.db)?);
        store.complete_task(&ordinary.pop().unwrap(), CHANGE)?;
        assert!(lease(&mut store, &a)?.is_some());
        assert_eq!(
            store.task_scheduler_status()?.infrastructure.active,
            infrastructure
        );
    }
    Ok(())
}

#[test]
fn breaker_and_dependencies_gate_only_explicit_scopes() -> Result<()> {
    let mut store = StateStore::open(":memory:")?;
    let a = approved_fixture(&mut store)?;
    store.db.execute("INSERT INTO circuit_breakers(breaker_id,target_definition_id,module_path,engine,state,updated_at)
        VALUES ('b','b',?1,'lean4','TRIPPED','test')", [&a.plan.target_module])?;
    assert!(lease(&mut store, &a)?.is_none());
    assert_eq!(counts(&store.db)?, (0, 0));
    store
        .db
        .execute("UPDATE circuit_breakers SET state='CLOSED'", [])?;
    store.register_task(task("parent", TaskTrack::Exploration), CHANGE)?;
    let scoped = lease(&mut store, &a)?.unwrap();
    store.finish_task_run(&scoped, TaskRunOutcome::Failed, CHANGE)?;
    store.db.execute(
        "INSERT INTO scheduler_task_dependencies(task_seq,dep_seq)
        SELECT task_seq,(SELECT queue_seq FROM scheduler_tasks WHERE task_id='parent')
        FROM scheduler_sediment_adaptations",
        [],
    )?;
    assert!(lease(&mut store, &a)?.is_none());
    assert_eq!(
        store.task_scheduler_status()?.infrastructure,
        Default::default()
    );
    let parent = store
        .claim_task(Some(TaskTrack::Exploration), TTL, CHANGE)?
        .unwrap();
    store.complete_task(&parent, CHANGE)?;
    assert!(lease(&mut store, &a)?.is_some());
    Ok(())
}

#[test]
fn candidate_writeback_requires_approved_native_binding_and_changes_no_trust() -> Result<()> {
    let mut store = StateStore::open(":memory:")?;
    let a = approved_fixture(&mut store)?;
    let plan = &a.plan;
    let formal = format!("{}#{}", plan.target_module, plan.target_decl);
    store.create_claim(
        NewClaim {
            id: "other-claim",
            title: "same target",
            informal_statement: "True",
            engine: ENGINE,
            formal_target: Some(&formal),
            module_path: Some(&plan.target_module),
            critical: false,
        },
        CHANGE,
    )?;
    let l = lease(&mut store, &a)?.unwrap();
    let seq = last_seq(&store.db)?;
    assert!(store.complete_task(&l, CHANGE).is_err());
    let candidate = |base_commit, claim_id, role| TaskRunOutcome::Candidate {
        base_commit,
        claim_id,
        role,
        executable_sha256: SHA,
        result_json: r#"{"unverified":true}"#,
    };
    for outcome in [
        candidate(&SHA[..40], &plan.target_claim, "prover_worker"),
        candidate(&plan.reviewed_commit, "other-claim", "prover_worker"),
        candidate(
            &plan.reviewed_commit,
            &plan.target_claim,
            "independent_reviewer",
        ),
        TaskRunOutcome::ModelCandidate {
            base_commit: &plan.reviewed_commit,
            claim_id: &plan.target_claim,
            role: "prover_worker",
            result_json: "true",
        },
    ] {
        assert!(
            store
                .finish_task_run(&l, outcome, CHANGE)
                .unwrap_err()
                .to_string()
                .contains("native prover_worker")
        );
        assert_eq!(store.task_scheduler_status()?.infrastructure.active, 1);
        assert!(store.task_candidate(&l.id)?.is_none());
        assert_eq!(last_seq(&store.db)?, seq);
    }
    store.db.execute(
        "UPDATE claims SET stale=1 WHERE claim_id=?1",
        [&plan.target_claim],
    )?;
    assert!(
        store
            .finish_task_run(
                &l,
                candidate(&plan.reviewed_commit, &plan.target_claim, "prover_worker"),
                CHANGE
            )
            .is_err()
    );
    assert_eq!(store.task_scheduler_status()?.infrastructure.active, 1);
    store.db.execute(
        "UPDATE claims SET stale=0 WHERE claim_id=?1",
        [&plan.target_claim],
    )?;
    let claim_before = serde_json::to_value(store.claim(&plan.target_claim)?)?;
    store.finish_task_run(
        &l,
        candidate(&plan.reviewed_commit, &plan.target_claim, "prover_worker"),
        CHANGE,
    )?;
    let recorded = store.task_candidate(&l.id)?.unwrap();
    assert_eq!(recorded.source, TaskCandidateSource::NativeWorker);
    assert_eq!(recorded.base_commit, plan.reviewed_commit);
    assert_eq!(recorded.claim_id, plan.target_claim);
    assert_eq!(recorded.executable_sha256.as_deref(), Some(SHA));
    assert_eq!(
        serde_json::to_value(store.claim(&plan.target_claim)?)?,
        claim_before
    );
    assert_eq!(store.task_scheduler_status()?.infrastructure.completed, 1);
    let seq = last_seq(&store.db)?;
    assert!(lease(&mut store, &a)?.is_none());
    assert_eq!(last_seq(&store.db)?, seq);
    Ok(())
}
