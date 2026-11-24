#![cfg(unix)]

use ai4math_store::{Change, NewClaim, StateStore, Trust};
use anyhow::Result;
use serde_json::Value;
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Command, Output},
};

const STATE: &str = ".runtime/state.sqlite";
const WAL: &str = ".runtime/state.sqlite-wal";
const SECRET: &str = "Fixture statement is not needed for the inventory";
const HEAD: &str = "1111111111111111111111111111111111111111";

fn invoke(root: &Path, cmd: &str) -> Result<Output> {
    Ok(Command::new(env!("CARGO_BIN_EXE_ai4math"))
        .arg(cmd)
        .arg(root)
        .output()?)
}

#[test]
fn operator_debt_inventory_reports_stable_rows_without_initializing_or_writing() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    let absent = invoke(&root, "t2-trust-debt")?;
    assert!(!absent.status.success() && absent.stdout.is_empty());
    assert!(!root.join(".runtime").exists());
    let init = invoke(&root, "init")?;
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stderr)
    );
    let mut store = StateStore::open(root.join(STATE))?;
    let change = Change {
        actor: "t2-inventory-fixture",
        git_commit: Some(HEAD),
    };
    store.create_claim(
        NewClaim {
            id: "alpha",
            title: "Example",
            informal_statement: SECRET,
            engine: "lean4",
            formal_target: None,
            module_path: None,
            critical: true,
        },
        change,
    )?;
    store.set_local_trust("alpha", Trust::L4a, change)?;
    let head = store.status()?.last_event_seq;
    let read_bytes =
        || -> Result<_> { Ok((fs::read(root.join(STATE))?, fs::read(root.join(WAL))?)) };
    let before = read_bytes()?;
    let pass = invoke(&root, "t2-trust-debt")?;
    assert!(
        pass.status.success(),
        "{}",
        String::from_utf8_lossy(&pass.stderr)
    );
    let report: Value = serde_json::from_slice(&pass.stdout)?;
    assert_eq!(report["schema"], "ai4math.t2.trust-debt-scan.v1");
    assert_eq!(report["advisory_only"], true);
    assert_eq!(report["finding_count"], 2);
    assert_eq!(report["ledger_event_seq"], head);
    assert_eq!(report["findings"][0]["role"], "direct");
    assert_eq!(report["findings"][1]["role"], "effective_ceiling");
    assert!(!String::from_utf8(pass.stdout)?.contains(SECRET));
    assert_eq!(read_bytes()?, before);

    // A shared runtime must not disclose the report, even for an otherwise valid ledger.
    let runtime = root.join(".runtime");
    fs::set_permissions(&runtime, fs::Permissions::from_mode(0o755))?;
    let fail = invoke(&root, "t2-trust-debt")?;
    fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700))?;
    assert!(!fail.status.success() && fail.stdout.is_empty());
    assert!(String::from_utf8(fail.stderr)?.contains("owner-only .runtime directory"));
    assert_eq!(read_bytes()?, before);
    assert_eq!(store.status()?.last_event_seq, head);
    Ok(())
}
