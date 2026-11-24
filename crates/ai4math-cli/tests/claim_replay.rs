#![cfg(unix)]

use serde_json::Value;
use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

fn ai(root: &Path, cmd: &str, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ai4math"))
        .arg(cmd)
        .arg(root)
        .args(args)
        .output()
        .unwrap()
}

fn ok(out: Output) -> Vec<u8> {
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

fn json(out: Output) -> Value {
    serde_json::from_slice(&ok(out)).unwrap()
}

#[test]
fn claim_replay_uses_existing_private_ledger_and_exact_cursor() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let missing = ai(root, "claim-replay", &["target"]);
    assert!(!missing.status.success());
    assert!(!root.join(".runtime").exists());

    ok(ai(root, "init", &["lean4"]));
    ok(ai(
        root,
        "declare-claim",
        &[
            "target",
            "Target.lean",
            "result",
            "Readable title",
            "An informal claim",
        ],
    ));
    let path = root.join(".runtime/state.sqlite");
    let before = fs::read(&path).unwrap();
    let current = json(ai(root, "claim-replay", &["target"]));
    let n = current["ledger_head_event_seq"].as_i64().unwrap();
    assert!(n > 0);
    assert_eq!(current["through_event_seq"], n);
    assert_eq!(current["applied_events"], 1);
    assert_eq!(current["claim_id"], "target");
    assert_eq!(current["claim"]["title"], "Readable title");
    assert_eq!(current["claim"]["formal_target"], "Target.lean#result");
    assert_eq!(current["claim"]["local_trust"], "L0");
    assert_eq!(current["claim"]["last_event_seq"], n);
    let pre = json(ai(root, "claim-replay", &["target", "0"]));
    assert_eq!(pre["ledger_head_event_seq"], n);
    assert_eq!(pre["through_event_seq"], 0);
    assert_eq!(pre["applied_events"], 0);
    assert!(pre.get("claim").unwrap().is_null());
    assert!(json(ai(root, "claim-replay", &["unknown"]))["claim"].is_null());

    let future = (n + 1).to_string();
    for cursor in [
        "",
        "+1",
        "-1",
        "01",
        "1e1",
        " 1",
        "9223372036854775808",
        &future,
    ] {
        let refused = ai(root, "claim-replay", &["target", cursor]);
        assert!(!refused.status.success(), "accepted {cursor:?}");
        assert!(refused.stdout.is_empty(), "published partial replay");
    }
    assert_eq!(fs::read(path).unwrap(), before, "replay wrote the ledger");
}

#[test]
fn claim_replay_does_not_follow_an_aliased_runtime() {
    use std::os::unix::fs::symlink;
    let source = tempfile::tempdir().unwrap();
    ok(ai(source.path(), "init", &["lean4"]));
    let alias = tempfile::tempdir().unwrap();
    symlink(
        source.path().join(".runtime"),
        alias.path().join(".runtime"),
    )
    .unwrap();
    let refused = ai(alias.path(), "claim-replay", &["target"]);
    assert!(!refused.status.success());
    assert!(refused.stdout.is_empty());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("owner-controlled project"));
}
