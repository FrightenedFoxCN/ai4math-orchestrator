#![cfg(unix)]

use ai4math_broker::ProjectNativeGate;
use ai4math_store::{Change, NewClaim, StateStore, Trust};
use serde_json::json;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

const ANCHOR: &str = "Audit.lean";
const DEBT_ERROR: &str = "T2 trust debt scan failed";
const COMPILER_ERROR: &str = "resolve native compiler executable";

fn checked(out: Output) -> String {
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

fn git(root: &Path, args: &[&str]) -> String {
    checked(
        Command::new("/usr/bin/git")
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .current_dir(root)
            .args([
                "-c",
                "user.name=T2 Fixture",
                "-c",
                "user.email=t2@example.invalid",
                "-c",
                "core.hooksPath=/dev/null",
            ])
            .args(args)
            .output()
            .unwrap(),
    )
}

fn ai(root: &Path, command: &str) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_ai4math"));
    cmd.arg(command).arg(root);
    cmd
}

fn rejects(out: Output, expected: &str, forbidden: &[&str]) {
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.code().is_some() && !out.status.success(),
        "{stderr}"
    );
    assert!(
        out.stdout.is_empty(),
        "unexpected stdout: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(stderr.contains(expected), "expected {expected:?}: {stderr}");
    for phase in forbidden {
        assert!(!stderr.contains(phase), "unexpected {phase:?}: {stderr}");
    }
}

// Capture the complete bare Git tree and the durable SQLite files (including WAL frames).
// The state connection stays open; SQLite's transient shared-memory read marks are not data.
fn snapshot(root: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
    fn walk(root: &Path, dir: &Path, files: &mut BTreeMap<PathBuf, Option<Vec<u8>>>) {
        for entry in fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let rel = path.strip_prefix(root).unwrap();
            if rel == Path::new(".runtime/state.sqlite-shm") {
                continue;
            }
            let kind = entry.file_type().unwrap();
            if kind.is_dir() {
                files.insert(rel.to_owned(), None);
                walk(root, &path, files);
            } else {
                assert!(kind.is_file(), "unexpected entry: {}", path.display());
                files.insert(rel.to_owned(), Some(fs::read(&path).unwrap()));
            }
        }
    }
    let mut files = BTreeMap::new();
    walk(root, root, &mut files);
    files
}

#[test]
fn t2_audit_runs_guards_then_debt_then_compiler_without_writing() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let source = root.join("source");
    fs::create_dir(&source).unwrap();
    git(&source, &["init", "--initial-branch=main", "--quiet"]);
    fs::write(source.join(ANCHOR), "def n : Nat := 0\n").unwrap();
    git(&source, &["add", ANCHOR]);
    git(&source, &["commit", "--quiet", "-m", "initial"]);
    let old = git(&source, &["rev-parse", "HEAD"]);
    git(
        &source,
        &["commit", "--allow-empty", "--quiet", "-m", "review"],
    );
    let head = git(&source, &["rev-parse", "HEAD"]);
    git(
        &root,
        &["clone", "--bare", "--quiet", "source", "managed.git"],
    );
    let repo = root.join("managed.git");
    checked(ai(&root, "init").arg("lean4").output().unwrap());
    checked(
        ai(&root, "task-bind")
            .arg(&repo)
            .args([ANCHOR, &head])
            .output()
            .unwrap(),
    );
    let gate = ProjectNativeGate::new(&root);
    drop(gate.try_heavy().unwrap()); // Provision the advisory lock before measuring changes.
    let mut store = StateStore::open(root.join(".runtime/state.sqlite")).unwrap();
    let change = Change {
        actor: "t2-audit-fixture",
        git_commit: Some(&head),
    };
    store
        .create_claim(
            NewClaim {
                id: "alpha",
                title: "Example",
                informal_statement: "Fixture only",
                engine: "lean4",
                formal_target: None,
                module_path: None,
                critical: true,
            },
            change,
        )
        .unwrap();
    store.set_local_trust("alpha", Trust::L4a, change).unwrap();
    assert_eq!(store.scan_trust_debt().unwrap().finding_count, 2);

    let missing = root.join("missing-native-lean");
    let run = |command: &str, reviewed: &str| {
        ai(&root, command)
            .arg(&repo)
            .args([ANCHOR, reviewed])
            .arg(&missing)
            .output()
            .unwrap()
    };
    let check = |store: &StateStore, command, reviewed, expected, forbidden: &[&str]| {
        let before = snapshot(&root);
        let seq = store.status().unwrap().last_event_seq;
        rejects(run(command, reviewed), expected, forbidden);
        let after = snapshot(&root);
        assert_eq!(
            before.keys().collect::<Vec<_>>(),
            after.keys().collect::<Vec<_>>()
        );
        for (path, bytes) in before {
            assert!(bytes == after[&path], "changed {}", path.display());
        }
        assert_eq!(store.status().unwrap().last_event_seq, seq);
    };

    // Even after a nonempty debt scan succeeds, compiler failure cannot emit partial JSON.
    check(&store, "t2-audit", &head, COMPILER_ERROR, &[DEBT_ERROR]);
    let bad_seq = store
        .append_event(
            change,
            "CLAIM_TRUST_CHANGED",
            "claim",
            "alpha",
            json!({"previous_local":"L4a", "local":"bogus", "was_stale":false, "detail":{}}),
        )
        .unwrap();
    assert_eq!(store.status().unwrap().last_event_seq, bad_seq);

    // Both guards take precedence over the now-broken graph and the missing compiler.
    check(
        &store,
        "t2-audit",
        &old,
        "reviewed T2 commit is not current main",
        &[DEBT_ERROR, COMPILER_ERROR],
    );
    let lease = gate.try_heavy().unwrap();
    check(
        &store,
        "t2-audit",
        &head,
        "project native execution is busy",
        &[DEBT_ERROR, COMPILER_ERROR],
    );
    drop(lease);

    check(&store, "t2-audit", &head, DEBT_ERROR, &[COMPILER_ERROR]);
    // The standalone build still ignores Claim replay and reaches the compiler.
    check(&store, "t2-build", &head, COMPILER_ERROR, &[DEBT_ERROR]);
}
