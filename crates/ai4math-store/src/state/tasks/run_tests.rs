use super::{
    TaskTrack::*,
    runs::{DEFAULT_MAX_ATTEMPTS, INITIAL_RETRY_BACKOFF_MS},
    tests::{ACTOR, BASE, BLOCKED_MODULE, LEASE, take, task},
    *,
};
use crate::state::NewClaim;

const SHA: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const CLAIM: &str = "claim.a";
const ROLE: &str = "prover";

fn candidate(result_json: &str) -> TaskRunOutcome<'_> {
    TaskRunOutcome::Candidate {
        base_commit: BASE,
        claim_id: CLAIM,
        role: ROLE,
        executable_sha256: SHA,
        result_json,
    }
}

fn model_candidate(result_json: &str) -> TaskRunOutcome<'_> {
    TaskRunOutcome::ModelCandidate {
        base_commit: BASE,
        claim_id: CLAIM,
        role: "prover_worker",
        result_json,
    }
}

fn put_claim(store: &mut StateStore) -> Result<()> {
    store.create_claim(
        NewClaim {
            id: CLAIM,
            title: "Fixture claim",
            informal_statement: "Fixture statement",
            engine: "lean4",
            formal_target: Some(&format!("{BLOCKED_MODULE}#a")),
            module_path: Some(BLOCKED_MODULE),
            critical: false,
        },
        ACTOR,
    )
}

#[test]
fn expected_claim_matches_fifo_and_alternation_without_side_effects() -> Result<()> {
    let mut store = StateStore::open(":memory:")?;
    for (id, track) in [
        ("i0", Infrastructure),
        ("i1", Infrastructure),
        ("e0", Exploration),
    ] {
        store.register_task(task(id, track, &[]), ACTOR)?;
    }
    let before = store.task_scheduler_status()?;
    let events = store.events_after(0, 100)?.len();
    for (track, expected) in [(None, "e0"), (None, "i1"), (Some(Infrastructure), "i1")] {
        assert!(
            store
                .claim_expected_task(track, expected, LEASE, ACTOR)?
                .is_none()
        );
    }
    assert_eq!(store.task_scheduler_status()?, before);
    assert_eq!(store.events_after(0, 100)?.len(), events);
    let first = store
        .claim_expected_task(None, "i0", LEASE, ACTOR)?
        .context("expected i0")?;
    assert_eq!((first.id.as_str(), first.fence.generation), ("i0", 1));
    // i1 is oldest overall but alternation takes precedence over global FIFO.
    assert!(
        store
            .claim_expected_task(None, "i1", LEASE, ACTOR)?
            .is_none()
    );
    let next = store
        .claim_expected_task(None, "e0", LEASE, ACTOR)?
        .context("expected e0")?;
    assert_eq!((next.id.as_str(), next.fence.generation), ("e0", 2));
    assert_eq!(take(&mut store, None)?.id, "i1");
    Ok(())
}

#[test]
fn submission_grace_extends_only_the_persisted_lease_ttl() -> Result<()> {
    let mut store = StateStore::open(":memory:")?;
    store.register_task(task("model", Exploration, &[]), ACTOR)?;
    let execution = Duration::from_secs(5);
    let grace = Duration::from_secs(7);
    let before = now_ms(&store.db)?;
    let lease = store
        .claim_expected_task_with_submission_grace(
            Some(Exploration),
            "model",
            execution,
            grace,
            ACTOR,
        )?
        .context("expected model task")?;
    let after = now_ms(&store.db)?;
    let total_ms = (execution + grace).as_millis() as i64;
    assert!(lease.expires_at_ms >= before + total_ms);
    assert!(lease.expires_at_ms <= after + total_ms);
    assert_eq!(lease.fence.generation, 1);
    assert_eq!(
        store.db.query_row(
            "SELECT lease_expires_at_ms FROM scheduler_tasks WHERE task_id='model'",
            [],
            |r| r.get::<_, i64>(0),
        )?,
        lease.expires_at_ms
    );

    let events = store.events_after(0, 100)?.len();
    for (execution, grace) in [
        (Duration::ZERO, Duration::from_secs(1)),
        (MAX_LEASE, Duration::from_millis(1)),
        (Duration::from_secs(1), MAX_LEASE + Duration::from_millis(1)),
    ] {
        assert!(
            store
                .claim_expected_task_with_submission_grace(
                    Some(Exploration),
                    "model",
                    execution,
                    grace,
                    ACTOR,
                )
                .is_err()
        );
    }
    assert_eq!(store.events_after(0, 100)?.len(), events);
    store.complete_task(&lease, ACTOR)?;
    assert!(store.complete_task(&lease, ACTOR).is_err());
    Ok(())
}

