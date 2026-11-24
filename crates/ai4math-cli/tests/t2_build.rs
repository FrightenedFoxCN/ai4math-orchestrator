#![cfg(unix)]

use ai4math_broker::ProjectNativeGate;
use ai4math_store::{Change, NewClaim, StateStore, Trust};
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
    let pin = "a".repeat(64);
    let managed_theorem = |repo: &str, reviewed: &str| {
        ai(
            &root,
            "t2-check-managed",
            &[
                repo,
                "Audit.lean",
                "Fixture.n",
                reviewed,
                nonexistent,
                &pin,
                &pin,
            ],
        )
    };
    let manifest = root.join("reviewed-targets.json");
    fs::write(
        &manifest,
        br#"[{"target_module":"Audit.lean","declaration":"Fixture.n"}]"#,
    )
    .unwrap();
    let managed_batch = |repo: &str, reviewed: &str| {
        ai(
            &root,
            "t2-check-batch",
            &[
                repo,
                "Audit.lean",
                reviewed,
                nonexistent,
                &pin,
                &pin,
                manifest.to_str().unwrap(),
            ],
        )
    };
    let release = |repo: &str, reviewed: &str, policy: &str| {
        ai(
            &root,
            "t2-release-check",
            &[
                repo,
                "Audit.lean",
                reviewed,
                nonexistent,
                policy,
                &pin,
                &pin,
                &pin,
            ],
        )
    };
    rejects(
        release(repo_str, &head, "bad"),
        "policy SHA-256 must be 64 lowercase hex digits",
    );
    rejects(
        release(repo_str, &head, &pin),
        "initialized private project",
    );
    rejects(
        managed_batch(repo_str, &head),
        "initialized private project",
    );
    rejects(audit(repo_str, &head), "initialized private project");
    rejects(
        managed_theorem(repo_str, &head),
        "initialized private project",
    );
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
    rejects(
        managed_theorem(other_str, &head),
        "not bound to the supplied managed repository",
    );
    rejects(
        managed_theorem(repo_str, &old),
        "reviewed T2 commit is not current main",
    );
    rejects(
        managed_batch(other_str, &head),
        "not bound to the supplied managed repository",
    );
    rejects(
        managed_batch(repo_str, &old),
        "reviewed T2 commit is not current main",
    );
    rejects(
        release(other_str, &head, &pin),
        "not bound to the supplied managed repository",
    );
    rejects(
        release(repo_str, &old, &pin),
        "reviewed T2 commit is not current main",
    );
    let lease = ProjectNativeGate::new(&root).try_heavy().unwrap();
    rejects(audit(repo_str, &head), "project native execution is busy");
    rejects(
        managed_theorem(repo_str, &head),
        "project native execution is busy",
    );
    rejects(
        managed_batch(repo_str, &head),
        "project native execution is busy",
    );
    rejects(
        release(repo_str, &head, &pin),
        "project native execution is busy",
    );
    drop(lease);
    fs::copy(&manifest, root.join("ai4math-t2-release.json")).unwrap();
    rejects(release(repo_str, &head, &pin), "ai4math-t2-release.json");
    let after = StateStore::open_readonly(&state)
        .unwrap()
        .status()
        .unwrap()
        .last_event_seq;
    assert_eq!(after, before);
    assert_eq!(git(&repo, &["show-ref"]), main_before);
    assert!(!repo.join("ai4math-golden").exists());

    let mut store = StateStore::open(&state).unwrap();
    let change = Change {
        actor: "release-guard-fixture",
        git_commit: Some(&head),
    };
    store
        .create_claim(
            NewClaim {
                id: "oracle_claim",
                title: "Oracle",
                informal_statement: "Unrepaid debt",
                engine: "lean4",
                formal_target: None,
                module_path: None,
                critical: false,
            },
            change,
        )
        .unwrap();
    store
        .set_local_trust("oracle_claim", Trust::L4a, change)
        .unwrap();
    let with_debt = store.status().unwrap().last_event_seq;
    rejects(
        release(repo_str, &head, &pin),
        "T2 release requires zero audited L4 trust debt findings",
    );
    assert_eq!(store.status().unwrap().last_event_seq, with_debt);
    assert_eq!(
        store.claim("oracle_claim").unwrap().unwrap().local_trust,
        Trust::L4a
    );
    assert_eq!(git(&repo, &["show-ref"]), main_before);
}
