use std::process::Command;

use serde_json::Value;

fn run(_root: &str, args: &[&str]) -> Value {
    let output = Command::new(env!("CARGO_BIN_EXE_ai4math"))
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "ai4math {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn init_managed_binds_an_idempotent_reviewed_repository() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().to_str().unwrap();

    let initialized = run(root, &["init-managed", root, "lean4"]);
    assert_eq!(initialized["bound"], true);
    assert_eq!(initialized["bound"], true);
    let marker: Value = serde_json::from_slice(
        &std::fs::read(temp.path().join(".runtime/managed-git.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(marker["anchor_module"], "Main.lean");
    let repo = marker["repo"].as_str().unwrap();
    let commit = marker["base_commit"].as_str().unwrap();

    let status = run(root, &["status", root]);
    assert_eq!(status["managed_repo_bound"], true);
    assert_eq!(status["managed_repo"], repo);
    assert_eq!(status["state"]["last_event_seq"], 1);

    let registered = run(
        root,
        &[
            "task-register",
            root,
            repo,
            "managed.smoke",
            "infrastructure",
            "Main.lean",
            "goal",
        ],
    );
    assert_eq!(registered["dispatched"], false);

    let again = run(root, &["init-managed", root, "lean4"]);
    assert_eq!(again["bound"], true);
    let marker: Value = serde_json::from_slice(
        &std::fs::read(temp.path().join(".runtime/managed-git.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(marker["base_commit"], commit);
    let status = run(root, &["status", root]);
    assert_eq!(status["state"]["last_event_seq"], 2);
}

#[test]
fn plain_init_remains_unbound_for_specialized_fixtures() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().to_str().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ai4math"))
        .args(["init", root, "rocq"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let status = run(root, &["status", root]);
    assert_eq!(status["managed_repo_bound"], false);
    assert_eq!(status["managed_repo"], Value::Null);
    assert_eq!(status["state"]["last_event_seq"], 0);
}