#[test]
fn candidate_persists_without_changing_proof_trust_or_exposing_output() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("candidate.sqlite");
    let mut store = StateStore::open(&path)?;
    put_claim(&mut store)?;
    let trust_before = store.claim(CLAIM)?.context("missing fixture claim")?;
    store.register_task(task("a", Exploration, &[]), ACTOR)?;
    store.register_task(task("dependent", Infrastructure, &["a"]), ACTOR)?;
    let lease = take(&mut store, None)?;
    let result = r#" { "text": "untrusted-output-canary", "fake_proof": true } "#;
    assert!(store.task_candidate("a")?.is_none());
    store.finish_task_run(&lease, candidate(result), ACTOR)?;
    assert!(
        store
            .finish_task_run(&lease, candidate("false"), ACTOR)
            .is_err()
    );
    assert!(store.complete_task(&lease, ACTOR).is_err());
    assert_eq!(take(&mut store, None)?.id, "dependent");
    let after = store.claim(CLAIM)?.context("missing fixture claim")?;
    assert_eq!(before_trust(&trust_before), before_trust(&after));
    let events = store.events_after(0, 100)?;
    let completed = events
        .iter()
        .find(|e| e.event_type == "TASK_COMPLETED")
        .context("missing event")?;
    assert_eq!(completed.payload, json!({"untrusted_candidate": true}));
    assert!(!serde_json::to_string(&events)?.contains("untrusted-output-canary"));
    drop(store);

    let reopened = StateStore::open_readonly(&path)?;
    let stored = reopened
        .task_candidate("a")?
        .context("missing durable candidate")?;
    assert_eq!(stored.task_id, "a");
    assert_eq!(stored.track, Exploration);
    assert_eq!(stored.engine, "lean4");
    assert_eq!(stored.module_path, BLOCKED_MODULE);
    assert_eq!(stored.target, "a");
    assert_eq!(stored.base_commit, BASE);
    assert_eq!(stored.claim_id, CLAIM);
    assert_eq!(stored.role, ROLE);
    assert_eq!(stored.source, TaskCandidateSource::NativeWorker);
    assert_eq!(stored.executable_sha256.as_deref(), Some(SHA));
    let serialized = serde_json::to_value(&stored)?;
    assert!(serialized.get("source").is_none());
    assert_eq!(serialized["executable_sha256"], SHA);
    assert_eq!(stored.result_json, result);
    assert!(stored.recorded_at_ms > 0 && stored.recorded_at_ms < lease.expires_at_ms);
    assert!(!format!("{stored:?}").contains("untrusted-output-canary"));
    assert_eq!(reopened.task_scheduler_status()?.exploration.completed, 1);
    assert!(reopened.task_candidate("unknown")?.is_none());
    Ok(())
}

fn before_trust(claim: &crate::ClaimState) -> (crate::Trust, crate::Trust, Option<&str>) {
    (
        claim.local_trust,
        claim.effective_trust,
        claim.receipt_id.as_deref(),
    )
}

