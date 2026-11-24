use super::{TaskTrack::*, *};
use std::{sync::Barrier, thread};

pub(super) const ACTOR: Change<'static> = Change {
    actor: "operator:scheduler-test",
    git_commit: None,
};
pub(super) const LEASE: Duration = Duration::from_secs(60);
pub(super) const BLOCKED_MODULE: &str = "Math/Definitions.lean";
pub(super) const BASE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

pub(super) fn task<'a>(id: &'a str, track: TaskTrack, dependencies: &'a [&'a str]) -> NewTask<'a> {
    NewTask {
        id,
        track,
        engine: "lean4",
        module_path: BLOCKED_MODULE,
        target: id,
        dependencies,
    }
}

pub(super) fn take(store: &mut StateStore, track: Option<TaskTrack>) -> Result<TaskLease> {
    store
        .claim_task(track, LEASE, ACTOR)?
        .context("expected an eligible task")
}

#[test]
fn track_filtered_fifo_and_global_alternation_survive_reopen() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("scheduler.sqlite");
    let mut store = StateStore::open(&path)?;
    for (id, track) in [
        ("i1", Infrastructure),
        ("i2", Infrastructure),
        ("e1", Exploration),
        ("e2", Exploration),
        ("i3", Infrastructure),
    ] {
        store.register_task(task(id, track, &[]), ACTOR)?;
    }
    let oldest = take(&mut store, None)?;
    assert_eq!(oldest.id, "i1");
    drop(store);

    let mut store = StateStore::open(&path)?;
    assert_eq!(take(&mut store, None)?.id, "e1");
    assert_eq!(take(&mut store, None)?.id, "i2");
    assert_eq!(take(&mut store, Some(Infrastructure))?.id, "i3");
    assert!(
        store
            .claim_task(Some(Infrastructure), LEASE, ACTOR)?
            .is_none()
    );
    assert_eq!(take(&mut store, None)?.id, "e2");
    store.complete_task(&oldest, ACTOR)?;
    let snapshot = StateStore::open_readonly(&path)?.task_scheduler_status()?;
    assert_eq!(
        (
            snapshot.exploration.active,
            snapshot.infrastructure.active,
            snapshot.infrastructure.completed
        ),
        (2, 2, 1)
    );
    Ok(())
}

#[test]
fn dependencies_must_be_enrolled_distinct_and_completed() -> Result<()> {
    let mut store = StateStore::open(":memory:")?;
    store.register_task(task("a", Exploration, &[]), ACTOR)?;
    store.register_task(task("b", Exploration, &[]), ACTOR)?;
    let before = store.events_after(0, 100)?;
    for deps in [
        &["unknown"][..],
        &["a", "a"],
        &["child"],
        &["\0"],
        &["a"; DEPENDENCY_LIMIT + 1],
    ] {
        assert!(
            store
                .register_task(task("child", Infrastructure, deps), ACTOR)
                .is_err()
        );
    }
    assert!(
        store
            .register_task(task("a", Infrastructure, &[]), ACTOR)
            .is_err()
    );
    assert!(
        store
            .register_task(
                NewTask {
                    module_path: "Math/../alias.lean",
                    ..task("child", Exploration, &[])
                },
                ACTOR
            )
            .is_err()
    );
    assert_eq!(store.events_after(0, 100)?.len(), before.len());
    store.register_task(task("child", Infrastructure, &["a", "b"]), ACTOR)?;
    let a = take(&mut store, None)?;
    assert_eq!(a.id, "a");
    assert_eq!(
        store
            .task_scheduler_status()?
            .infrastructure
            .dependency_blocked,
        1
    );
    store.complete_task(&a, ACTOR)?;
    assert!(
        store
            .claim_task(Some(Infrastructure), LEASE, ACTOR)?
            .is_none()
    );
    let b = take(&mut store, None)?;
    assert_eq!(b.id, "b");
    store.complete_task(&b, ACTOR)?;
    assert_eq!(take(&mut store, Some(Infrastructure))?.id, "child");
    Ok(())
}

