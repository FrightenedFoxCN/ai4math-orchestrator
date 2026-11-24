use super::*;
use crate::{enroll_claim, init, managed_tests::git};
use ai4math_core::Engine;
use std::path::PathBuf;

const CLAIM: &str = "oracle-critical";
const TARGET: &str = "Goal.lean";
const MAIN: &str = "refs/heads/main";
const QUERY: &str = r#"{"version":1,"variables":[{"name":"x","lower":0,"upper":3}],"premises":[],"conclusion":{"terms":[{"var":"x","coeff":1}],"relation":"lt","constant":3}}"#;

fn fixture(root: &Path) -> Result<(PathBuf, PathBuf, String)> {
    init(root, Engine::Lean4)?;
    let work = root.join("work");
    fs::create_dir(&work)?;
    git(&work, &["init", "--initial-branch=main"]);
    fs::write(
        work.join(TARGET),
        "theorem target (x : Nat) : x < 3 := {{PROOF_BODY}}\n",
    )?;
    git(&work, &["add", "."]);
    git(&work, &["commit", "-m", "freeze target"]);
    let head = git(&work, &["rev-parse", "HEAD"]);
    git(root, &["clone", "--bare", "work", "repo.git"]);
    let repo = root.join("repo.git");
    enroll_claim(
        root,
        CLAIM,
        TARGET,
        "target",
        "Critical statement",
        "All the examined natural numbers are less than three.",
        true,
    )?;
    let mut store = StateStore::open(root.join(".runtime/state.sqlite"))?;
    store.bind_task_repository(repo.canonicalize()?.to_str().unwrap(), &head)?;
    Ok((work, repo, head))
}

fn advance(work: &Path, repo: &Path) -> Result<String> {
    fs::write(
        work.join(TARGET),
        "-- updated committed context\ntheorem target (x : Nat) : x < 3 := {{PROOF_BODY}}\n",
    )?;
    git(work, &["add", "."]);
    git(work, &["commit", "-m", "review context changed"]);
    git(
        work,
        &["push", repo.to_str().unwrap(), "HEAD:refs/heads/main"],
    );
    Ok(git(repo, &["rev-parse", MAIN]))
}

#[test]
fn approval_is_read_only_and_bound_to_snapshot_encoding_and_solvers() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path();
    let (work, repo, head) = fixture(root)?;
    let ledger = StateStore::open_readonly(root.join(".runtime/state.sqlite"))?;
    let seq = ledger.status()?.last_event_seq;
    let q = OracleQuery::parse(QUERY.as_bytes())?.fingerprint()?;
    let solver = [&*"1".repeat(64), &*"2".repeat(64)];
    let original = {
        let lock = PromotionRepo::lock(&repo)?;
        let original = prepare(root, &lock, &ledger, CLAIM, &q, solver)?;
        assert_eq!(original.commit, head);
        assert_eq!(original, prepare(root, &lock, &ledger, CLAIM, &q, solver)?);
        assert_ne!(
            original.approval_sha256,
            prepare(root, &lock, &ledger, CLAIM, &q, [solver[1], solver[0]])?.approval_sha256
        );
        assert_ne!(
            original.approval_sha256,
            prepare(root, &lock, &ledger, CLAIM, &"3".repeat(64), solver)?.approval_sha256
        );
        original
    };
    advance(&work, &repo)?;
    let lock = PromotionRepo::lock(&repo)?;
    assert_ne!(
        original.approval_sha256,
        prepare(root, &lock, &ledger, CLAIM, &q, solver)?.approval_sha256
    );
    assert_eq!(ledger.status()?.last_event_seq, seq);
    Ok(())
}

