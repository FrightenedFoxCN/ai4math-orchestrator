use super::*;
use crate::{ManagedProofInput, declare_claim, init, managed_tests::git, promote_managed_body};
use std::path::PathBuf;

const CLAIM: &str = "hammer-goal";
const TARGET: &str = "Goal.lean";
const MAIN: &str = "refs/heads/main";
const PREMISES: &str = r#"{"version":1,"premises":["Hints.seed","Hints.toMiddle","Hints.toLast"]}"#;
const SUPPORT: &str = r#"namespace Hints
inductive First : Prop where | start
inductive Middle : Prop where | step (h : First)
inductive Last : Prop where | step (h : Middle)
theorem seed : First := First.start
theorem toMiddle : First → Middle := Middle.step
theorem toLast : Middle → Last := Last.step
end Hints
"#;

fn fixture(root: &Path) -> Result<(PathBuf, PathBuf, String)> {
    init(root, Engine::Lean4)?;
    let work = root.join("work");
    fs::create_dir(&work)?;
    git(&work, &["init", "--initial-branch=main"]);
    fs::write(work.join("Hints.lean"), SUPPORT)?;
    fs::write(
        work.join(TARGET),
        "import Hints\ntheorem goal : Hints.Last := {{PROOF_BODY}}\n",
    )?;
    fs::write(work.join("ai4math-modules.txt"), "Hints.lean\nGoal.lean\n")?;
    git(&work, &["add", "."]);
    git(&work, &["commit", "-m", "frozen Hammer fixture"]);
    let head = git(&work, &["rev-parse", "HEAD"]);
    git(root, &["clone", "--bare", "work", "repo.git"]);
    let repo = root.join("repo.git");
    declare_claim(
        root,
        CLAIM,
        TARGET,
        "goal",
        "Bounded reconstruction",
        "The chain reaches the last proposition.",
    )?;
    let mut store = StateStore::open(root.join(".runtime/state.sqlite"))?;
    store.bind_task_repository(repo.canonicalize()?.to_str().unwrap(), &head)?;
    Ok((work, repo, head))
}

#[test]
fn hammer_rejects_a_stale_review_before_invoking_a_compiler() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path();
    let (work, repo, head) = fixture(root)?;
    fs::write(
        work.join("Hints.lean"),
        format!("{SUPPORT}\n-- new review\n"),
    )?;
    git(&work, &["add", "."]);
    git(&work, &["commit", "-m", "update review"]);
    git(
        &work,
        &["push", repo.to_str().unwrap(), "HEAD:refs/heads/main"],
    );
    let store = StateStore::open_readonly(root.join(".runtime/state.sqlite"))?;
    let seq = store.status()?.last_event_seq;
    assert!(
        reconstruct(
            root,
            &repo,
            CLAIM,
            &head,
            Path::new("/missing.json"),
            Path::new("/missing/lean")
        )
        .err()
        .unwrap()
        .to_string()
        .contains("no longer matches main")
    );
    assert!(
        promote_managed_body(
            root,
            &repo,
            CLAIM,
            TARGET,
            "goal",
            Path::new("/missing/lean"),
            ManagedProofInput::ReviewedFile {
                file: Path::new("/missing-body"),
                reviewed_base: &head
            },
            None
        )
        .unwrap_err()
        .to_string()
        .contains("older Git snapshot")
    );
    assert_eq!(store.status()?.last_event_seq, seq);
    Ok(())
}

#[test]
fn hammer_rejects_frozen_module_before_invoking_a_compiler() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path();
    let (_, repo, base) = fixture(root)?;
    crate::pivot::trip(
        root,
        &repo,
        "hammer-frozen",
        TARGET,
        "goal",
        "operator paused this module",
        false,
    )?;
    let store = StateStore::open_readonly(root.join(".runtime/state.sqlite"))?;
    let seq = store.status()?.last_event_seq;
    let error = reconstruct(
        root,
        &repo,
        CLAIM,
        &base,
        Path::new("/missing.json"),
        Path::new("/missing/lean"),
    )
    .err()
    .unwrap();
    assert!(error.to_string().contains("breaker gates"), "{error:#}");
    assert_eq!(store.status()?.last_event_seq, seq);
    assert_eq!(git(&repo, &["rev-parse", MAIN]), base);
    Ok(())
}

#[test]
#[cfg(target_os = "macos")]
#[ignore = "requires native Seatbelt and AI4MATH_TEST_LEAN"]
fn native_hammer_reconstruction_remains_advisory_until_reviewed_promotion() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path();
    let (_, repo, base) = fixture(root)?;
    let exe = PathBuf::from(std::env::var_os("AI4MATH_TEST_LEAN").context("set native Lean")?);
    let premises = root.join("premises.json");
    let state = StateStore::open_readonly(root.join(".runtime/state.sqlite"))?;
    let event_seq = state.status()?.last_event_seq;
    fs::write(&premises, r#"{"version":1,"premises":["Hints.toLast"]}"#)?;
    assert!(reconstruct(root, &repo, CLAIM, &base, &premises, &exe).is_err());
    fs::write(&premises, PREMISES)?;
    let result = serde_json::to_value(reconstruct(root, &repo, CLAIM, &base, &premises, &exe)?)?;
    assert_eq!(result["base_commit"], base);
    assert_eq!(result["file_path"], TARGET);
    assert_eq!(result["target_id"], "goal");
    assert!(result.get("receipt").is_none());
    assert!(result.get("receipt_sha256").is_none());
    assert_eq!(git(&repo, &["rev-parse", MAIN]), base);
    assert_eq!(state.status()?.last_event_seq, event_seq);
    assert_eq!(state.claim(CLAIM)?.unwrap().local_trust, Trust::L0);
    assert!(state.promotion_intent()?.is_none());
    let body = root.join("reviewed-body.txt");
    fs::write(
        &body,
        result["proof_body"]
            .as_str()
            .context("missing reconstructed proof body")?,
    )?;
    promote_managed_body(
        root,
        &repo,
        CLAIM,
        TARGET,
        "goal",
        &exe,
        ManagedProofInput::ReviewedFile {
            file: &body,
            reviewed_base: &base,
        },
        None,
    )?;
    assert_ne!(git(&repo, &["rev-parse", MAIN]), base);
    assert_eq!(state.claim(CLAIM)?.unwrap().local_trust, Trust::L5);
    Ok(())
}