#[test]
fn concurrent_connections_cannot_double_claim_or_oversubscribe() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("shared.sqlite");
    let mut seed = StateStore::open(&path)?;
    for i in 0..10 {
        seed.register_task(task(&format!("e{i}"), Exploration, &[]), ACTOR)?;
        seed.register_task(task(&format!("i{i}"), Infrastructure, &[]), ACTOR)?;
    }
    let peers = (0..12)
        .map(|_| StateStore::open(&path))
        .collect::<Result<Vec<_>>>()?;
    let barrier = Barrier::new(peers.len());
    let leases = thread::scope(|s| {
        let workers = peers
            .into_iter()
            .map(|mut peer| {
                let barrier = &barrier;
                s.spawn(move || {
                    barrier.wait();
                    peer.claim_task(None, LEASE, ACTOR)
                })
            })
            .collect::<Vec<_>>();
        workers
            .into_iter()
            .map(|w| w.join().unwrap())
            .collect::<Result<Vec<_>>>()
    })?;
    let ids = leases
        .iter()
        .flatten()
        .map(|lease| lease.id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(ids.len() as i64, ACTIVE_LIMIT);
    assert_eq!(
        BTreeSet::from_iter(ids),
        BTreeSet::from(["e0", "e1", "e2", "i0", "i1", "i2"])
    );
    assert!(
        seed.claim_task(Some(Infrastructure), LEASE, ACTOR)?
            .is_none()
    );
    for lease in leases
        .iter()
        .flatten()
        .filter(|lease| lease.track == Infrastructure)
    {
        seed.complete_task(lease, ACTOR)?;
    }
    assert_eq!(take(&mut seed, Some(Exploration))?.id, "e3");
    assert!(seed.claim_task(Some(Exploration), LEASE, ACTOR)?.is_none());
    assert_eq!(take(&mut seed, Some(Infrastructure))?.id, "i3");
    let status = seed.task_scheduler_status()?;
    assert_eq!(
        (status.exploration.active, status.infrastructure.active),
        (TRACK_ACTIVE_LIMIT, 1)
    );
    Ok(())
}

fn breaker_state(store: &StateStore, id: &str, state: &str) -> Result<()> {
    // Unit fixture only. Production transitions belong to the breaker subsystem.
    store.db.execute(
        "UPDATE circuit_breakers SET state=?1 WHERE breaker_id=?2",
        params![state, id],
    )?;
    Ok(())
}

#[test]
fn tripped_and_half_open_gate_both_branches_until_every_breaker_closes() -> Result<()> {
    let mut store = StateStore::open(":memory:")?;
    store.register_task(task("branch-a", Exploration, &[]), ACTOR)?;
    store.register_task(task("branch-b", Infrastructure, &[]), ACTOR)?;
    let old = take(&mut store, Some(Exploration))?;
    let repo = tempfile::tempdir()?;
    let repo = repo.path().to_str().unwrap();
    store.register_tripped_breaker(
        repo,
        BASE,
        "breaker1",
        "Defs.root",
        BLOCKED_MODULE,
        "lean4",
        "definition incident",
        false,
    )?;
    let frozen = store
        .events_after(0, 100)?
        .into_iter()
        .find(|event| event.event_type == "TASK_LEASES_FROZEN")
        .context("breaker trip did not atomically fence lease")?;
    assert_eq!(frozen.payload["count"], 1);
    store.register_task(
        NewTask {
            module_path: "Math/Unaffected.lean",
            ..task("other-module", Exploration, &[])
        },
        ACTOR,
    )?;
    store.register_task(
        NewTask {
            engine: "agda",
            ..task("other-engine", Infrastructure, &[])
        },
        ACTOR,
    )?;
    let status = store.task_scheduler_status()?;
    // Same engine/different module and same module/different engine remain ready.
    assert_eq!(
        (status.exploration.ready, status.exploration.breaker_blocked),
        (1, 1)
    );
    assert_eq!(
        (
            status.infrastructure.ready,
            status.infrastructure.breaker_blocked
        ),
        (1, 1)
    );
    assert_eq!(take(&mut store, None)?.id, "other-engine");
    assert_eq!(take(&mut store, None)?.id, "other-module");

    for state in ["TRIPPED", "HALF_OPEN", "ARCHIVED"] {
        breaker_state(&store, "breaker1", state)?;
        assert!(store.claim_task(None, LEASE, ACTOR)?.is_none());
        assert!(
            store
                .claim_task(Some(Infrastructure), LEASE, ACTOR)?
                .is_none()
        );
        assert!(store.complete_task(&old, ACTOR).is_err());
        let status = store.task_scheduler_status()?;
        assert_eq!(
            (
                status.exploration.breaker_blocked,
                status.infrastructure.breaker_blocked
            ),
            (1, 1)
        );
    }
    store.register_tripped_breaker(
        repo,
        BASE,
        "breaker2",
        "Defs.anotherRoot",
        BLOCKED_MODULE,
        "lean4",
        "second incident",
        false,
    )?;
    breaker_state(&store, "breaker1", "CLOSED")?;
    assert!(store.claim_task(None, LEASE, ACTOR)?.is_none());
    breaker_state(&store, "breaker2", "CLOSED")?;
    assert!(
        store.complete_task(&old, ACTOR).is_err(),
        "closing never revives a frozen lease"
    );
    assert_eq!(take(&mut store, Some(Exploration))?.id, "branch-a");
    assert_eq!(take(&mut store, Some(Infrastructure))?.id, "branch-b");
    Ok(())
}

