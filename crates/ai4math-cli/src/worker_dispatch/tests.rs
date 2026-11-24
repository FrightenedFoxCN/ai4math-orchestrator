use super::*;
use crate::{breaker_probes, init, managed_tests::git};
use std::{fs, path::PathBuf};

const MODULE: &str = "Target.lean";
const CLAIM: &str = "claim-one";
const DECL: &str = "target";
const STATEMENT: &str = "Truth is inhabited.";
const FROZEN: &str =
    "-- WRITER-PRIVATE-CONTEXT\ndef helper : Nat := 3\ntheorem target : True := {{PROOF_BODY}}\n";

fn project(root: &Path) -> Result<PathBuf> {
    init(root, Engine::Lean4)?;
    let source = root.join("source");
    fs::create_dir(&source)?;
    git(&source, &["init", "--initial-branch=main"]);
    fs::write(source.join(MODULE), FROZEN)?;
    git(&source, &["add", MODULE]);
    git(&source, &["commit", "-m", "freeze definition and theorem"]);
    git(root, &["clone", "--bare", "source", "repo.git"]);
    let repo = root.join("repo.git");
    let head = git(&repo, &["rev-parse", "refs/heads/main"]);
    assert!(bind(root, &repo, MODULE, &"a".repeat(40)).is_err());
    bind(root, &repo, MODULE, &head)?;
    crate::declare_claim(root, CLAIM, MODULE, DECL, "Truth fixture", STATEMENT)?;
    Ok(repo)
}

#[test]
fn request_and_output_follow_restricted_role_protocol() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path();
    let repo = project(root)?;
    let git = PromotionRepo::lock(&repo)?;
    let store = StateStore::open(root.join(".runtime/state.sqlite"))?;
    let reviewer = prepare(
        root,
        &git,
        &store,
        "audit-1",
        CLAIM,
        Role::IndependentReviewer,
        5,
    )?;
    let v: serde_json::Value = serde_json::from_slice(&reviewer.payload)?;
    assert_eq!(v["mode"], "review_header");
    assert_eq!(v["formal_header"], "theorem target : True");
    assert_eq!(v["informal_statement"], STATEMENT);
    for forbidden in [
        "WRITER-PRIVATE-CONTEXT",
        "helper",
        "frozen_target",
        "fence",
        "socket",
        "state.sqlite",
        root.to_str().unwrap(),
    ] {
        assert!(
            !std::str::from_utf8(&reviewer.payload)?.contains(forbidden),
            "unexpected context {forbidden}"
        );
    }
    assert!(
        check_result(
            Role::IndependentReviewer,
            &reviewer,
            br#"{"kind":"review","verdict":"looks_consistent","rationale":"matches"}"#
        )
        .is_ok()
    );
    assert!(check_result(Role::IndependentReviewer,&reviewer,br#"{"kind":"review","verdict":"looks_consistent","rationale":"matches","reviewer_ticket_id":"forged"}"#).is_err());
    let prover = prepare(root, &git, &store, "prove-1", CLAIM, Role::ProverWorker, 5)?;
    assert!(
        check_result(
            Role::ProverWorker,
            &prover,
            br#"{"kind":"proof_body","proof_body":"True.intro"}"#
        )
        .is_ok()
    );
    assert!(
        check_result(
            Role::ProverWorker,
            &prover,
            br#"{"kind":"proof_body","proof_body":"True.intro\naxiom outOfSlot : False"}"#
        )
        .is_err()
    );
    assert!(
        check_result(
            Role::ProverWorker,
            &prover,
            br#"{"kind":"PROVED","receipt_id":"fake"}"#
        )
        .is_err()
    );
    assert!(
        check_result(
            Role::Explorer,
            &prover,
            br#"{"kind":"proof_body","proof_body":"True.intro"}"#
        )
        .is_err()
    );
    Ok(())
}

#[test]
#[cfg(unix)]
fn dispatch_requires_owner_only_runtime_and_private_database_identity() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir()?;
    let root = dir.path();
    init(root, Engine::Lean4)?;
    let state = private_state_path(root)?;
    fs::set_permissions(root.join(".runtime"), fs::Permissions::from_mode(0o755))?;
    assert!(private_state_path(root).is_err());
    fs::set_permissions(root.join(".runtime"), fs::Permissions::from_mode(0o700))?;
    fs::hard_link(&state, root.join("duplicate.sqlite"))?;
    assert!(private_state_path(root).is_err());
    Ok(())
}

