use super::*;
use crate::{NewClaim, NewPlan, NewTask, PlanStatus, TaskTrack};
use std::time::Duration;

const REPO: &str = "/managed/project";
const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
const Z3: &str = "2222222222222222222222222222222222222222222222222222222222222222";
const CVC5: &str = "3333333333333333333333333333333333333333333333333333333333333333";
const CLAIM: &str = "claim:gap";
const MODULE: &str = "Proofs/Gap.lean";
const TARGET: &str = "Gap.positive";
const ACTOR: Change<'_> = Change {
    actor: "test",
    git_commit: Some(COMMIT),
};
const LEASE: Duration = Duration::from_secs(60);

fn setup(path: impl AsRef<Path>) -> Result<StateStore> {
    let mut store = StateStore::open(path)?;
    store.bind_task_repository(REPO, COMMIT)?;
    store.create_claim(
        NewClaim {
            id: CLAIM,
            title: "Gap",
            informal_statement: "The gap is positive.",
            engine: "lean4",
            formal_target: Some("Proofs/Gap.lean#Gap.positive"),
            module_path: Some(MODULE),
            critical: true,
        },
        ACTOR,
    )?;
    store.create_plan(
        NewPlan {
            id: "proof-route",
            target: CLAIM,
            name: "Proof",
            track: "exploration",
            status: PlanStatus::Active,
            dependencies: &[],
        },
        ACTOR,
    )?;
    task(
        &mut store,
        "held",
        TaskTrack::Exploration,
        "lean4",
        MODULE,
        TARGET,
        &[],
    )?;
    Ok(store)
}

fn task(
    store: &mut StateStore,
    id: &str,
    track: TaskTrack,
    engine: &str,
    module_path: &str,
    target: &str,
    dependencies: &[&str],
) -> Result<()> {
    store.register_task(
        NewTask {
            id,
            track,
            engine,
            module_path,
            target,
            dependencies,
        },
        ACTOR,
    )
}

// Construct persisted readback fixtures without exposing an unsealed insert API.
// The native CLI tests exercise the actual sealed broker-to-store write path.
fn retained(
    store: &mut StateStore,
    upper: i64,
    edit: impl FnOnce(&mut Value),
    resign: bool,
) -> Result<String> {
    let query = json!({"version":1,"variables":[{"name":"x","lower":-1,"upper":upper}],
        "premises":[{"terms":[{"var":"x","coeff":1}],"relation":"ge","constant":0}],
        "conclusion":{"terms":[{"var":"x","coeff":1}],"relation":"ge","constant":1}});
    let query_sha256 = OracleQuery::parse(&serde_json::to_vec(&query)?)?.fingerprint()?;
    let approval = store.oracle_approval(REPO, CLAIM, COMMIT, &query_sha256, [Z3, CVC5])?;
    let mut body = json!({"observation_sha256":"","query_sha256":query_sha256,
        "outcome":"counterexample","query":query,
        "exact_check":{"assignments_checked":upper+2,"premises_satisfying":upper+1,"counterexample":[["x",0]]},
        "solvers":[{"engine":"z3","executable_sha256":Z3,"status":"sat"},
                   {"engine":"cvc5","executable_sha256":CVC5,"status":"sat"}]});
    let fingerprint = |body: &Value| -> Result<String> {
        let b: CounterexampleEvidenceV1 = serde_json::from_value(body.clone())?;
        digest((
            "ai4math:bounded-oracle:v1",
            b.query_sha256,
            b.solvers,
            b.outcome,
            b.exact_check,
        ))
    };
    let original_hash = fingerprint(&body)?;
    edit(&mut body);
    let obs_hash = if resign {
        fingerprint(&body)?
    } else {
        original_hash
    };
    body["observation_sha256"] = json!(obs_hash);
    let id = record_id(&approval.approval_sha256, &obs_hash)?;
    store.write(|tx| {
        let seq = event(
            tx,
            Change {
                actor: "broker",
                git_commit: Some(COMMIT),
            },
            RECORDED,
            "claim",
            CLAIM,
            json!({"id":id,"approval_sha256":approval.approval_sha256,"observation_sha256":obs_hash,
                "query_sha256":approval.query_sha256,"outcome":body["outcome"]}),
        )?;
        tx.execute(
            "INSERT INTO oracle_observations (
            id,approval_sha256,claim_id,git_commit,repo,engine,formal_target,informal_statement,
            query_sha256,z3_sha256,cvc5_sha256,observation_sha256,observation_json,event_seq)
            VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
            params![
                id,
                approval.approval_sha256,
                CLAIM,
                COMMIT,
                REPO,
                approval.engine,
                approval.formal_target,
                approval.informal_statement,
                query_sha256,
                Z3,
                CVC5,
                obs_hash,
                body.to_string(),
                seq
            ],
        )?;
        Ok(id)
    })
}

