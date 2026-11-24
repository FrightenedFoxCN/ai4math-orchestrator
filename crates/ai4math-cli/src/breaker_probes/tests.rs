use super::*;
use crate::{init, managed_tests::git, pivot};
use ai4math_store::retained::hash_fact;
use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf, time::Duration};

const MODULE: &str = "Spec.lean";
const D: &str = "watched";
const ID: &str = "B5";
const REASON: &str = "Keep the definition, after its regression checks pass";
const CODE: &str = "def watched : Nat := 1\ntheorem probeOne : watched = 1 := rfl\ntheorem probeTwo : 0 < watched := by decide\ntheorem unrelated : True := True.intro\n";

fn retained(root: &Path, store: &mut StateStore) -> Result<String> {
    let mut body = serde_json::to_value(store.briefing_facts()?)?;
    let id = hash_fact(&body)?;
    body["fact_sha256"] = json!(id);
    let dir = root.join(".runtime/briefings");
    fs::create_dir_all(&dir)?;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
    fs::write(dir.join(format!("{id}.json")), serde_json::to_vec(&body)?)?;
    Ok(id)
}
fn change() -> Change<'static> {
    Change {
        actor: "test:coordinator",
        git_commit: None,
    }
}
fn setup(root: &Path) -> Result<(PathBuf, StateStore)> {
    init(root, Engine::Lean4)?;
    let source = root.join("source");
    fs::create_dir(&source)?;
    git(&source, &["init", "--initial-branch=main"]);
    fs::write(source.join(MODULE), CODE)?;
    git(&source, &["add", MODULE]);
    git(&source, &["commit", "-m", "reviewable fixtures"]);
    git(root, &["clone", "--bare", "source", "repo.git"]);
    let repo = root.join("repo.git");
    let mut store = StateStore::open(root.join(".runtime/state.sqlite"))?;
    for (id, track) in [
        ("task-one", TaskTrack::Exploration),
        ("task-two", TaskTrack::Infrastructure),
    ] {
        store.register_task(
            NewTask {
                id,
                track,
                engine: "lean4",
                module_path: MODULE,
                target: D,
                dependencies: &[],
            },
            change(),
        )?;
    }
    let stale = store
        .claim_task(None, Duration::from_secs(60), change())?
        .context("lease before trip")?;
    pivot::trip(
        root,
        &repo,
        ID,
        MODULE,
        D,
        "Operator reports an unhealthy definition",
        false,
    )?;
    assert!(store.complete_task(&stale, change()).is_err());
    assert!(
        store
            .claim_task(None, Duration::from_secs(60), change())?
            .is_none()
    );
    let fact = retained(root, &mut store)?;
    // Generate the public Pivot approval exactly as the executable does. This
    // uses the same pure store plan under an equal reviewed Git main.
    let approved = {
        let git = PromotionRepo::lock(&repo)?;
        let review =
            ai4math_store::retained::read_retained_review(&root.join(".runtime/briefings"), &fact)?
                .unwrap();
        store
            .preview_resume(
                repo_name(&git)?,
                &git.main_head()?,
                &fact,
                &review,
                ID,
                REASON,
            )?
            .adr_id()?
    };
    pivot::apply(root, &repo, &fact, ID, REASON, &approved)?;
    assert!(
        store
            .claim_task(None, Duration::from_secs(60), change())?
            .is_none()
    );
    Ok((repo, store))
}
fn approval(root: &Path, repo: &Path, store: &mut StateStore, cases: &[&str]) -> Result<String> {
    let git = PromotionRepo::lock(repo)?;
    compile_plan(root, &git, store, ID, cases)?.approval_id()
}

