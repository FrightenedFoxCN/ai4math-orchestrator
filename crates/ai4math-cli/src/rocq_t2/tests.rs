use super::*;
use crate::{init, managed_tests::git, worker_dispatch};
use std::{fs, path::PathBuf};

fn setup(root: &Path) -> Result<(PathBuf, String)> {
    init(root, Engine::Rocq)?;
    let source = root.join("src");
    fs::create_dir(&source)?;
    git(&source, &["init", "--initial-branch=main"]);
    fs::write(source.join("Support.v"), "Definition holds : True := I.\n")?;
    fs::write(
        source.join("Goal.v"),
        "Require Import Support.\nTheorem target : True.\nProof. exact (Support.holds). Qed.\n",
    )?;
    fs::write(source.join("ai4math-modules.txt"), "Support.v\nGoal.v\n")?;
    git(&source, &["add", "."]);
    git(&source, &["commit", "-m", "committed Rocq fixture"]);
    git(root, &["clone", "--bare", "src", "repo.git"]);
    let repo = root.join("repo.git");
    let head = git(&repo, &["rev-parse", "refs/heads/main"]);
    worker_dispatch::bind(root, &repo, "Goal.v", &head)?;
    Ok((repo, head))
}

#[test]
fn rocq_snapshot_does_not_run_on_stale_head_or_wrong_engine() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let root = tmp.path();
    let (repo, head) = setup(root)?;
    let (names, observed) = t2_build::at_engine_snapshot_generation(
        root,
        &repo,
        (Engine::Rocq, "Goal.v"),
        &head,
        |snapshot, files| {
            assert_eq!(snapshot.base(), head);
            Ok(files
                .iter()
                .map(|(path, _)| path.to_string())
                .collect::<Vec<_>>())
        },
    )?;
    assert_eq!(names, ["Support.v", "Goal.v"]);
    assert_eq!(
        observed,
        crate::open_managed_state(root, true)?
            .status()?
            .last_event_seq
    );
    let unused = Inputs {
        compiler: Path::new("unused"),
        data: Path::new("unused"),
        compiler_sha256: "",
        checker: Path::new("unused"),
        checker_sha256: "",
    };
    let err = check(root, &repo, &"0".repeat(40), ("Goal.v", "target"), unused).unwrap_err();
    assert!(
        !err.to_string()
            .contains("absolute operator toolchain paths")
    );
    let rejected = t2_build::at_engine_snapshot_generation::<()>(
        root,
        &repo,
        (Engine::Lean4, "Goal.v"),
        &head,
        |_, _| panic!("wrong engine invoked build"),
    );
    assert!(rejected.is_err());
    Ok(())
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "requires native Seatbelt and pinned Rocq/coqchk installation"]
fn native_rocq_t2_checks_fresh_objects_without_changing_git_or_ledger() -> Result<()> {
    use anyhow::Context;
    let tmp = tempfile::tempdir()?;
    let root = tmp.path();
    let (repo, head) = setup(root)?;
    let compiler = std::env::var("AI4MATH_TEST_ROCQ").context("missing Rocq")?;
    let data = std::env::var("AI4MATH_TEST_ROCQ_DATA").context("missing Rocq data")?;
    let checker = std::env::var("AI4MATH_TEST_COQCHK").context("missing coqchk")?;
    let compiler_sha =
        std::env::var("AI4MATH_TEST_ROCQ_SHA256").context("missing Rocq worker pin")?;
    let checker_sha = std::env::var("AI4MATH_TEST_COQCHK_SHA256").context("missing coqchk pin")?;
    let inputs = |digest| Inputs {
        compiler: Path::new(&compiler),
        data: Path::new(&data),
        compiler_sha256: &compiler_sha,
        checker: Path::new(&checker),
        checker_sha256: digest,
    };
    let state = crate::open_managed_state(root, true)?;
    let before = state.status()?.last_event_seq;
    check(
        root,
        &repo,
        &head,
        ("Goal.v", "target"),
        inputs(&checker_sha),
    )?;
    assert!(
        check(
            root,
            &repo,
            &head,
            ("Goal.v", "target"),
            inputs(&"0".repeat(64))
        )
        .is_err()
    );
    assert_eq!(state.status()?.last_event_seq, before);
    assert_eq!(git(&repo, &["rev-parse", "refs/heads/main"]), head);
    assert!(!repo.join("ai4math-golden").exists());
    Ok(())
}
