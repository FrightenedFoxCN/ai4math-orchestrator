#![cfg(unix)]

use ai4math_broker::{ProjectNativeBusy, ProjectNativeGate};
use ai4math_store::StateStore;
use anyhow::{Context, Result};
use std::process::{Command, Output};

fn ai(args: &[&str]) -> Result<Output> {
    Ok(Command::new(env!("CARGO_BIN_EXE_ai4math"))
        .args(args)
        .output()?)
}

#[test]
fn all_managed_native_commands_refuse_busy_project_without_mutation() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().to_str().context("UTF8 fixture")?;
    assert!(ai(&["init", root])?.status.success());
    let gate = ProjectNativeGate::new(dir.path());
    let state = StateStore::open_readonly(dir.path().join(".runtime/state.sqlite"))?;
    let seq = state.status()?.last_event_seq;
    let text = ProjectNativeBusy.to_string();
    let script = "/missing/executable";
    let input = "/missing/input";
    let no_repo = "/missing/repository";
    let oid = "a".repeat(40);
    let hash = "b".repeat(64);
    let matrix: &[&[&str]] = &[
        &[
            "promote-managed",
            root,
            no_repo,
            "claim",
            "Goal.lean",
            "target",
            script,
            input,
        ],
        &[
            "promote-reviewed-body",
            root,
            no_repo,
            "claim",
            "Goal.lean",
            "target",
            &oid,
            script,
            input,
        ],
        &["recover-promotion", root, no_repo, script],
        &[
            "refute-managed",
            root,
            no_repo,
            "claim",
            "Goal.lean",
            "target",
            script,
            input,
        ],
        &["t3-recheck", root, no_repo, &hash, script],
        &[
            "breaker-probes-run",
            root,
            no_repo,
            "breaker",
            &hash,
            script,
        ],
        &[
            "hammer-reconstruct",
            root,
            no_repo,
            "claim",
            &oid,
            input,
            script,
        ],
        &[
            "oracle-retain",
            root,
            no_repo,
            "claim",
            &hash,
            input,
            script,
            script,
        ],
        &[
            "task-run",
            root,
            no_repo,
            "task",
            "claim",
            "prover_worker",
            script,
            &hash,
            "5",
        ],
    ];
    // One active fast request must exclude a heavyweight launch in another PID.
    let fast = gate.try_fast()?;
    for args in matrix {
        let output = ai(args)?;
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !output.status.success() && stderr.contains(&text),
            "{}: {stderr}",
            args[0]
        );
    }
    for cmd in ["status", "tasks-status"] {
        let output = ai(&[cmd, root])?;
        assert!(
            output.status.success(),
            "{cmd} read must not take a native lease: {:?}",
            output
        );
    }
    assert_eq!(state.status()?.last_event_seq, seq);
    assert!(state.promotion_intent()?.is_none());
    drop(fast);
    let output = ai(&[
        "hammer-reconstruct",
        root,
        no_repo,
        "claim",
        &oid,
        input,
        script,
    ])?;
    assert!(!output.status.success());
    assert!(!String::from_utf8_lossy(&output.stderr).contains(&text));
    Ok(())
}
