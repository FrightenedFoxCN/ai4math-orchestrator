#![cfg(unix)]

use ai4math_broker::ProjectNativeGate;
use ai4math_store::StateStore;
use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

fn checked(out: Output) -> String {
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().into()
}
fn git(root: &Path, args: &[&str]) -> String {
    checked(
        Command::new("/usr/bin/git")
            .current_dir(root)
            .args([
                "-c",
                "user.name=T2 Fixture",
                "-c",
                "user.email=t2@example.invalid",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .output()
            .unwrap(),
    )
}
fn ai(root: &Path, command: &str, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ai4math"))
        .arg(command)
        .arg(root)
        .args(args)
        .output()
        .unwrap()
}
fn rejects(out: Output, expected: &str) {
    assert!(!out.status.success() && out.stdout.is_empty());
    let error = String::from_utf8_lossy(&out.stderr);
    assert!(error.contains(expected), "{error}");
}

#[test]
fn t2_refuses_outside_the_bound_current_snapshot_or_when_native_resources_are_busy() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let src = root.join("source");
    fs::create_dir(&src).unwrap();
    git(&src, &["init", "--initial-branch=main", "--quiet"]);
    fs::write(src.join("Audit.lean"), "def n : Nat := 0\n").unwrap();
    git(&src, &["add", "."]);
    git(&src, &["commit", "--quiet", "-m", "initial"]);
    let old = git(&src, &["rev-parse", "HEAD"]);
    git(
        &src,
        &["commit", "--allow-empty", "--quiet", "-m", "new review"],
    );
    let head = git(&src, &["rev-parse", "HEAD"]);
    git(
        &root,
        &["clone", "--bare", "--quiet", "source", "managed.git"],
    );
    git(
        &root,
        &["clone", "--bare", "--quiet", "source", "unbound.git"],
    );
    let repo = root.join("managed.git");
    let other = root.join("unbound.git");
    let repo_str = repo.to_str().unwrap();
    let other_str = other.to_str().unwrap();
    let nonexistent = "/no-fixture-compiler-should-be-called";
    let audit = |repo: &str, reviewed: &str| {
        ai(
            &root,
            "t2-build",
            &[repo, "Audit.lean", reviewed, nonexistent],
        )
    };
    rejects(audit(repo_str, &head), "initialized private project");
    assert!(!root.join(".runtime").exists());
    checked(ai(&root, "init", &["lean4"]));
    rejects(audit(repo_str, &head), "bind the managed repository");
    checked(ai(&root, "task-bind", &[repo_str, "Audit.lean", &head]));
    let state = root.join(".runtime/state.sqlite");
    let before = StateStore::open_readonly(&state)
        .unwrap()
        .status()
        .unwrap()
        .last_event_seq;
    let main_before = git(&repo, &["show-ref"]);
    rejects(
        audit(other_str, &head),
        "not bound to the supplied managed repository",
    );
    rejects(
        audit(repo_str, &old),
        "reviewed T2 commit is not current main",
    );
    let lease = ProjectNativeGate::new(&root).try_heavy().unwrap();
    rejects(audit(repo_str, &head), "project native execution is busy");
    drop(lease);
    let after = StateStore::open_readonly(&state)
        .unwrap()
        .status()
        .unwrap()
        .last_event_seq;
    assert_eq!(after, before);
    assert_eq!(git(&repo, &["show-ref"]), main_before);
    assert!(!repo.join("ai4math-golden").exists());
}
