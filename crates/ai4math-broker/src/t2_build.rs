//! Repeatable native Lean build observation, not a proof or promotion receipt.
use super::{
    Broker, CheckMode, Engine, Lane, Pressure, TIMEOUT, Toolchain, breaker_probe,
    dependencies_digest, digest, path_for, stage_source, validate_modules,
};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use std::{
    fs,
    io::{self, Read},
    path::Path,
    time::{Duration, Instant},
};

const TOTAL_TIMEOUT: Duration = Duration::from_secs(120);
const MODULE_ARTIFACT_CAP: u64 = 32 * 1024 * 1024;
const TOTAL_ARTIFACT_CAP: u64 = 128 * 1024 * 1024;
const LEAN_OBJECT_PARTS: &[&str] = &[
    ".olean",
    ".olean.server",
    ".olean.private",
    ".ir.sig",
    ".ir",
];

/// Build metadata for the exact supplied ordered source set and compiler.
/// This does not certify any theorem or publish a reusable object.
#[derive(Debug, Serialize)]
#[must_use]
pub struct LeanBuildAudit {
    pub source_set_sha256: String,
    pub compiler_sha256: String,
    /// Elapsed build time in milliseconds, excluding validation and admission.
    pub compile_ms: u64,
    pub modules: Vec<LeanBuildModuleAudit>,
}

/// Source identity and elapsed time of one native Lean invocation.
#[derive(Debug, Serialize)]
pub struct LeanBuildModuleAudit {
    pub path: String,
    pub source_sha256: String,
    pub compile_ms: u64,
}

fn validate_inputs(tool: &Toolchain, modules: &[(&str, &str)]) -> Result<()> {
    ensure!(
        tool.engine == Engine::Lean4,
        "build audit requires native Lean"
    );
    ensure!(!modules.is_empty(), "empty Lean build audit source set");
    validate_modules(Engine::Lean4, modules, None)?;
    breaker_probe::reject_installation_shadows(tool, modules)
}

fn remaining(deadline: Instant) -> Result<Duration> {
    let time = deadline.saturating_duration_since(Instant::now());
    ensure!(
        !time.is_zero(),
        "Lean build audit wall-clock budget exhausted"
    );
    Ok(time)
}

/// The caller's wall budget includes every phase, even time that does not count
/// toward the independent build budget or compile telemetry.
pub(super) fn check_lean_audit_deadline(deadline: Option<Instant>) -> Result<()> {
    ensure!(
        deadline.is_none_or(|deadline| Instant::now() < deadline),
        "Lean heartbeat audit wall-clock deadline exhausted"
    );
    Ok(())
}

/// Only the current module's expected object parts may reach the read-only
/// predecessor pool. The sandboxed subprocess has exited and cannot fork;
/// the containing room is broker-owned and is never writable by the compiler.
fn collect_module_outputs(
    scratch: &Path,
    objects: &Path,
    module: &str,
    remaining_bytes: &mut u64,
) -> Result<()> {
    ensure!(
        fs::symlink_metadata(scratch)?.is_dir(),
        "invalid Lean output directory"
    );
    let mut module_budget = MODULE_ARTIFACT_CAP;
    let mut has_object = false;
    let mut parts = Vec::with_capacity(LEAN_OBJECT_PARTS.len());
    for entry in fs::read_dir(scratch)? {
        let entry = entry?;
        let name = entry.file_name();
        let suffix = name
            .to_str()
            .and_then(|name| name.strip_prefix(module))
            .filter(|suffix| LEAN_OBJECT_PARTS.contains(suffix))
            .with_context(|| format!("unexpected Lean build output {name:?}"))?;
        ensure!(
            entry.file_type()?.is_file(),
            "non-regular Lean build output {name:?}"
        );
        let mut read = fs::OpenOptions::new();
        read.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            read.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        let file = read.open(entry.path())?;
        let metadata = file.metadata()?;
        ensure!(metadata.is_file(), "non-regular Lean build output {name:?}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            ensure!(metadata.nlink() == 1, "linked Lean build output {name:?}");
        }
        let len = metadata.len();
        module_budget = module_budget
            .checked_sub(len)
            .context("Lean build outputs exceed the 32 MiB per-module limit")?;
        if suffix == ".olean" {
            ensure!(len > 0, "empty Lean build output {name:?}");
            has_object = true;
        }
        parts.push((name, file, len));
    }
    ensure!(has_object, "missing Lean build output {module}.olean");
    let bytes_after_copy = remaining_bytes
        .checked_sub(MODULE_ARTIFACT_CAP - module_budget)
        .context("Lean build outputs exceed the 128 MiB aggregate limit")?;
    for (name, file, len) in parts {
        let mut out = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(objects.join(&name))?;
        // Stream exactly the checked length; at most one extra byte is read
        // to detect growth, and no file-sized buffer is retained by the audit.
        ensure!(
            io::copy(&mut file.take(len + 1), &mut out)? == len,
            "Lean build output changed: {name:?}"
        );
    }
    *remaining_bytes = bytes_after_copy;
    Ok(())
}

