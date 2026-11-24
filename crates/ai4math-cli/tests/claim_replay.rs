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

#[test]
fn plan_replay_uses_event_sequence_without_graph_or_repo_lookup() {
    use ai4math_store::{Change, NewPlan, PlanStatus, StateStore};
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    ok(ai(root, "init", &["lean4"]));
    for (id, module) in [("target", "Target.lean"), ("lemma", "Lemma.lean")] {
        ok(ai(
            root,
            "declare-claim",
            &[id, module, "result", id, "An informal claim"],
        ));
    }
    let path = root.join(".runtime/state.sqlite");
    let seq = {
        let mut store = StateStore::open(&path).unwrap();
        store
            .create_plan(
                NewPlan {
                    id: "route",
                    target: "target",
                    name: "Recorded route",
                    track: "infrastructure",
                    status: PlanStatus::Active,
                    dependencies: &[("lemma", "lemma")],
                },
                Change {
                    actor: "test:operator",
                    git_commit: None,
                },
            )
            .unwrap();
        store.status().unwrap().last_event_seq
    };
    let before = fs::read(&path).unwrap();
    let found = json(ai(root, "plan-replay", &["route"]));
    assert_eq!(
        found,
        serde_json::json!({"plan_id":"route","through_event_seq":seq,"ledger_head_event_seq":seq,
        "applied_events":1,"plan":{"plan_id":"route","target_claim_id":"target","name":"Recorded route","track":"infrastructure",
        "dependencies":[{"claim_id":"lemma","kind":"lemma"}],"durable_status":"ACTIVE","status":"ACTIVE",
        "effective_trust":"L0","blocking_claim_id":null,"refuted_claim_id":null,"last_event_seq":seq}})
    );
    let earlier = (seq - 1).to_string();
    assert!(json(ai(root, "plan-replay", &["route", &earlier]))["plan"].is_null());
    assert!(json(ai(root, "plan-replay", &["unknown"]))["plan"].is_null());
    let future = (seq + 1).to_string();
    for cursor in ["-1", "01", &future] {
        let bad = ai(root, "plan-replay", &["route", cursor]);
        assert!(!bad.status.success());
        assert!(bad.stdout.is_empty());
    }
    assert_eq!(fs::read(path).unwrap(), before);
}

#[test]
fn graph_replay_exports_a_single_event_cut_and_never_initializes_state() {
    use ai4math_store::{Change, NewPlan, PlanStatus, StateStore};
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    assert!(!ai(root, "graph-replay", &[]).status.success());
    assert!(!root.join(".runtime").exists());
    ok(ai(root, "init", &["lean4"]));
    ok(ai(
        root,
        "declare-claim",
        &[
            "z-target",
            "Target.lean",
            "result",
            "Target",
            "An informal claim",
        ],
    ));
    ok(ai(
        root,
        "declare-claim",
        &[
            "a-dep",
            "Dep.lean",
            "result",
            "Dependency",
            "An informal dependency",
        ],
    ));
    let path = root.join(".runtime/state.sqlite");
    let (prefix, head) = {
        let mut store = StateStore::open(&path).unwrap();
        let before = store.status().unwrap().last_event_seq;
        store
            .create_plan(
                NewPlan {
                    id: "route",
                    target: "z-target",
                    name: "Route",
                    track: "infrastructure",
                    status: PlanStatus::Active,
                    dependencies: &[("a-dep", "lemma")],
                },
                Change {
                    actor: "test:operator",
                    git_commit: None,
                },
            )
            .unwrap();
        (before, store.status().unwrap().last_event_seq)
    };
    let before_bytes = fs::read(&path).unwrap();
    let latest = json(ai(root, "graph-replay", &[]));
    assert_eq!(latest["through_event_seq"], head);
    assert_eq!(latest["ledger_head_event_seq"], head);
    assert_eq!(latest["applied_claim_events"], 2);
    assert_eq!(latest["applied_plan_events"], 1);
    assert_eq!(latest["dependency_edge_count"], 1);
    assert_eq!(latest["claims"][0]["claim_id"], "a-dep");
    assert_eq!(latest["claims"][1]["claim_id"], "z-target");
    assert_eq!(
        latest["plans"][0]["dependencies"],
        serde_json::json!([{"claim_id":"a-dep","kind":"lemma"}])
    );
    let previous = json(ai(root, "graph-replay", &[&prefix.to_string()]));
    assert_eq!(previous["claims"], latest["claims"]);
    assert_eq!(previous["through_event_seq"], prefix);
    assert_eq!(previous["ledger_head_event_seq"], head);
    assert_eq!(previous["plans"], serde_json::json!([]));
    let future = (head + 1).to_string();
    for cursor in ["-1", "01", &future] {
        let bad = ai(root, "graph-replay", &[cursor]);
        assert!(!bad.status.success());
        assert!(bad.stdout.is_empty());
    }
    assert_eq!(fs::read(path).unwrap(), before_bytes);
}