#[test]
fn failed_run_releases_for_retry_but_expired_reclaimed_and_foreign_runs_fail() -> Result<()> {
    let mut store = StateStore::open(":memory:")?;
    let mut other = StateStore::open(":memory:")?;
    for db in [&mut store, &mut other] {
        put_claim(db)?;
        db.register_task(task("a", Exploration, &[]), ACTOR)?;
    }
    let expired = take(&mut store, None)?;
    let foreign = take(&mut other, None)?;
    assert!(
        store
            .finish_task_run(&foreign, candidate("null"), ACTOR)
            .is_err()
    );
    store.db.execute(
        "UPDATE scheduler_tasks SET lease_expires_at_ms=0 WHERE task_id='a'",
        [],
    )?;
    assert!(
        store
            .finish_task_run(&expired, candidate("null"), ACTOR)
            .is_err()
    );
    assert!(
        store
            .finish_task_run(&expired, TaskRunOutcome::Failed, ACTOR)
            .is_err()
    );
    let retry = take(&mut store, None)?;
    assert!(
        store
            .finish_task_run(&expired, candidate("null"), ACTOR)
            .is_err()
    );
    assert!(
        store
            .finish_task_run(&expired, TaskRunOutcome::Failed, ACTOR)
            .is_err()
    );
    assert_eq!(store.task_scheduler_status()?.exploration.active, 1);
    store.finish_task_run(&retry, TaskRunOutcome::Failed, ACTOR)?;
    assert!(
        store
            .finish_task_run(&retry, TaskRunOutcome::Failed, ACTOR)
            .is_err()
    );
    assert!(
        store
            .finish_task_run(&retry, candidate("null"), ACTOR)
            .is_err()
    );
    let status = store.task_scheduler_status()?.exploration;
    assert_eq!(
        (
            status.active,
            status.ready,
            status.completed,
            status.expired
        ),
        (0, 0, 0, 0)
    );
    assert_eq!(status.retrying, 1);
    assert_eq!(status.quarantined, 0);
    assert!(store.task_candidate("a")?.is_none());
    let failures: Vec<_> = store
        .events_after(0, 100)?
        .into_iter()
        .filter(|e| e.event_type == "TASK_RUN_FAILED")
        .collect();
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].payload["state"], "backoff");
    assert_eq!(failures[0].payload["attempt"], 1);
    assert_eq!(failures[0].payload["max_attempts"], DEFAULT_MAX_ATTEMPTS);
    assert_eq!(failures[0].payload["backoff_ms"], INITIAL_RETRY_BACKOFF_MS);
    assert!(failures[0].payload["retry_at_ms"].as_i64().is_some());
    // Advance the durable deadline without sleeping; FIFO resumes unchanged.
    store.db.execute(
        "UPDATE scheduler_task_retries SET retry_at_ms=0 WHERE task_seq=(SELECT queue_seq FROM scheduler_tasks WHERE task_id='a')",
        [],
    )?;
    let newest = take(&mut store, None)?;
    assert_eq!(newest.id, "a");
    store.finish_task_run(&newest, candidate("null"), ACTOR)?;
    Ok(())
}

#[test]
fn explicit_expected_retry_bypasses_backoff_but_not_quarantine() -> Result<()> {
    let mut store = StateStore::open(":memory:")?;
    store.register_task(task("retry", Exploration, &[]), ACTOR)?;
    let first = take(&mut store, Some(Exploration))?;
    store.finish_task_run(&first, TaskRunOutcome::Failed, ACTOR)?;
    assert!(store.claim_task(Some(Exploration), LEASE, ACTOR)?.is_none());

    let explicit = store
        .claim_expected_task(Some(Exploration), "retry", LEASE, ACTOR)?
        .context("explicit operator retry should bypass backoff")?;
    store.finish_task_run(&explicit, TaskRunOutcome::Failed, ACTOR)?;
    let explicit = store
        .claim_expected_task(Some(Exploration), "retry", LEASE, ACTOR)?
        .context("second explicit retry should bypass backoff")?;
    store.finish_task_run(&explicit, TaskRunOutcome::Failed, ACTOR)?;
    assert!(
        store
            .claim_expected_task(Some(Exploration), "retry", LEASE, ACTOR)?
            .is_none(),
        "quarantine must also block explicit retries"
    );
    Ok(())
}

