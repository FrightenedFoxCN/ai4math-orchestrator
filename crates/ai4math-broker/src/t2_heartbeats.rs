//! Advisory command heartbeat observations from an isolated native Lean replay.
use super::t2_build::check_lean_audit_deadline;
use super::{Broker, Engine, LeanBuildAudit, TIMEOUT, Toolchain, path_for, sandbox, stage_source};
use anyhow::{Context, Result, anyhow, ensure};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{self, Read},
    path::Path,
    time::{Duration, Instant},
};

const WRAPPER_PATH: &str = "probe/Ai4mathT2HeartbeatProbe.lean";
const WRAPPER: &str = include_str!("lean/t2_heartbeat_probe.lean");
const SEAL: &str = "ai4math:t2-heartbeats:v1 ";
const MAX_SUMMARY_BYTES: usize = SEAL.len() + 3 * 20 + 3;
// Shared wire limits with the embedded wrapper; keep JSON integers exact.
const MAX_COMMAND_COUNT: u64 = 1_000_000;
const MAX_RAW_HEARTBEATS: u64 = (1 << 53) - 1;
const MAX_COMPILER_BYTES: u64 = 64 * 1024 * 1024;

/// Ordered build metadata and a heartbeat observation for one module.
/// Neither the build nor the observation grants proof or promotion authority.
#[derive(Debug, Serialize)]
#[must_use]
pub struct LeanHeartbeatAudit {
    pub build: LeanBuildAudit,
    pub profile: LeanHeartbeatProfile,
}

/// Per-command heartbeat aggregates in Lean's raw units, not milliseconds.
#[derive(Debug, Serialize)]
#[must_use]
pub struct LeanHeartbeatProfile {
    /// The exact source path in the build snapshot, such as `Fixture.lean`.
    pub target_module: String,
    pub command_count: u64,
    pub max_command_heartbeats_raw: u64,
    pub total_command_heartbeats_raw: u64,
}

fn summary_counter(field: Option<&str>) -> Result<u64> {
    let field = field.context("missing Lean heartbeat counter")?;
    ensure!(
        !field.is_empty()
            && field.len() <= 20
            && field.bytes().all(|byte| byte.is_ascii_digit())
            && (field == "0" || !field.starts_with('0')),
        "malformed Lean heartbeat counter"
    );
    field
        .parse()
        .map_err(|_| anyhow!("Lean heartbeat counter exceeds u64"))
}

fn parse_summary(log: &str, target_module: &str) -> Result<LeanHeartbeatProfile> {
    ensure!(
        log.is_ascii() && log.len() <= MAX_SUMMARY_BYTES,
        "invalid sealed Lean heartbeat summary"
    );
    let mut fields = log
        .strip_prefix(SEAL)
        .and_then(|sealed| sealed.strip_suffix('\n'))
        .context("expected exact sealed Lean heartbeat summary")?
        .split(' ');
    let command_count = summary_counter(fields.next())?;
    let max_command_heartbeats_raw = summary_counter(fields.next())?;
    let total_command_heartbeats_raw = summary_counter(fields.next())?;
    ensure!(fields.next().is_none(), "extra Lean heartbeat output");
    ensure!(command_count > 0, "no Lean commands were profiled");
    ensure!(
        command_count <= MAX_COMMAND_COUNT && total_command_heartbeats_raw <= MAX_RAW_HEARTBEATS,
        "Lean heartbeat summary exceeds its bounds"
    );
    ensure!(
        max_command_heartbeats_raw <= total_command_heartbeats_raw
            && u128::from(total_command_heartbeats_raw)
                <= u128::from(command_count) * u128::from(max_command_heartbeats_raw),
        "inconsistent Lean heartbeat counters"
    );
    Ok(LeanHeartbeatProfile {
        target_module: target_module.to_owned(),
        command_count,
        max_command_heartbeats_raw,
        total_command_heartbeats_raw,
    })
}