#[test]
fn supersession_and_interrupted_attempt_never_unfreeze_tasks() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path();
    let (repo, mut store) = setup(root)?;
    assert_eq!(store.live_telemetry()?.half_open_breakers, 1);
    assert_eq!(
        store.briefing_facts()?.breakers.items[0]
            .probe_status
            .as_deref(),
        Some("AWAITING_REVIEW")
    );
    assert!(approval(root, &repo, &mut store, &["probeOne", "probeOne"]).is_err());
    let prior = approval(root, &repo, &mut store, &["probeOne"])?;
    assert!(arm(root, &repo, ID, &prior, &["probeTwo"]).is_err());
    arm(root, &repo, ID, &prior, &["probeOne"])?;
    let head = git(&repo, &["rev-parse", "refs/heads/main"]);
    let first = store.start_breaker_probes(ID, &prior, &head)?;
    // Simulate the lock-owning coordinator having crashed without a result.
    let next = store.start_breaker_probes(ID, &prior, &head)?;
    assert!(store.finish_breaker_probes(&first, &head, None).is_err());
    store.finish_breaker_probes(&next, &head, None)?;
    let state = store.briefing_facts()?.breakers.items.remove(0);
    assert_eq!(state.state, "HALF_OPEN");
    assert_eq!(state.probe_status.as_deref(), Some("NEEDS_PROBE"));
    assert_eq!(state.probe_passed, Some(0));
    let later = approval(root, &repo, &mut store, &["probeOne", "probeTwo"])?;
    arm(root, &repo, ID, &later, &["probeOne", "probeTwo"])?;
    assert!(store.start_breaker_probes(ID, &prior, &head).is_err());
    assert!(
        store
            .claim_task(None, Duration::from_secs(60), change())?
            .is_none()
    );
    // Publish an altered definition (fixture outside the cooperating writer protocol).
    let source = root.join("source");
    git(
        &source,
        &["pull", "--ff-only", repo.to_str().unwrap(), "main"],
    );
    fs::write(source.join(MODULE), CODE.replace(":= 1", ":= 2"))?;
    git(&source, &["add", MODULE]);
    git(&source, &["commit", "-m", "alter reviewed definition"]);
    git(
        &source,
        &["push", repo.to_str().unwrap(), "HEAD:refs/heads/main"],
    );
    assert!(
        approval(root, &repo, &mut store, &["probeOne"])
            .unwrap_err()
            .to_string()
            .contains("changed after the ADR")
    );
    assert!(run(root, &repo, ID, &later, Path::new("/invalid/compiler")).is_err());
    assert_eq!(store.live_telemetry()?.half_open_breakers, 1);
    Ok(())
}

#[test]
#[cfg(target_os = "macos")]
#[ignore = "requires native host and AI4MATH_TEST_LEAN"]
fn native_half_open_gate_opens_only_after_exact_committed_lean_probes() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path();
    let (repo, mut store) = setup(root)?;
    let exe = std::env::var_os("AI4MATH_TEST_LEAN")
        .context("set AI4MATH_TEST_LEAN to native executable")?;
    let exe = Path::new(&exe);
    let wrong = ["probeOne", "unrelated"];
    let bad = approval(root, &repo, &mut store, &wrong)?;
    arm(root, &repo, ID, &bad, &wrong)?;
    let err = run(root, &repo, ID, &bad, exe).unwrap_err();
    assert!(err.to_string().contains("stays HALF_OPEN"));
    assert!(!format!("{err:#}").contains("sandbox_apply"), "{err:#}");
    assert!(
        store
            .claim_task(None, Duration::from_secs(60), change())?
            .is_none()
    );
    let correct = ["probeOne", "probeTwo"];
    let ok = approval(root, &repo, &mut store, &correct)?;
    arm(root, &repo, ID, &ok, &correct)?;
    run(root, &repo, ID, &ok, exe)?;
    let batch = store.current_breaker_probe_batch(ID)?.unwrap();
    assert_eq!(batch.status, "PASSED");
    assert!(batch.receipt_sha256.is_some());
    let latest = store.briefing_facts()?;
    assert_eq!(latest.breakers.items[0].probe_passed, Some(2));
    assert_eq!(latest.breakers.items[0].state, "CLOSED");
    let seq = latest.event_to_inclusive;
    run(root, &repo, ID, &ok, exe)?;
    assert_eq!(store.status()?.last_event_seq, seq);
    let first = store
        .claim_task(None, Duration::from_secs(60), change())?
        .unwrap();
    let next = store
        .claim_task(None, Duration::from_secs(60), change())?
        .unwrap();
    assert_ne!(first.track, next.track);
    assert_ne!(first.id, next.id);
    store.complete_task(&first, change())?;
    store.complete_task(&next, change())?;
    Ok(())
}