#[test]
fn failed_task_rotates_fifo_then_persists_backoff_and_quarantines() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("retry.sqlite");
    let mut store = StateStore::open(&path)?;
    store.register_task(task("bad", Exploration, &[]), ACTOR)?;
    store.register_task(task("good", Exploration, &[]), ACTOR)?;

    let first = take(&mut store, Some(Exploration))?;
    assert_eq!(first.id, "bad");
    store.finish_task_run(&first, TaskRunOutcome::Failed, ACTOR)?;
    let now = now_ms(&store.db)?;
    let retry: (String, i64, i64, i64, Option<i64>, Option<i64>, i64) = store.db.query_row(
        "SELECT state,attempt_count,max_attempts,backoff_ms,retry_at_ms,quarantined_at_ms,last_failure_event_seq
         FROM scheduler_task_retries r JOIN scheduler_tasks t ON t.queue_seq=r.task_seq WHERE t.task_id='bad'",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)),
    )?;
    assert_eq!(retry.0, "backoff");
    assert_eq!(
        (retry.1, retry.2, retry.3),
        (1, DEFAULT_MAX_ATTEMPTS, INITIAL_RETRY_BACKOFF_MS)
    );
    assert!(
        retry
            .4
            .is_some_and(|deadline| deadline > now && deadline <= now + INITIAL_RETRY_BACKOFF_MS)
    );
    assert_eq!(retry.5, None);
    assert!(retry.6 > 0);
    let status = store.task_scheduler_status()?.exploration;
    assert_eq!(
        (status.ready, status.retrying, status.quarantined),
        (1, 1, 0)
    );

    // Backoff and its audit binding survive a writable reopen and readonly use.
    drop(store);
    let readonly = StateStore::open_readonly(&path)?;
    assert_eq!(readonly.task_scheduler_status()?.exploration, status);
    let persisted: (String, i64, i64, i64, Option<i64>, Option<i64>, i64) = readonly.db.query_row(
        "SELECT state,attempt_count,max_attempts,backoff_ms,retry_at_ms,quarantined_at_ms,last_failure_event_seq
         FROM scheduler_task_retries r JOIN scheduler_tasks t ON t.queue_seq=r.task_seq WHERE t.task_id='bad'",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)),
    )?;
    assert_eq!(persisted, retry);
    drop(readonly);

    let mut store = StateStore::open(&path)?;
    // The failed FIFO head no longer blocks a normal task during backoff.
    let good = take(&mut store, Some(Exploration))?;
    assert_eq!(good.id, "good");
    store.complete_task(&good, ACTOR)?;
    assert!(store.claim_task(Some(Exploration), LEASE, ACTOR)?.is_none());

    store.db.execute(
        "UPDATE scheduler_task_retries SET retry_at_ms=0
         WHERE task_seq=(SELECT queue_seq FROM scheduler_tasks WHERE task_id='bad')",
        [],
    )?;
    let second = take(&mut store, Some(Exploration))?;
    assert_eq!(second.id, "bad");
    store.finish_task_run(&second, TaskRunOutcome::Failed, ACTOR)?;
    let second_backoff: (i64, i64) = store.db.query_row(
        "SELECT attempt_count,backoff_ms FROM scheduler_task_retries",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    assert_eq!(second_backoff, (2, INITIAL_RETRY_BACKOFF_MS * 2));

    store.db.execute(
        "UPDATE scheduler_task_retries SET retry_at_ms=0
         WHERE task_seq=(SELECT queue_seq FROM scheduler_tasks WHERE task_id='bad')",
        [],
    )?;
    let third = take(&mut store, Some(Exploration))?;
    assert_eq!(third.id, "bad");
    store.finish_task_run(&third, TaskRunOutcome::Failed, ACTOR)?;
    let exhausted: (String, i64, i64, i64, Option<i64>, Option<i64>) = store.db.query_row(
        "SELECT state,attempt_count,max_attempts,backoff_ms,retry_at_ms,quarantined_at_ms FROM scheduler_task_retries",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
    )?;
    assert_eq!(exhausted.0, "quarantined");
    assert_eq!(
        (exhausted.1, exhausted.2, exhausted.3),
        (DEFAULT_MAX_ATTEMPTS, DEFAULT_MAX_ATTEMPTS, 0)
    );
    assert_eq!(exhausted.4, None);
    assert!(exhausted.5.is_some());
    assert!(
        store
            .finish_task_run(&third, TaskRunOutcome::Failed, ACTOR)
            .is_err()
    );
    assert!(store.claim_task(Some(Exploration), LEASE, ACTOR)?.is_none());
    let status = store.task_scheduler_status()?.exploration;
    assert_eq!(
        (status.ready, status.retrying, status.quarantined),
        (0, 0, 1)
    );
    let failures: Vec<_> = store
        .events_after(0, 100)?
        .into_iter()
        .filter(|event| event.event_type == "TASK_RUN_FAILED")
        .collect();
    assert_eq!(failures.len(), 3);
    assert_eq!(failures[2].payload["state"], "quarantined");
    assert_eq!(failures[2].payload["attempt"], DEFAULT_MAX_ATTEMPTS);
    assert!(store.task_candidate("bad")?.is_none());
    Ok(())
}