fn verify_compiler(path: &Path, expected: &str) -> Result<()> {
    ensure!(
        path.canonicalize()
            .context("resolve pinned Lean compiler")?
            == path,
        "pinned Lean compiler no longer has its original canonical path"
    );
    let mut read = fs::OpenOptions::new();
    read.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        read.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = read.open(path).context("open pinned Lean compiler")?;
    let metadata = file.metadata().context("inspect pinned Lean compiler")?;
    ensure!(
        metadata.is_file(),
        "pinned Lean compiler must be a regular file"
    );
    ensure!(
        metadata.len() <= MAX_COMPILER_BYTES,
        "pinned Lean compiler exceeds the 64 MiB hash limit"
    );
    let mut hasher = Sha256::new();
    let len = io::copy(&mut file.take(MAX_COMPILER_BYTES + 1), &mut hasher)
        .context("hash pinned Lean compiler")?;
    ensure!(
        len == metadata.len() && len <= MAX_COMPILER_BYTES,
        "pinned Lean compiler size changed during hashing"
    );
    ensure!(
        format!("{:x}", hasher.finalize()) == expected,
        "Lean compiler SHA-256 mismatch"
    );
    Ok(())
}

/// Preserve the trusted operation context and native exit status, but never
/// return the sandbox's captured source-controlled diagnostics to the caller.
fn without_child_diagnostics(error: anyhow::Error) -> anyhow::Error {
    let Some(exit) = error.downcast_ref::<sandbox::ChildExit>() else {
        return error;
    };
    let status = anyhow!("sandboxed Lean process exited {}", exit.status);
    match error.chain().next() {
        Some(outer) if !outer.is::<sandbox::ChildExit>() => status.context(outer.to_string()),
        _ => status,
    }
}

fn remaining(deadline: Instant) -> Result<Duration> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    ensure!(
        !remaining.is_zero(),
        "Lean heartbeat probe wall-clock budget exhausted"
    );
    Ok(remaining)
}

fn profile_target(
    tool: &Toolchain,
    objects: &Path,
    target_module: &str,
    module_name: &str,
    source: &str,
    audit_deadline: Option<Instant>,
) -> Result<LeanHeartbeatProfile> {
    let probe_deadline = Instant::now() + TIMEOUT;
    check_lean_audit_deadline(audit_deadline)?;
    let temp = tempfile::tempdir().context("create Lean heartbeat probe room")?;
    let room = temp.path().canonicalize()?;
    let output = room.join("output");
    fs::create_dir(&output)?;
    fs::create_dir(room.join("probe"))?;
    fs::write(room.join(WRAPPER_PATH), WRAPPER)?;
    stage_source(Engine::Lean4, &room, &output, target_module, source)?;
    let args = ["-j1", "--run", WRAPPER_PATH, target_module, module_name].map(str::to_owned);
    // The wrapper and exact source are in the read-only room. Only output is
    // writable. The broker's object prefix is granted as read-only data, not
    // native executable code, and is the only explicit Lean import path.
    check_lean_audit_deadline(audit_deadline)?;
    let log = sandbox::run_grouped_until(
        sandbox::Invocation {
            executable: &tool.executable,
            args: &args,
            room: &room,
            writable: &output,
            reads: &tool.reads,
            env: &[("LEAN_PATH", objects)],
            timeout: remaining(probe_deadline)?,
        },
        &[objects.to_path_buf()],
        &[],
        tool.sandbox_group,
        audit_deadline.map(|deadline| deadline.min(probe_deadline)),
    )
    .map_err(without_child_diagnostics)?;
    // run_grouped merges stdout and stderr without trimming. Every byte must
    // belong to the one summary line; do not search past other source output.
    check_lean_audit_deadline(audit_deadline)?;
    remaining(probe_deadline)?;
    let profile = parse_summary(&log, target_module)?;
    temp.close().context("discard Lean heartbeat probe room")?;
    check_lean_audit_deadline(audit_deadline)?;
    remaining(probe_deadline)?;
    Ok(profile)
}

impl Broker {
    /// Compile the complete ordered snapshot and separately profile one exact
    /// source module. The profile reads only the build's object prefix ending
    /// at that module and does not count toward build time or its deadline.
    pub fn audit_lean_heartbeats(
        &self,
        tool: &Toolchain,
        modules: &[(&str, &str)],
        target_module: &str,
        compiler_sha256: &str,
    ) -> Result<LeanHeartbeatAudit> {
        self.audit_lean_heartbeats_with_deadline(
            tool,
            modules,
            target_module,
            compiler_sha256,
            None,
        )
    }