impl Broker {
    /// Build the supplied Lean modules in order with the pinned native compiler.
    /// Each invocation sees only its own staged source, the trusted installation,
    /// and the read-only object parts from its ordered predecessors. All scratch
    /// files are removed, and initialization or build errors return no report.
    pub fn audit_lean_build(
        &self,
        tool: &Toolchain,
        modules: &[(&str, &str)],
    ) -> Result<LeanBuildAudit> {
        self.audit_lean_build_with(tool, modules, None::<fn(&str, &Path) -> Result<()>>)
    }

    /// Run broker-owned observations on the exact object prefix ending at each
    /// module. Their elapsed time does not count toward the build's deadline or
    /// compile telemetry. The caller must withhold observations until success.
    pub(super) fn audit_lean_build_with(
        &self,
        tool: &Toolchain,
        modules: &[(&str, &str)],
        after_module: Option<impl FnMut(&str, &Path) -> Result<()>>,
    ) -> Result<LeanBuildAudit> {
        self.audit_lean_build_with_until(tool, modules, after_module, None)
    }

    pub(super) fn audit_lean_build_with_until(
        &self,
        tool: &Toolchain,
        modules: &[(&str, &str)],
        mut after_module: Option<impl FnMut(&str, &Path) -> Result<()>>,
        audit_deadline: Option<Instant>,
    ) -> Result<LeanBuildAudit> {
        let mut build_deadline = Instant::now() + TOTAL_TIMEOUT;
        check_lean_audit_deadline(audit_deadline)?;
        validate_inputs(tool, modules)?;
        check_lean_audit_deadline(audit_deadline)?;
        let _lease = self.resources.acquire(Lane::Heavy, Pressure::Red)?;
        check_lean_audit_deadline(audit_deadline)?;
        remaining(build_deadline)?;
        let build_start = Instant::now();
        let room = tempfile::tempdir().context("create Lean build audit room")?;
        let room_path = room.path().canonicalize()?;
        let predecessors = room_path.join("objects");
        fs::create_dir(&predecessors)?;
        let mut audited = Vec::with_capacity(modules.len());
        let mut remaining_bytes = TOTAL_ARTIFACT_CAP;
        let mut observation_time = Duration::ZERO;
        for &(path, source) in modules {
            check_lean_audit_deadline(audit_deadline)?;
            remaining(build_deadline)?;
            let module = path_for(Engine::Lean4, path)?;
            let scratch = tempfile::tempdir_in(&room_path)?;
            let output = scratch.path().join("output");
            fs::create_dir(&output)?;
            stage_source(Engine::Lean4, scratch.path(), &output, path, source)?;
            let started = Instant::now();
            let timeout = TIMEOUT.min(remaining(build_deadline)?);
            check_lean_audit_deadline(audit_deadline)?;
            let deadline =
                audit_deadline.map(|deadline| deadline.min(build_deadline).min(started + TIMEOUT));
            self.check_with_output_timeout(
                tool,
                scratch.path(),
                &output,
                path,
                CheckMode::IsolatedLean {
                    predecessors: &predecessors,
                    timeout,
                    deadline,
                },
            )
            .with_context(|| format!("build isolated Lean module {path:?}"))?;
            let compile_time = started.elapsed();
            check_lean_audit_deadline(audit_deadline)?;
            ensure!(
                compile_time < timeout,
                "Lean build module {path:?} exceeded its wall-clock budget"
            );
            collect_module_outputs(&output, &predecessors, module, &mut remaining_bytes)
                .with_context(|| format!("collect isolated Lean module {path:?}"))?;
            scratch.close().context("discard Lean module scratch")?;
            audited.push(LeanBuildModuleAudit {
                path: path.to_owned(),
                source_sha256: digest(source.as_bytes()),
                compile_ms: compile_time.as_millis() as u64,
            });
            if let Some(observe) = &mut after_module {
                check_lean_audit_deadline(audit_deadline)?;
                remaining(build_deadline)?;
                let check_start = Instant::now();
                observe(module, &predecessors)?;
                let elapsed = check_start.elapsed();
                observation_time += elapsed;
                build_deadline += elapsed;
                check_lean_audit_deadline(audit_deadline)?;
            }
        }
        room.close().context("discard Lean build audit room")?;
        let source_set_sha256 = dependencies_digest(modules);
        check_lean_audit_deadline(audit_deadline)?;
        remaining(build_deadline)?;
        Ok(LeanBuildAudit {
            source_set_sha256,
            compiler_sha256: tool.binary_sha256.clone(),
            compile_ms: build_start
                .elapsed()
                .saturating_sub(observation_time)
                .as_millis() as u64,
            modules: audited,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AgdaMode, MAX_MODULES, SOURCE_CAP, sandbox};

    fn fixture_tool(install: &Path) -> Toolchain {
        Toolchain {
            engine: Engine::Lean4,
            executable: install.join("unused"),
            reads: vec![install.to_owned()],
            agda_mode: AgdaMode::default(),
            agda_data: None,
            rocq_install: None,
            binary_sha256: String::new(),
            sandbox_group: sandbox::ProcessGroup::OwnSession,
        }
    }

    #[test]
    fn rejects_invalid_input_shape_and_installation_shadows() -> Result<()> {
        let install = tempfile::tempdir()?;
        fs::write(install.path().join("Installed.olean"), b"fixture")?;
        let mut tool = fixture_tool(install.path());
        assert!(validate_inputs(&tool, &[("Valid.lean", "")]).is_ok());
        for modules in [
            &[][..],
            &[("../Valid.lean", "")],
            &[("Valid.v", "")],
            &[("Valid.lean", ""), ("VaLid.lean", "")],
            &[("Init.lean", "")],
            &[("Installed.lean", "")],
            &[("Big.lean", &"a".repeat(SOURCE_CAP + 1))],
            &[("Excess.lean", ""); MAX_MODULES + 1],
        ] {
            assert!(
                validate_inputs(&tool, modules).is_err(),
                "accepted invalid source set"
            );
        }
        tool.engine = Engine::Agda;
        assert!(validate_inputs(&tool, &[("Valid.lean", "")]).is_err());
        Ok(())
    }

    #[test]
    fn expired_build_budget_fails_closed() {
        assert!(remaining(Instant::now()).is_err());
    }

    #[test]
    fn expired_audit_deadline_prevents_build_launch() -> Result<()> {
        let install = tempfile::tempdir()?;
        let tool = fixture_tool(install.path());
        let err = Broker::new()
            .audit_lean_build_with_until(
                &tool,
                &[("Fixture.lean", "def fixture : Nat := 1\n")],
                Some(|_: &str, _: &Path| panic!("observer must not run")),
                Some(Instant::now()),
            )
            .unwrap_err();
        assert!(err.to_string().contains("wall-clock deadline exhausted"));
        Ok(())
    }

    #[test]
    fn enforces_combined_object_budgets_and_expected_output_names() -> Result<()> {
        let scratch = tempfile::tempdir()?;
        let objects = tempfile::tempdir()?;
        let file = scratch.path().join("Fixture.olean");
        fs::write(&file, b"abc")?;
        let part = scratch.path().join("Fixture.olean.private");
        fs::File::create(&part)?.set_len(MODULE_ARTIFACT_CAP)?;
        let mut remaining_bytes = TOTAL_ARTIFACT_CAP;
        let err = collect_module_outputs(
            scratch.path(),
            objects.path(),
            "Fixture",
            &mut remaining_bytes,
        )
        .unwrap_err();
        assert!(err.to_string().contains("per-module limit"));
        assert_eq!(fs::read_dir(objects.path())?.count(), 0);

        let scratch = tempfile::tempdir()?;
        let objects = tempfile::tempdir()?;
        fs::write(scratch.path().join("Fixture.olean"), b"abc")?;
        let mut remaining_bytes = 2;
        let err = collect_module_outputs(
            scratch.path(),
            objects.path(),
            "Fixture",
            &mut remaining_bytes,
        )
        .unwrap_err();
        assert!(err.to_string().contains("aggregate limit"));
        fs::write(scratch.path().join("Other.olean"), b"unexpected")?;
        let mut remaining_bytes = TOTAL_ARTIFACT_CAP;
        let err = collect_module_outputs(
            scratch.path(),
            objects.path(),
            "Fixture",
            &mut remaining_bytes,
        )
        .unwrap_err();
        assert!(err.to_string().contains("unexpected Lean build output"));
        assert!(!objects.path().join("Other.olean").exists());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_and_hard_linked_objects() -> Result<()> {
        let scratch = tempfile::tempdir()?;
        let objects = tempfile::tempdir()?;
        let outside = tempfile::NamedTempFile::new()?;
        let path = scratch.path().join("Fixture.olean");
        std::os::unix::fs::symlink(outside.path(), &path)?;
        let mut remaining_bytes = TOTAL_ARTIFACT_CAP;
        assert!(
            collect_module_outputs(
                scratch.path(),
                objects.path(),
                "Fixture",
                &mut remaining_bytes
            )
            .is_err()
        );
        fs::remove_file(&path)?;
        fs::hard_link(outside.path(), &path)?;
        let err = collect_module_outputs(
            scratch.path(),
            objects.path(),
            "Fixture",
            &mut remaining_bytes,
        )
        .unwrap_err();
        assert!(err.to_string().contains("linked Lean build output"));
        Ok(())
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "set AI4MATH_TEST_LEAN to the native Lean executable and run explicitly"]
    fn t2_build_native_predecessors_only() -> Result<()> {
        let lean = std::env::var_os("AI4MATH_TEST_LEAN")
            .or_else(|| std::env::var_os("AI4MATH_TEST_NATIVE_LEAN"))
            .context("native Lean path")?;
        let tool = Toolchain::lean(lean)?;
        let broker = Broker::new();
        let first = ("Support.lean", "def expected : Nat := 2\n");
        let second = (
            "Target.lean",
            "import Support\ntheorem target : expected = 2 := rfl\n",
        );
        let audit = broker.audit_lean_build(&tool, &[first, second])?;
        assert_eq!(
            audit.source_set_sha256,
            dependencies_digest(&[first, second])
        );
        assert_eq!(audit.compiler_sha256, tool.binary_sha256);
        for (audit, (path, source)) in audit.modules.iter().zip([first, second]) {
            assert_eq!(audit.path, path);
            assert_eq!(audit.source_sha256, digest(source.as_bytes()));
        }
        assert_eq!(audit.modules.len(), 2);
        assert!(broker.audit_lean_build(&tool, &[second, first]).is_err());
        Ok(())
    }
}