#[test]
fn candidate_and_failure_recheck_breaker_even_without_freeze_hook() -> Result<()> {
    let mut store = StateStore::open(":memory:")?;
    put_claim(&mut store)?;
    store.register_task(task("a", Exploration, &[]), ACTOR)?;
    let lease = take(&mut store, None)?;
    store.db.execute("INSERT INTO circuit_breakers(breaker_id,target_definition_id,module_path,engine,state,updated_at)
        VALUES('fixture','Defs.name',?1,'lean4','TRIPPED','now')", [BLOCKED_MODULE])?;
    assert!(
        store
            .finish_task_run(&lease, candidate("null"), ACTOR)
            .is_err()
    );
    assert!(
        store
            .finish_task_run(&lease, TaskRunOutcome::Failed, ACTOR)
            .is_err()
    );
    assert!(store.task_candidate("a")?.is_none());
    assert_eq!(store.task_scheduler_status()?.exploration.active, 1);
    assert_eq!(store.events_after(0, 100)?.len(), 3);
    Ok(())
}

#[test]
fn candidate_requires_matching_runnable_claim() -> Result<()> {
    let mut store = StateStore::open(":memory:")?;
    store.register_task(task("a", Exploration, &[]), ACTOR)?;
    let lease = take(&mut store, None)?;
    for outcome in [candidate("true"), model_candidate("true")] {
        assert!(store.finish_task_run(&lease, outcome, ACTOR).is_err());
    }
    put_claim(&mut store)?;
    let before = store.events_after(0, 100)?.len();
    // Simulate changes occurring after coordinator-side checks and lease issuance.
    for (invalid, reset) in [
        ("engine='agda'", "engine='lean4'"),
        ("module_path='Math/Other.lean'", "module_path=?2"),
        ("formal_target='other'", "formal_target=module_path || '#a'"),
        ("stale=1", "stale=0"),
        ("effective_stale=1", "effective_stale=0"),
        ("effective_trust_level='L2b'", "effective_trust_level='L0'"),
        ("effective_trust_level='L3'", "effective_trust_level='L0'"),
    ] {
        store.db.execute(
            &format!("UPDATE claims SET {invalid} WHERE claim_id=?1"),
            [CLAIM],
        )?;
        for outcome in [candidate("true"), model_candidate("true")] {
            assert!(store.finish_task_run(&lease, outcome, ACTOR).is_err());
        }
        assert!(store.task_candidate("a")?.is_none());
        if reset.contains("?2") {
            store.db.execute(
                &format!("UPDATE claims SET {reset} WHERE claim_id=?1"),
                [CLAIM, BLOCKED_MODULE],
            )?;
        } else {
            store.db.execute(
                &format!("UPDATE claims SET {reset} WHERE claim_id=?1"),
                [CLAIM],
            )?;
        }
    }
    assert_eq!(store.events_after(0, 100)?.len(), before);
    assert_eq!(store.task_scheduler_status()?.exploration.active, 1);
    store.finish_task_run(&lease, candidate("true"), ACTOR)?;
    assert!(
        store
            .finish_task_run(&lease, model_candidate("true"), ACTOR)
            .is_err()
    );
    Ok(())
}

#[test]
fn malformed_metadata_oversize_result_and_audit_failure_do_not_consume_lease() -> Result<()> {
    let mut store = StateStore::open(":memory:")?;
    put_claim(&mut store)?;
    store.register_task(task("a", Exploration, &[]), ACTOR)?;
    let lease = take(&mut store, None)?;
    let at_limit = format!(" \"{}\" ", "é".repeat((32 * 1024 - 4) / 2));
    assert_eq!(at_limit.len(), 32 * 1024);
    let too_large = format!("{at_limit} ");
    for (base_commit, claim_id, role, executable_sha256, result_json) in [
        ("short-commit", CLAIM, ROLE, SHA, "true"),
        (BASE, &"x".repeat(101), ROLE, SHA, "true"),
        (BASE, CLAIM, &"x".repeat(65), SHA, "true"),
        (BASE, CLAIM, "bad role", SHA, "true"),
        (BASE, CLAIM, ROLE, "short-hash", "true"),
        (BASE, CLAIM, ROLE, SHA, "["),
        (BASE, CLAIM, ROLE, SHA, &too_large),
    ] {
        assert!(
            store
                .finish_task_run(
                    &lease,
                    TaskRunOutcome::Candidate {
                        base_commit,
                        claim_id,
                        role,
                        executable_sha256,
                        result_json,
                    },
                    ACTOR
                )
                .is_err()
        );
    }
    let bad_audit = Change {
        actor: "",
        git_commit: None,
    };
    assert!(
        store
            .finish_task_run(&lease, candidate("true"), bad_audit)
            .is_err()
    );
    assert!(
        store
            .finish_task_run(&lease, TaskRunOutcome::Failed, bad_audit)
            .is_err()
    );
    assert!(store.task_candidate("a")?.is_none());
    assert_eq!(store.events_after(0, 100)?.len(), 3);
    assert_eq!(store.task_scheduler_status()?.exploration.active, 1);
    store.finish_task_run(&lease, candidate(&at_limit), ACTOR)?;
    assert_eq!(
        store
            .task_candidate("a")?
            .context("expected candidate at bound")?
            .result_json,
        at_limit
    );
    Ok(())
}

#[test]
fn model_result_completes_once_without_creating_proof_evidence() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("model.sqlite");
    let mut store = StateStore::open(&path)?;
    put_claim(&mut store)?;
    let claim_before = serde_json::to_value(store.claim(CLAIM)?)?;
    store.register_task(task("a", Exploration, &[]), ACTOR)?;
    store.register_task(task("dependent", Infrastructure, &["a"]), ACTOR)?;
    let lease = take(&mut store, None)?;
    let result = r#" {"proof_body":"model-output-canary","receipt_id":"fake","trust":"L5"} "#;
    assert!(store.task_candidate("a")?.is_none());
    store.finish_task_run(&lease, model_candidate(result), ACTOR)?;
    for outcome in [
        model_candidate("null"),
        candidate("null"),
        TaskRunOutcome::Failed,
    ] {
        assert!(store.finish_task_run(&lease, outcome, ACTOR).is_err());
    }
    assert!(store.complete_task(&lease, ACTOR).is_err());
    assert_eq!(take(&mut store, None)?.id, "dependent");
    let events = store.events_after(0, 100)?;
    let completed: Vec<_> = events
        .iter()
        .filter(|e| e.event_type == "TASK_COMPLETED")
        .collect();
    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0].entity_id, "a");
    assert_eq!(completed[0].payload, json!({"untrusted_candidate": true}));
    assert!(!serde_json::to_string(&events)?.contains("model-output-canary"));
    drop(store);

    let store = StateStore::open_readonly(&path)?;
    assert_eq!(serde_json::to_value(store.claim(CLAIM)?)?, claim_before);
    assert_eq!(
        store.db.query_row(
            "SELECT (SELECT count(*) FROM verification_receipts),
                (SELECT count(*) FROM recorded_promotions),
                (SELECT count(*) FROM promotion_intent),
                (SELECT count(*) FROM scheduler_task_candidates),
                (SELECT count(*) FROM scheduler_model_task_candidates)",
            [],
            |row| Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?
            )),
        )?,
        (0, 0, 0, 0, 1)
    );
    let row = store.task_candidate("a")?.context("missing model result")?;
    assert_eq!(row.source, TaskCandidateSource::DshModel);
    assert_eq!(row.executable_sha256, None);
    assert_eq!(row.result_json, result);
    assert!(!format!("{row:?}").contains("model-output-canary"));
    let serialized = serde_json::to_value(row)?;
    assert_eq!(serialized["source"], "dsh_model");
    assert!(serialized.get("executable_sha256").is_none());
    assert_eq!(serialized["base_commit"], BASE);
    assert_eq!(serialized["claim_id"], CLAIM);
    assert_eq!(serialized["role"], "prover_worker");
    assert_eq!(store.task_scheduler_status()?.exploration.completed, 1);
    Ok(())
}

