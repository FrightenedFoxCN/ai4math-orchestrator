use std::process::{Command, Output};

fn ai(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ai4math"))
        .args(args)
        .output()
        .unwrap()
}

fn success(output: Output) -> String {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn refutation_recovery_commands_are_explicit_and_idempotent() {
    let root = tempfile::tempdir().unwrap();
    success(ai(&["init", root.path().to_str().unwrap(), "lean4"]));

    let pending: serde_json::Value = serde_json::from_str(&success(ai(&[
        "refute-pending",
        root.path().to_str().unwrap(),
    ])))
    .unwrap();
    assert_eq!(pending["pending"], serde_json::Value::Null);

    let report: serde_json::Value = serde_json::from_str(&success(ai(&[
        "refute-reconcile",
        root.path().to_str().unwrap(),
    ])))
    .unwrap();
    assert_eq!(report["records"], serde_json::json!([]));
    assert_eq!(report["failures"], serde_json::json!([]));

    let missing = ai(&[
        "refute-reconcile",
        root.path().to_str().unwrap(),
        "unknown-claim",
    ]);
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("no durable refutation recipe"));
}