#[test]
#[cfg(target_os = "macos")]
#[ignore = "requires native Seatbelt and AI4MATH_TEST_Z3 / AI4MATH_TEST_CVC5"]
fn native_confirmed_oracle_retains_idempotently_without_authority() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path();
    let (work, repo, main) = fixture(root)?;
    let ledger = StateStore::open_readonly(root.join(".runtime/state.sqlite"))?;
    let before = ledger.claim(CLAIM)?.unwrap();
    let seq = ledger.status()?.last_event_seq;
    let query = OracleQuery::parse(QUERY.as_bytes())?;
    let path = root.join("query.json");
    fs::write(&path, QUERY)?;
    let z3 = PathBuf::from(std::env::var_os("AI4MATH_TEST_Z3").context("set native z3")?);
    let cvc5 = PathBuf::from(std::env::var_os("AI4MATH_TEST_CVC5").context("set native cvc5")?);
    let solvers = OracleSolvers::new(&z3, &cvc5)?;
    let approval = current_approval(root, &repo, CLAIM, &query, &solvers)?;
    let changed = QUERY.replace("\"upper\":3", "\"upper\":2");
    fs::write(&path, changed)?;
    assert!(
        retain(
            root,
            &repo,
            CLAIM,
            &approval.approval_sha256,
            &path,
            &z3,
            &cvc5
        )
        .unwrap_err()
        .to_string()
        .contains("approval does not match")
    );
    assert_eq!(ledger.status()?.last_event_seq, seq);
    fs::write(&path, QUERY)?;
    let observation = Broker::new().probe_bounded_oracle(&solvers, query)?;
    assert_eq!(
        observation.outcome(),
        ai4math_broker::OracleOutcome::Counterexample
    );
    retain(
        root,
        &repo,
        CLAIM,
        &approval.approval_sha256,
        &path,
        &z3,
        &cvc5,
    )?;
    let persisted = record_current(root, &repo, &approval, &observation)?;
    let saved = ledger.oracle_observation(&persisted.id)?.unwrap();
    assert_eq!(saved.id, persisted.id);
    assert_eq!(saved.approval, approval);
    assert_eq!(saved.observation["outcome"], "counterexample");
    assert_eq!(
        saved.observation["exact_check"]["counterexample"],
        serde_json::json!([["x", 3]])
    );
    assert_eq!(saved.event_seq, seq + 1);
    assert_eq!(ledger.status()?.last_event_seq, seq + 1);
    let events = ledger.events_after(seq, 8)?;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event_type, "ORACLE_OBSERVATION_RECORDED");
    assert!(
        !events[0]
            .payload
            .to_string()
            .contains("examined natural numbers")
    );
    assert_eq!(git(&repo, &["rev-parse", MAIN]), main);
    let after = ledger.claim(CLAIM)?.unwrap();
    assert_eq!(after.local_trust, before.local_trust);
    assert_eq!(after.effective_trust, before.effective_trust);
    assert_eq!(after.receipt_id, before.receipt_id);
    assert_eq!(after.wiki_id, before.wiki_id);
    assert_eq!(after.stale, before.stale);
    assert_eq!(ledger.status()?.pending_decisions, 0);
    assert_eq!(fs::read_dir(root.join(".mathwiki/graveyard"))?.count(), 0);
    read_retained(root, &persisted.id)?;

    use ai4math_store::{Change, NewTask, TaskTrack};
    use std::time::Duration;
    let mut scheduler = StateStore::open(root.join(".runtime/state.sqlite"))?;
    let actor = Change {
        actor: "fixture",
        git_commit: Some(&main),
    };
    for (id, track, target) in [
        ("explore", TaskTrack::Exploration, "target"),
        ("explore-later", TaskTrack::Exploration, "target"),
        ("other", TaskTrack::Exploration, "other"),
        ("verify", TaskTrack::Infrastructure, "target"),
    ] {
        scheduler.register_task(
            NewTask {
                id,
                track,
                engine: "lean4",
                module_path: TARGET,
                target,
                dependencies: &[],
            },
            actor,
        )?;
    }
    let before_hold = ledger.status()?.last_event_seq;
    assert!(hold(root, &repo, "other", &persisted.id, 60).is_err());
    assert!(hold(root, &repo, "verify", &persisted.id, 60).is_err());
    assert_eq!(ledger.status()?.last_event_seq, before_hold);
    hold(root, &repo, "explore", &persisted.id, 60)?;
    hold(root, &repo, "explore", &persisted.id, 60)?;
    let held = ledger.oracle_task_hold("explore")?.unwrap();
    assert!(held.active);
    assert_eq!(held.commit, main);
    assert_eq!(ledger.status()?.last_event_seq, before_hold + 1);
    assert_eq!(
        ledger.task_scheduler_status()?.exploration.held_for_oracle,
        1
    );
    let telemetry = scheduler.live_telemetry()?;
    assert_eq!(telemetry.task_scheduler.exploration.held_for_oracle, 1);
    assert!(
        scheduler
            .claim_expected_task(
                Some(TaskTrack::Exploration),
                "explore",
                Duration::from_secs(10),
                actor
            )?
            .is_none()
    );
    let infra = scheduler
        .claim_task(
            Some(TaskTrack::Infrastructure),
            Duration::from_secs(10),
            actor,
        )?
        .unwrap();
    assert_eq!(infra.id, "verify");
    let next = scheduler
        .claim_task(Some(TaskTrack::Exploration), Duration::from_secs(10), actor)?
        .unwrap();
    assert_eq!(next.id, "explore-later");
    assert!(hold(root, &repo, "explore-later", &persisted.id, 60).is_err());
    hold_status(root, "explore")?;
    release_hold(root, "explore", &held.id)?;
    let seq_after_release = ledger.status()?.last_event_seq;
    release_hold(root, "explore", &held.id)?;
    assert_eq!(ledger.status()?.last_event_seq, seq_after_release);
    assert!(!ledger.oracle_task_hold("explore")?.unwrap().active);
    assert_eq!(
        ledger.task_scheduler_status()?.exploration.held_for_oracle,
        0
    );
    let resumed = scheduler
        .claim_task(Some(TaskTrack::Exploration), Duration::from_secs(10), actor)?
        .unwrap();
    assert_eq!(resumed.id, "explore");
    assert_eq!(
        scheduler.claim(CLAIM)?.unwrap().local_trust,
        before.local_trust
    );
    assert_eq!(scheduler.status()?.pending_decisions, 0);
    assert!(
        scheduler
            .events_after(before_hold, 32)?
            .iter()
            .all(|e| !e.event_type.contains("REFUT"))
    );
    let seq = ledger.status()?.last_event_seq;
    advance(&work, &repo)?;
    assert!(
        hold(root, &repo, "explore", &persisted.id, 60)
            .unwrap_err()
            .to_string()
            .contains("not from the current reviewed project")
    );
    assert!(
        record_current(root, &repo, &approval, &observation)
            .unwrap_err()
            .to_string()
            .contains("changed during the probe")
    );
    assert_eq!(ledger.status()?.last_event_seq, seq);
    Ok(())
}