#[test]
fn malformed_model_result_and_audit_failure_leave_live_lease_untouched() -> Result<()> {
    let mut store = StateStore::open(":memory:")?;
    put_claim(&mut store)?;
    store.register_task(task("a", Exploration, &[]), ACTOR)?;
    let lease = take(&mut store, None)?;
    let at_limit = format!(" \"{}\" ", "é".repeat((32 * 1024 - 4) / 2));
    assert_eq!(at_limit.len(), 32 * 1024);
    let too_large = format!("{at_limit} ");
    for (base_commit, claim_id, role, result_json) in [
        ("short", CLAIM, "prover_worker", "true"),
        (BASE, "bad claim", "prover_worker", "true"),
        (BASE, CLAIM, "prover", "true"),
        (BASE, CLAIM, "independent_reviewer ", "true"),
        (BASE, CLAIM, "oracle", "true"),
        (BASE, CLAIM, "prover_worker", "["),
        (BASE, CLAIM, "prover_worker", &too_large),
    ] {
        assert!(
            store
                .finish_task_run(
                    &lease,
                    TaskRunOutcome::ModelCandidate {
                        base_commit,
                        claim_id,
                        role,
                        result_json,
                    },
                    ACTOR
                )
                .is_err()
        );
    }
    let bad_audit = Change {
        actor: "",
        git_commit: None,
    };
    assert!(
        store
            .finish_task_run(&lease, model_candidate("true"), bad_audit)
            .is_err()
    );
    assert!(store.task_candidate("a")?.is_none());
    assert_eq!(store.events_after(0, 100)?.len(), 3);
    assert_eq!(store.task_scheduler_status()?.exploration.active, 1);
    store.finish_task_run(
        &lease,
        TaskRunOutcome::ModelCandidate {
            base_commit: SHA,
            claim_id: CLAIM,
            role: "independent_reviewer",
            result_json: &at_limit,
        },
        ACTOR,
    )?;
    let row = store
        .task_candidate("a")?
        .context("missing reviewer result")?;
    assert_eq!(row.role, "independent_reviewer");
    assert_eq!(row.result_json, at_limit);
    Ok(())
}

