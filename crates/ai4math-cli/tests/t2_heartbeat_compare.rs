use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

fn sample(change: bool, max: u64, total: u64) -> Value {
    let (commit, set, source) = if change {
        ("2", "b", "d")
    } else {
        ("1", "a", "c")
    };
    json!({"schema":"ai4math.t2.lean-heartbeats.v1", "advisory_only":true,
        "reviewed_commit":commit.repeat(40), "ledger_event_seq":21,
        "build":{"source_set_sha256":set.repeat(64), "compiler_sha256":"e".repeat(64), "compile_ms":1,
                 "modules":[{"path":"Audit.lean", "source_sha256":source.repeat(64),"compile_ms":1}]},
        "profile":{"target_module":"Audit.lean", "command_count":3,
                   "max_command_heartbeats_raw":max, "total_command_heartbeats_raw":total}
    })
}

fn run(
    cwd: &Path,
    baseline: &Path,
    candidate: &Path,
    absolute: &str,
    relative: &str,
) -> Result<Output> {
    Ok(Command::new(env!("CARGO_BIN_EXE_ai4math"))
        .current_dir(cwd)
        .arg("t2-compare-heartbeats")
        .arg(baseline)
        .arg(candidate)
        .args([absolute, relative])
        .output()?)
}

#[test]
fn offline_heartbeat_cli_only_emits_a_report_for_valid_comparison_inputs() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let baseline = tmp.path().join("before.json");
    let candidate = tmp.path().join("after.json");
    let before = serde_json::to_vec(&[
        sample(false, 10, 18),
        sample(false, 10, 20),
        sample(false, 9, 17),
    ])?;
    let after = serde_json::to_vec(&[
        sample(true, 11, 19),
        sample(true, 12, 20),
        sample(true, 13, 20),
    ])?;
    fs::write(&baseline, &before)?;
    fs::write(&candidate, &after)?;

    let flagged = run(tmp.path(), &baseline, &candidate, "2", "20")?;
    assert_eq!(flagged.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&flagged.stderr).contains("measured T2 heartbeat increase"));
    let report: Value =
        serde_json::from_slice(&flagged.stdout).context("read regression report")?;
    assert_eq!(report["schema"], "ai4math.t2.heartbeat-comparison.v1");
    assert_eq!(report["advisory_only"], true);
    assert_eq!(report["regressed"], true);
    assert_eq!(report["max_command"]["regressed"], true);
    assert_eq!(report["total_commands"]["regressed"], false);

    let unflagged = run(tmp.path(), &baseline, &candidate, "3", "20")?;
    assert!(unflagged.status.success());
    assert!(unflagged.stderr.is_empty());
    let report: Value = serde_json::from_slice(&unflagged.stdout)?;
    assert_eq!(report["regressed"], false);
    assert_eq!(fs::read(&baseline)?, before);
    assert_eq!(fs::read(&candidate)?, after);
    assert!(!tmp.path().join(".runtime").exists());

    for (absolute, relative) in [
        ("03", "20"),
        ("0", "20"),
        ("9007199254740992", "20"),
        ("1", "1001"),
    ] {
        let bad = run(tmp.path(), &baseline, &candidate, absolute, relative)?;
        assert!(!bad.status.success() && bad.stdout.is_empty());
    }
    fs::write(&candidate, b"not json")?;
    let bad = run(tmp.path(), &baseline, &candidate, "2", "20")?;
    assert!(!bad.status.success() && bad.stdout.is_empty());
    Ok(())
}

#[cfg(unix)]
#[test]
fn offline_heartbeat_cli_rejects_links_directories_and_overlimit_files() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let input = tmp.path().join("input.json");
    let alias = tmp.path().join("alias.json");
    fs::write(&input, serde_json::to_vec(&vec![sample(false, 10, 18); 3])?)?;
    std::os::unix::fs::symlink(&input, &alias)?;
    for invalid in [&alias, tmp.path()] {
        let out = run(tmp.path(), &input, invalid, "2", "20")?;
        assert!(!out.status.success() && out.stdout.is_empty());
        assert!(String::from_utf8_lossy(&out.stderr).contains("regular file"));
    }
    let big = tmp.path().join("big.json");
    fs::File::create(&big)?.set_len(1024 * 1024 + 1)?;
    let out = run(tmp.path(), &input, &big, "2", "20")?;
    assert!(!out.status.success() && out.stdout.is_empty());
    assert!(String::from_utf8_lossy(&out.stderr).contains("1 MiB"));
    assert!(!tmp.path().join(".runtime").exists());
    Ok(())
}