    /// Like [Self::audit_lean_heartbeats], with an additional absolute wall-clock
    /// deadline covering both build and profiling. It can be shared across a
    /// serial batch; it never resets and does not change compile telemetry.
    pub fn audit_lean_heartbeats_until(
        &self,
        tool: &Toolchain,
        modules: &[(&str, &str)],
        target_module: &str,
        compiler_sha256: &str,
        deadline: Instant,
    ) -> Result<LeanHeartbeatAudit> {
        self.audit_lean_heartbeats_with_deadline(
            tool,
            modules,
            target_module,
            compiler_sha256,
            Some(deadline),
        )
    }

    fn audit_lean_heartbeats_with_deadline(
        &self,
        tool: &Toolchain,
        modules: &[(&str, &str)],
        target_module: &str,
        compiler_sha256: &str,
        audit_deadline: Option<Instant>,
    ) -> Result<LeanHeartbeatAudit> {
        check_lean_audit_deadline(audit_deadline)?;
        ensure!(
            tool.engine == Engine::Lean4,
            "heartbeat audit requires native Lean"
        );
        let target_name = path_for(Engine::Lean4, target_module)?;
        let target_source = modules
            .iter()
            .find_map(|&(path, source)| (path == target_module).then_some(source))
            .context("heartbeat target module must be in the supplied source set")?;
        ensure!(
            compiler_sha256.len() == 64
                && compiler_sha256
                    .bytes()
                    .all(|b| b.is_ascii_digit() || matches!(b, b'a'..=b'f')),
            "expected Lean compiler SHA-256 must be 64 lowercase hex digits"
        );
        ensure!(
            compiler_sha256 == tool.compiler_sha256(),
            "Lean compiler SHA-256 mismatch with the approved toolchain"
        );
        check_lean_audit_deadline(audit_deadline)?;
        verify_compiler(&tool.executable, compiler_sha256)
            .context("verify Lean heartbeat compiler before audit")?;
        check_lean_audit_deadline(audit_deadline)?;

        let mut observation = None;
        let build = self
            .audit_lean_build_with_until(
                tool,
                modules,
                Some(|module: &str, objects: &Path| {
                    if module == target_name {
                        observation = Some(
                            profile_target(
                                tool,
                                objects,
                                target_module,
                                module,
                                target_source,
                                audit_deadline,
                            )
                            .with_context(|| {
                                format!("profile isolated Lean module {target_module:?}")
                            }),
                        );
                    }
                    // An unavailable observation does not skip later modules
                    // as long as the enclosing audit still has time.
                    Ok(())
                }),
                audit_deadline,
            )
            .map_err(without_child_diagnostics);
        check_lean_audit_deadline(audit_deadline)?;
        let pinned = verify_compiler(&tool.executable, compiler_sha256)
            .context("recheck Lean heartbeat compiler after audit");
        check_lean_audit_deadline(audit_deadline)?;
        let build = build?;
        let profile = observation.context("missing Lean heartbeat observation")??;
        pinned?;
        Ok(LeanHeartbeatAudit { build, profile })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_exact_summary_and_checks_aggregate_bounds() -> Result<()> {
        let profile = parse_summary("ai4math:t2-heartbeats:v1 4 10 29\n", "Fixture.lean")?;
        assert_eq!(profile.target_module, "Fixture.lean");
        assert_eq!(profile.command_count, 4);
        assert_eq!(profile.max_command_heartbeats_raw, 10);
        assert_eq!(profile.total_command_heartbeats_raw, 29);
        assert!(parse_summary(&format!("{SEAL}2 0 0\n"), "Fixture.lean").is_ok());
        assert!(
            parse_summary(
                &format!("{SEAL}{MAX_COMMAND_COUNT} {0} {0}\n", MAX_RAW_HEARTBEATS),
                "Fixture.lean"
            )
            .is_ok()
        );
        for invalid in [
            "1000001 0 0",
            "1 9007199254740992 9007199254740992",
            "2 9007199254740991 9007199254740992",
        ] {
            assert!(parse_summary(&format!("{SEAL}{invalid}\n"), "Fixture.lean").is_err());
        }
        for invalid in ["0 0 0", "2 3 2", "1 3 4", "3 2 7", "2 0 1"] {
            assert!(parse_summary(&format!("{SEAL}{invalid}\n"), "Fixture.lean").is_err());
        }
        Ok(())
    }

    #[test]
    fn rejects_noise_and_noncanonical_summary_fields() {
        let good = format!("{SEAL}1 4 4\n");
        for invalid in [
            String::new(),
            format!("{SEAL}unavailable\n"),
            format!("source log\n{good}"),
            format!("{good}source log"),
            format!("{good}{good}"),
            format!("{good}\n"),
            format!("{SEAL}1 4 4\r\n"),
            format!("{SEAL}1 4 4"),
            format!("{SEAL}1  4 4\n"),
            format!("{SEAL}1\t4 4\n"),
            format!("{SEAL}1 4 4 \n"),
            format!("{SEAL}01 4 4\n"),
            format!("{SEAL}+1 4 4\n"),
            format!("{SEAL}1 4 -4\n"),
            format!("{SEAL}1 4 4\0\n"),
            format!("{SEAL}１ 4 4\n"),
            format!("{SEAL}18446744073709551616 4 4\n"),
            format!("{SEAL}Fixture 1 4 4\n"),
        ] {
            let err = parse_summary(&invalid, "Fixture.lean").unwrap_err();
            assert!(!format!("{err:#}").contains("source log"));
        }
    }

    #[test]
    fn detects_changed_compiler_and_expired_deadline() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().canonicalize()?.join("lean");
        fs::write(&path, b"original")?;
        let expected = crate::digest(b"original");
        verify_compiler(&path, &expected)?;
        fs::write(&path, b"modified")?;
        assert!(
            verify_compiler(&path, &expected)
                .unwrap_err()
                .to_string()
                .contains("SHA-256 mismatch")
        );
        fs::OpenOptions::new()
            .write(true)
            .open(&path)?
            .set_len(MAX_COMPILER_BYTES + 1)?;
        assert!(verify_compiler(&path, &expected).is_err());
        assert!(remaining(Instant::now()).is_err());
        Ok(())
    }