#[test]
fn expired_reclaimed_and_frozen_model_leases_cannot_complete() -> Result<()> {
    let mut store = StateStore::open(":memory:")?;
    put_claim(&mut store)?;
    store.register_task(task("a", Exploration, &[]), ACTOR)?;
    let expired = take(&mut store, None)?;
    store.db.execute(
        "UPDATE scheduler_tasks SET lease_expires_at_ms=0 WHERE task_id='a'",
        [],
    )?;
    assert!(
        store
            .finish_task_run(&expired, model_candidate("null"), ACTOR)
            .is_err()
    );
    let frozen = take(&mut store, None)?;
    assert!(
        store
            .finish_task_run(&expired, model_candidate("null"), ACTOR)
            .is_err()
    );
    store.db.execute("INSERT INTO circuit_breakers(breaker_id,target_definition_id,module_path,engine,state,updated_at)
        VALUES('fixture','Defs.name',?1,'lean4','TRIPPED','now')", [BLOCKED_MODULE])?;
    // Even if the hook has not cleared the lease, a live gate prevents completion.
    assert!(
        store
            .finish_task_run(&frozen, model_candidate("null"), ACTOR)
            .is_err()
    );
    let invalidated = store.write(|tx| freeze_module_leases(tx, "lean4", BLOCKED_MODULE, ACTOR))?;
    assert_eq!(invalidated, 1);
    assert!(store.task_candidate("a")?.is_none());
    assert_eq!(
        store.task_scheduler_status()?.exploration.breaker_blocked,
        1
    );
    store.db.execute(
        "UPDATE circuit_breakers SET state='CLOSED' WHERE breaker_id='fixture'",
        [],
    )?;
    assert!(
        store
            .finish_task_run(&frozen, model_candidate("null"), ACTOR)
            .is_err()
    );
    let retry = take(&mut store, None)?;
    assert!(
        store
            .finish_task_run(&frozen, model_candidate("null"), ACTOR)
            .is_err()
    );
    store.finish_task_run(&retry, model_candidate("null"), ACTOR)?;
    assert_eq!(store.task_scheduler_status()?.exploration.completed, 1);
    Ok(())
}

#[test]
fn legacy_readonly_candidate_is_unchanged_and_writable_open_adds_model_table() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("legacy.sqlite");
    let mut store = StateStore::open(&path)?;
    put_claim(&mut store)?;
    store.register_task(task("a", Exploration, &[]), ACTOR)?;
    let native = take(&mut store, None)?;
    // Remove the additive table to reproduce the pre-model layout.
    store
        .db
        .execute("DROP TABLE scheduler_model_task_candidates", [])?;
    store.finish_task_run(&native, candidate("true"), ACTOR)?;
    let mut model_task = task("model", Infrastructure, &["a"]);
    model_task.target = "a";
    store.register_task(model_task, ACTOR)?;
    let model = take(&mut store, None)?;
    drop(store);

    let readonly = StateStore::open_readonly(&path)?;
    let retained = serde_json::to_value(
        readonly
            .task_candidate("a")?
            .context("missing legacy row")?,
    )?;
    assert_eq!(retained["executable_sha256"], SHA);
    assert!(retained.get("source").is_none());
    assert!(readonly.task_candidate("model")?.is_none());
    assert!(readonly.task_candidate("unknown")?.is_none());
    assert!(!readonly.db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='scheduler_model_task_candidates')",
        [],
        |row| row.get::<_, bool>(0),
    )?);
    drop(readonly);

    let mut store = StateStore::open(&path)?;
    assert_eq!(
        serde_json::to_value(store.task_candidate("a")?.context("missing migrated row")?)?,
        retained
    );
    store.finish_task_run(&model, model_candidate("false"), ACTOR)?;
    drop(store);
    let readonly = StateStore::open_readonly(&path)?;
    let row = readonly
        .task_candidate("model")?
        .context("missing migrated model result")?;
    assert_eq!(row.source, TaskCandidateSource::DshModel);
    assert_eq!(row.result_json, "false");
    Ok(())
}
