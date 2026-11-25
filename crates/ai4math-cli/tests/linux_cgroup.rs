//! CLI report must never claim verification or acceptance from a fixture.
use std::process::Command;

#[test]
fn cgroup_preflight_fails_closed_on_non_cgroup_directory() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    for (name, contents) in [
        ("memory.max", "1073741824\n"),
        ("pids.max", "8\n"),
        ("cpu.max", "50000 100000\n"),
        ("cgroup.type", "domain\n"),
        ("cgroup.events", "populated 0\nfrozen 0\n"),
        ("cgroup.stat", "nr_descendants 0\nnr_dying_descendants 0\n"),
        ("cgroup.kill", ""),
    ] {
        std::fs::write(dir.path().join(name), contents)?;
    }
    let leaf = dir.path().canonicalize()?;
    let output = Command::new(env!("CARGO_BIN_EXE_ai4math"))
        .arg("linux-cgroup-preflight")
        .arg(&leaf)
        .output()?;
    assert!(
        !output.status.success(),
        "ordinary files are not cgroup controls"
    );
    assert!(
        output.stdout.is_empty(),
        "failed probes must not publish a readiness report"
    );
    assert_eq!(std::fs::read(leaf.join("cgroup.kill"))?, b"");
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires an operator-provisioned, empty cgroup v2 leaf with finite limits"]
fn native_cgroup_preflight_reports_limits_without_enabling_verification() -> anyhow::Result<()> {
    let leaf = std::env::var_os("AI4MATH_TEST_CGROUP_LEAF")
        .ok_or_else(|| anyhow::anyhow!("set AI4MATH_TEST_CGROUP_LEAF to a provisioned leaf"))?;
    let output = Command::new(env!("CARGO_BIN_EXE_ai4math"))
        .arg("linux-cgroup-preflight")
        .arg(leaf)
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(report["schema"], "ai4math.linux-cgroup-preflight.v1");
    assert_eq!(report["mode"], "READ_ONLY");
    assert_eq!(report["backend_enabled"], false);
    assert_eq!(report["delegation_verified"], false);
    for field in [
        "memory_max_bytes",
        "pids_max",
        "cpu_quota_us",
        "cpu_period_us",
    ] {
        assert!(report["limits"][field].as_u64().unwrap_or(0) > 0, "{field}");
    }
    assert_eq!(report["limits"]["cgroup_kill_present"], true);
    Ok(())
}