#[test]
fn hold_is_one_task_only_and_both_claim_paths_respect_it() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("state.sqlite");
    let mut store = setup(&path)?;
    task(
        &mut store,
        "peer",
        TaskTrack::Exploration,
        "lean4",
        MODULE,
        TARGET,
        &[],
    )?;
    task(
        &mut store,
        "build",
        TaskTrack::Infrastructure,
        "lean4",
        MODULE,
        TARGET,
        &[],
    )?;
    task(
        &mut store,
        "dependent",
        TaskTrack::Exploration,
        "lean4",
        MODULE,
        TARGET,
        &["held"],
    )?;
    // An existing lease on a different task remains valid even with the same tuple.
    let build = store
        .claim_task(Some(TaskTrack::Infrastructure), LEASE, ACTOR)?
        .unwrap();
    let obs = retained(&mut store, 2, |_| {}, false)?;
    let claim = serde_json::to_value(store.claim(CLAIM)?)?;
    let plan = store.plan_state("proof-route")?;
    let seq = store.status()?.last_event_seq;
    let hold = store.hold_task_for_oracle(REPO, COMMIT, "held", &obs, 3600)?;
    assert_eq!(store.status()?.last_event_seq, seq + 1);
    assert_eq!(hold.expires_at - hold.created_at, 3_600_000);
    assert!(hold.active);
    assert_eq!(store.oracle_task_hold("held")?, Some(hold.clone()));
    assert_eq!(
        store.hold_task_for_oracle(REPO, COMMIT, "held", &obs, 1)?,
        hold
    );
    assert_eq!(store.status()?.last_event_seq, seq + 1);

    let readonly = StateStore::open_readonly(&path)?;
    let status = readonly.task_scheduler_status()?;
    assert_eq!(readonly.oracle_task_hold("held")?, Some(hold.clone()));
    assert_eq!(status.exploration.held_for_oracle, 1);
    assert_eq!(status.exploration.ready, 1);
    assert_eq!(status.exploration.dependency_blocked, 1);
    assert_eq!(status.exploration.breaker_blocked, 0);
    let status_json = serde_json::to_value(status)?;
    assert_eq!(status_json["exploration"]["held_for_oracle"], json!(1));
    assert!(
        status_json["infrastructure"]
            .get("held_for_oracle")
            .is_none()
    );

    assert!(
        store
            .claim_expected_task(Some(TaskTrack::Exploration), "held", LEASE, ACTOR)?
            .is_none()
    );
    let peer = store
        .claim_task(Some(TaskTrack::Exploration), LEASE, ACTOR)?
        .unwrap();
    assert_eq!(peer.id, "peer");
    store.complete_task(&build, ACTOR)?;
    store.complete_task(&peer, ACTOR)?;
    assert!(store.claim_task(None, LEASE, ACTOR)?.is_none());
    let released = store.release_oracle_task_hold("held", &hold.id)?;
    assert!(!released.active && released.released_at.is_some());
    assert_eq!(serde_json::to_value(store.claim(CLAIM)?)?, claim);
    assert_eq!(store.plan_state("proof-route")?, plan);
    let lease = store
        .claim_expected_task(None, "held", LEASE, ACTOR)?
        .unwrap();
    store.complete_task(&lease, ACTOR)?;
    assert_eq!(
        store.claim_task(None, LEASE, ACTOR)?.unwrap().id,
        "dependent"
    );
    Ok(())
}