#[cfg(target_os = "macos")]
fn fixture_worker(root: &Path) -> Result<(PathBuf, String)> {
    use sha2::{Digest, Sha256};
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/worker_dispatch/fixture.c");
    let exe = root.join("fixture-worker");
    ensure!(
        std::process::Command::new("/usr/bin/clang")
            .args(["-Wall", "-Wextra", "-Werror", "-o"])
            .arg(&exe)
            .arg(&src)
            .status()?
            .success(),
        "could not build fixture worker"
    );
    let sha = format!("{:x}", Sha256::digest(fs::read(&exe)?));
    Ok((exe, sha))
}

#[test]
#[cfg(target_os = "macos")]
#[ignore = "requires native Seatbelt, clang and AI4MATH_TEST_LEAN"]
fn native_worker_suggestion_is_queued_then_requires_managed_kernel_promotion() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path();
    let repo = project(root)?;
    let (exe, sha) = fixture_worker(root)?;
    for (id, track) in [
        ("explore", TaskTrack::Exploration),
        ("review", TaskTrack::Infrastructure),
        ("prove", TaskTrack::Infrastructure),
    ] {
        breaker_probes::enroll(root, &repo, id, track, MODULE, DECL, &[])?;
    }
    assert!(
        run(
            root,
            &repo,
            "prove",
            CLAIM,
            Role::ProverWorker,
            &exe,
            &sha,
            5
        )
        .is_err()
    );
    assert!(
        run(
            root,
            &repo,
            "explore",
            CLAIM,
            Role::Explorer,
            &exe,
            &"a".repeat(64),
            5
        )
        .is_err()
    );
    run(root, &repo, "explore", CLAIM, Role::Explorer, &exe, &sha, 5)?;
    run(
        root,
        &repo,
        "review",
        CLAIM,
        Role::IndependentReviewer,
        &exe,
        &sha,
        5,
    )?;
    run(
        root,
        &repo,
        "prove",
        CLAIM,
        Role::ProverWorker,
        &exe,
        &sha,
        5,
    )?;
    let store = StateStore::open_readonly(root.join(".runtime/state.sqlite"))?;
    assert_eq!(store.claim(CLAIM)?.unwrap().local_trust, Trust::L0);
    let row = store.task_candidate("prove")?.unwrap();
    assert!(row.result_json.contains("True.intro"));
    let lean =
        std::env::var_os("AI4MATH_TEST_LEAN").context("missing native Lean fixture executable")?;
    assert!(promote(root, &repo, "review", Path::new(&lean), None).is_err());
    promote(root, &repo, "prove", Path::new(&lean), None)?;
    assert_eq!(store.claim(CLAIM)?.unwrap().local_trust, Trust::L5);
    assert!(
        promote(root, &repo, "prove", Path::new(&lean), None)
            .unwrap_err()
            .to_string()
            .contains("older Git snapshot")
    );
    Ok(())
}

#[test]
#[cfg(target_os = "macos")]
#[ignore = "requires native macOS Seatbelt host"]
fn tripping_a_running_worker_rejects_its_suggestion() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path();
    let repo = project(root)?;
    let (exe, sha) = fixture_worker(root)?;
    let store = StateStore::open(root.join(".runtime/state.sqlite"))?;
    breaker_probes::enroll(
        root,
        &repo,
        "wait",
        TaskTrack::Infrastructure,
        MODULE,
        DECL,
        &[],
    )?;
    std::thread::scope(|s| -> Result<()> {
        let child = s.spawn(|| {
            run(
                root,
                &repo,
                "wait",
                CLAIM,
                Role::ProverWorker,
                &exe,
                &sha,
                5,
            )
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while store.task_scheduler_status()?.infrastructure.active == 0 {
            ensure!(
                !child.is_finished() && std::time::Instant::now() < deadline,
                "worker did not acquire a lease"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        crate::pivot::trip(
            root,
            &repo,
            "during-worker",
            MODULE,
            DECL,
            "Fixture trips while lease is live",
            false,
        )?;
        let error = child.join().unwrap().unwrap_err();
        assert!(
            format!("{error:#}").contains("lease is no longer valid"),
            "{error:#}"
        );
        assert!(store.task_candidate("wait")?.is_none());
        assert_eq!(
            store
                .task_scheduler_status()?
                .infrastructure
                .breaker_blocked,
            1
        );
        Ok(())
    })?;
    Ok(())
}