#[test]
fn completion_independently_rechecks_breaker_row() -> Result<()> {
    let mut store = StateStore::open(":memory:")?;
    store.register_task(task("a", Exploration, &[]), ACTOR)?;
    let lease = take(&mut store, None)?;
    // Simulate a breaker row already present without a trip hook to prove the
    // completion predicate independently checks current breaker state.
    store.db.execute("INSERT INTO circuit_breakers(breaker_id,target_definition_id,module_path,engine,state,updated_at)
        VALUES('fixture','Defs.name',?1,'lean4','HALF_OPEN','now')", [BLOCKED_MODULE])?;
    assert!(store.complete_task(&lease, ACTOR).is_err());
    Ok(())
}

#[test]
fn expired_reclaimed_foreign_and_replayed_completions_are_denied() -> Result<()> {
    let mut store = StateStore::open(":memory:")?;
    let mut other = StateStore::open(":memory:")?;
    for db in [&mut store, &mut other] {
        db.register_task(task("a", Exploration, &[]), ACTOR)?;
    }
    assert!(store.claim_task(None, Duration::ZERO, ACTOR).is_err());
    assert!(
        store
            .claim_task(None, MAX_LEASE + Duration::from_millis(1), ACTOR)
            .is_err()
    );
    let old = take(&mut store, None)?;
    let foreign = take(&mut other, None)?;
    assert_eq!(old.fence.generation, foreign.fence.generation);
    assert!(store.complete_task(&foreign, ACTOR).is_err());

    assert_eq!(
        store.db.execute(
            "UPDATE scheduler_tasks SET lease_expires_at_ms=0 WHERE task_id='a'",
            []
        )?,
        1
    );
    assert!(store.complete_task(&old, ACTOR).is_err());
    let expired = store.task_scheduler_status()?.exploration;
    assert_eq!((expired.active, expired.expired, expired.ready), (0, 1, 1));
    let new = take(&mut store, None)?;
    assert_eq!(new.id, old.id);
    assert!(new.fence.generation > old.fence.generation);
    assert!(store.complete_task(&old, ACTOR).is_err());
    store.complete_task(&new, ACTOR)?;
    assert!(store.complete_task(&new, ACTOR).is_err());
    assert!(store.claim_task(None, LEASE, ACTOR)?.is_none());
    assert_eq!(
        store
            .events_after(0, 100)?
            .into_iter()
            .map(|e| e.event_type)
            .collect::<Vec<_>>(),
        vec![
            "TASK_REGISTERED",
            "TASK_LEASED",
            "TASK_LEASED",
            "TASK_COMPLETED"
        ]
    );
    Ok(())
}

#[test]
fn failed_audit_event_rolls_back_task_mutation() -> Result<()> {
    let mut store = StateStore::open(":memory:")?;
    let missing_actor = Change {
        actor: "",
        git_commit: None,
    };
    assert!(
        store
            .register_task(task("a", Exploration, &[]), missing_actor)
            .is_err()
    );
    store.register_task(task("a", Exploration, &[]), ACTOR)?;
    assert!(store.claim_task(None, LEASE, missing_actor).is_err());
    let lease = take(&mut store, None)?;
    assert_eq!(lease.fence.generation, 1);
    assert!(store.complete_task(&lease, missing_actor).is_err());
    store.complete_task(&lease, ACTOR)?;
    Ok(())
}

#[test]
fn registry_cap_preserves_completed_identities() -> Result<()> {
    let mut store = StateStore::open(":memory:")?;
    store.db.execute("WITH RECURSIVE ids(n) AS (VALUES(1) UNION ALL SELECT n+1 FROM ids WHERE n<?1)
        INSERT INTO scheduler_tasks(task_id,track,engine,module_path,target,last_fence,completed_at_ms)
        SELECT 'fixture-'||n,'exploration','lean4',?2,'done',n,1 FROM ids", params![TASK_LIMIT-1, BLOCKED_MODULE])?;
    store.register_task(task("final", Infrastructure, &["fixture-1"]), ACTOR)?;
    assert!(
        store
            .register_task(task("overflow", Infrastructure, &[]), ACTOR)
            .is_err()
    );
    let status = store.task_scheduler_status()?;
    assert_eq!(status.exploration.completed, TASK_LIMIT - 1);
    assert_eq!(status.infrastructure.ready, 1);
    Ok(())
}