#[test]
fn release_uses_latest_generation_and_expiry_never_renews() -> Result<()> {
    let mut store = setup(":memory:")?;
    let obs = retained(&mut store, 2, |_| {}, false)?;
    let next = retained(&mut store, 3, |_| {}, false)?;
    let later = retained(&mut store, 4, |_| {}, false)?;
    for seconds in [0, 3601, u64::MAX] {
        assert!(
            store
                .hold_task_for_oracle(REPO, COMMIT, "held", &obs, seconds)
                .is_err()
        );
    }
    let first = store.hold_task_for_oracle(REPO, COMMIT, "held", &obs, 300)?;
    assert!(
        store
            .hold_task_for_oracle(REPO, COMMIT, "held", &next, 300)
            .is_err()
    );
    let released = store.release_oracle_task_hold("held", &first.id)?;
    let seq = store.status()?.last_event_seq;
    assert_eq!(store.release_oracle_task_hold("held", &first.id)?, released);
    assert_eq!(
        store.hold_task_for_oracle(REPO, COMMIT, "held", &obs, 1)?,
        released
    );
    assert_eq!(store.status()?.last_event_seq, seq);

    // Add a historical generation with a past deadline to exercise real-clock
    // expiry without sleeping, weakening triggers, or changing the system clock.
    store.write(|tx| {
        let seq = event(tx,ACTOR,crate::state::tasks::HELD,"task","held",json!({}))?;
        tx.execute("INSERT INTO scheduler_oracle_holds(id,task_seq,observation_id,created_at_ms,expires_at_ms,event_seq)
                    SELECT ?1,queue_seq,?2,0,1000,?3 FROM scheduler_tasks WHERE task_id='held'",
                   params![Z3,next,seq])?;
        Ok(())
    })?;
    let seq = store.status()?.last_event_seq;
    let expired = store.oracle_task_hold("held")?.unwrap();
    assert!(!expired.active && expired.released_at.is_none());
    assert_eq!(
        store.hold_task_for_oracle(REPO, COMMIT, "held", &next, 300)?,
        expired
    );
    assert_eq!(
        store.release_oracle_task_hold("held", &expired.id)?,
        expired
    );
    assert_eq!(store.status()?.last_event_seq, seq);
    assert_eq!(store.task_scheduler_status()?.exploration.ready, 1);
    assert_eq!(
        store.task_scheduler_status()?.exploration.held_for_oracle,
        0
    );

    let active = store.hold_task_for_oracle(REPO, COMMIT, "held", &later, 300)?;
    assert_ne!(active.id, first.id);
    assert!(store.release_oracle_task_hold("held", &first.id).is_err());
    assert!(store.release_oracle_task_hold("held", &expired.id).is_err());
    assert!(store.release_oracle_task_hold("wrong", &active.id).is_err());
    assert_eq!(store.oracle_task_hold("held")?, Some(active));
    for sql in [
        "UPDATE scheduler_oracle_holds SET expires_at_ms=expires_at_ms+1",
        "DELETE FROM scheduler_oracle_holds",
        "INSERT OR REPLACE INTO scheduler_oracle_holds SELECT * FROM scheduler_oracle_holds",
    ] {
        assert!(
            store.db.execute(sql, []).is_err(),
            "history rewrite allowed: {sql}"
        );
    }
    for kind in [crate::state::tasks::HELD, crate::state::tasks::RELEASED] {
        assert!(
            store
                .append_event(ACTOR, kind, "task", "held", json!({}))
                .is_err()
        );
    }
    Ok(())
}