    #[test]
    fn expired_audit_deadline_prevents_pin_check_and_profile_launch() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let tool = Toolchain {
            engine: Engine::Lean4,
            executable: temp.path().join("missing-lean"),
            reads: vec![],
            agda_mode: crate::AgdaMode::default(),
            agda_data: None,
            rocq_install: None,
            binary_sha256: crate::digest(b"fixture compiler"),
            sandbox_group: sandbox::ProcessGroup::OwnSession,
        };
        let target = "Fixture.lean";
        let source = "def fixture : Nat := 1\n";
        let deadline = Instant::now();
        let err = Broker::new()
            .audit_lean_heartbeats_until(
                &tool,
                &[(target, source)],
                target,
                tool.compiler_sha256(),
                deadline,
            )
            .unwrap_err();
        assert!(err.to_string().contains("wall-clock deadline exhausted"));
        let err = profile_target(
            &tool,
            temp.path(),
            target,
            "Fixture",
            source,
            Some(deadline),
        )
        .unwrap_err();
        assert!(err.to_string().contains("wall-clock deadline exhausted"));
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn redacts_native_exit_logs_but_preserves_context_and_deadline() {
        use std::os::unix::process::ExitStatusExt;
        let exit = sandbox::ChildExit {
            status: std::process::ExitStatus::from_raw(1 << 8),
            diagnostics: "private source log".to_owned(),
        };
        let error =
            anyhow::Error::new(exit).context(r#"build isolated Lean module "Downstream.lean""#);
        let error = without_child_diagnostics(error);
        assert!(error.to_string().contains("Downstream.lean"));
        assert!(format!("{error:#}").contains("exit status: 1"));
        assert!(!format!("{error:?}").contains("private source log"));
        let timeout = without_child_diagnostics(anyhow!("sandbox timeout after 30s"));
        assert_eq!(timeout.to_string(), "sandbox timeout after 30s");
    }
}