#[test]
fn rejects_wrong_evidence_task_and_current_context_without_writes() -> Result<()> {
    let mut store = setup(":memory:")?;
    let valid = retained(&mut store, 2, |_| {}, false)?;
    let bad: [(fn(&mut Value), bool); 10] = [
        (
            |v| v["outcome"] = json!("no_counterexample_within_bounds"),
            true,
        ),
        (|v| v["outcome"] = json!("disagreement"), true),
        (|v| v["outcome"] = json!("unavailable"), true),
        (|v| v["solvers"][1]["status"] = json!("unsat"), true),
        (|v| v["solvers"][1]["executable_sha256"] = json!(Z3), true),
        (|v| v["exact_check"]["counterexample"] = Value::Null, false),
        (
            |v| v["exact_check"]["counterexample"] = json!([["x", -1]]),
            true,
        ), // premise false
        (
            |v| v["exact_check"]["counterexample"] = json!([["x", 1]]),
            true,
        ), // conclusion true
        (
            |v| v["exact_check"]["counterexample"] = json!([["x", -2]]),
            true,
        ), // outside domain
        (
            |v| v["exact_check"]["premises_satisfying"] = json!(1),
            false,
        ), // bad fingerprint
    ];
    for (i, (edit, resign)) in bad.into_iter().enumerate() {
        let id = retained(&mut store, 3 + i as i64, edit, resign)?;
        let seq = store.status()?.last_event_seq;
        assert!(
            store
                .hold_task_for_oracle(REPO, COMMIT, "held", &id, 1)
                .is_err(),
            "accepted bad case {i}"
        );
        assert_eq!(store.status()?.last_event_seq, seq);
    }
    for (repo, commit) in [
        ("/other/project", COMMIT),
        (REPO, "abcdef0123456789abcdef0123456789abcdef01"),
    ] {
        assert!(
            store
                .hold_task_for_oracle(repo, commit, "held", &valid, 1)
                .is_err()
        );
    }
    for (id, track, engine, module, target) in [
        ("build", TaskTrack::Infrastructure, "lean4", MODULE, TARGET),
        ("engine", TaskTrack::Exploration, "agda", MODULE, TARGET),
        (
            "module",
            TaskTrack::Exploration,
            "lean4",
            "Proofs/Other.lean",
            TARGET,
        ),
        (
            "target",
            TaskTrack::Exploration,
            "lean4",
            MODULE,
            "Gap.other",
        ),
    ] {
        task(&mut store, id, track, engine, module, target, &[])?;
        assert!(
            store
                .hold_task_for_oracle(REPO, COMMIT, id, &valid, 1)
                .is_err()
        );
    }
    let seq = store.status()?.last_event_seq;
    for (trust, stale, effective_stale) in [("L2b", 0, 0), ("L5", 0, 0), ("L0", 1, 0), ("L0", 0, 1)]
    {
        store.db.execute("UPDATE claims SET local_trust_level=?1, stale=?2, effective_stale=?3 WHERE claim_id=?4",
            params![trust,stale,effective_stale,CLAIM])?;
        assert!(
            store
                .hold_task_for_oracle(REPO, COMMIT, "held", &valid, 1)
                .is_err()
        );
    }
    store.db.execute(
        "UPDATE claims SET effective_stale=0, informal_statement='Changed' WHERE claim_id=?1",
        [CLAIM],
    )?;
    assert!(
        store
            .hold_task_for_oracle(REPO, COMMIT, "held", &valid, 1)
            .is_err()
    );
    store.db.execute(
        "UPDATE claims SET informal_statement='The gap is positive.' WHERE claim_id=?1",
        [CLAIM],
    )?;
    store.db.execute(
        "INSERT INTO promotion_intent VALUES(1,?1,?2,?3,?3,'Proofs/Gap.lean','positive','test')",
        params![REPO, CLAIM, COMMIT],
    )?;
    assert!(
        store
            .hold_task_for_oracle(REPO, COMMIT, "held", &valid, 1)
            .is_err()
    );
    store.db.execute("DELETE FROM promotion_intent", [])?;
    store.db.execute(
        "INSERT INTO pivot_resume_intent VALUES(1,'ADR-test','{}',?1)",
        [COMMIT],
    )?;
    assert!(
        store
            .hold_task_for_oracle(REPO, COMMIT, "held", &valid, 1)
            .is_err()
    );
    store.db.execute("DELETE FROM pivot_resume_intent", [])?;
    assert!(store.oracle_task_hold("held")?.is_none());
    assert_eq!(store.status()?.last_event_seq, seq);

    let lease = store
        .claim_task(Some(TaskTrack::Exploration), LEASE, ACTOR)?
        .unwrap();
    assert!(
        store
            .hold_task_for_oracle(REPO, COMMIT, "held", &valid, 1)
            .is_err()
    );
    store.finish_task_run(&lease, crate::TaskRunOutcome::Failed, ACTOR)?;
    assert!(
        store
            .hold_task_for_oracle(REPO, COMMIT, "held", &valid, 1)
            .is_err()
    );
    let lease = store
        .claim_task(Some(TaskTrack::Exploration), LEASE, ACTOR)?
        .unwrap();
    store.complete_task(&lease, ACTOR)?;
    assert!(
        store
            .hold_task_for_oracle(REPO, COMMIT, "held", &valid, 1)
            .is_err()
    );
    Ok(())
}

#[test]
fn leasing_and_holding_the_same_task_are_serialized() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("race.sqlite");
    let mut store = setup(&path)?;
    let obs = retained(&mut store, 2, |_| {}, false)?;
    let mut peer = StateStore::open(&path)?;
    let barrier = std::sync::Barrier::new(2);
    let (hold, lease) = std::thread::scope(|scope| {
        let holder = scope.spawn(|| {
            barrier.wait();
            store.hold_task_for_oracle(REPO, COMMIT, "held", &obs, 60)
        });
        barrier.wait();
        let lease = peer.claim_task(None, LEASE, ACTOR);
        (holder.join().unwrap(), lease)
    });
    let lease = lease?;
    assert_ne!(hold.is_ok(), lease.is_some());
    if let Some(lease) = lease {
        assert!(peer.oracle_task_hold("held")?.is_none());
        peer.complete_task(&lease, ACTOR)?;
    } else {
        assert_eq!(peer.oracle_task_hold("held")?, Some(hold?));
    }
    Ok(())
}

#[test]
fn readonly_pre_hold_ledger_preserves_projection() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("state.sqlite");
    let store = setup(&path)?;
    let before = serde_json::to_value(store.task_scheduler_status()?)?;
    store.db.execute("DROP TABLE scheduler_oracle_holds", [])?;
    let readonly = StateStore::open_readonly(&path)?;
    assert_eq!(readonly.oracle_task_hold("held")?, None);
    assert_eq!(
        serde_json::to_value(readonly.task_scheduler_status()?)?,
        before
    );
    assert!(!readonly.db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='scheduler_oracle_holds')",
        [],
        |r| r.get::<_, bool>(0)
    )?);
    Ok(())
}
