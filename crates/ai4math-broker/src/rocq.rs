//! Native Rocq 9.3 tools, scoped to the committed proof-slot source profile.

use super::{TIMEOUT, Toolchain, golden, path_for, sandbox};
pub(super) use recheck::recheck_generated_objects;

mod recheck;
use ai4math_core::{Engine, FrozenTarget, PreFlightSeverity, scan_source};
use anyhow::{Context, Result, ensure};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::Instant,
};

const PROJECT: &str = "Ai4mathProject";
const CLOSED: &str = "Closed under the global context\n";
pub(super) const AUDIT_MODULE: &str = "Ai4mathRocqAudit";
const AUDIT_SOURCE: &str = "Ai4mathRocqAudit.v";

pub(super) struct Install {
    prefix: PathBuf,
    data: PathBuf,
    runtime: PathBuf,
}

impl Install {
    pub(super) fn resolve(driver: &Path, data: &Path) -> Result<(Self, PathBuf)> {
        let requested_prelude = data
            .join("theories/Init/Prelude.vo")
            .canonicalize()
            .context("expected the trusted Rocq data directory from 'rocq c -where'")?;
        // The public driver normally execs a worker located by mutable findlib
        // search configuration. Resolve only the same operator-trusted install's
        // worker and launch it directly instead; Seatbelt needs no second exec grant.
        let prefix = driver
            .parent()
            .and_then(Path::parent)
            .context("expected a native Rocq driver under its installation bin directory")?;
        ensure!(
            driver.file_name().is_some_and(|name| name == "rocq"),
            "expected a native Rocq 9.3 driver named rocq"
        );
        for relative in ["lib/ocaml/coq", "lib/coq"] {
            let Ok(data) = prefix.join(relative).canonicalize() else {
                continue;
            };
            if !data.starts_with(prefix)
                || data
                    .join("theories/Init/Prelude.vo")
                    .canonicalize()
                    .ok()
                    .as_ref()
                    != Some(&requested_prelude)
                || !requested_prelude.is_file()
            {
                continue;
            }
            // Homebrew's 'rocq c -where' can report a merged symlink tree. Grant
            // the matched, versioned Cellar directories, not that merged tree.
            let runtime = data
                .parent()
                .context("missing Rocq runtime parent")?
                .join("rocq-runtime")
                .canonicalize()
                .context("resolve paired Rocq runtime")?;
            let worker = runtime
                .join("rocqworker")
                .canonicalize()
                .context("resolve paired native Rocq worker")?;
            ensure!(
                runtime.starts_with(prefix)
                    && worker.starts_with(&runtime)
                    && runtime.join("META").is_file()
                    && runtime.join("plugins").is_dir(),
                "expected one complete native Rocq 9.3 installation"
            );
            return Ok((
                Self {
                    prefix: prefix.to_owned(),
                    data,
                    runtime,
                },
                worker,
            ));
        }
        anyhow::bail!("Rocq data does not match the native driver's installation")
    }
}

pub(super) fn validate_source(relative: &str, source: &str) -> Result<()> {
    let report = scan_source(Engine::Rocq, relative, source);
    if let Some(violation) = report
        .violations
        .into_iter()
        .find(|v| v.severity == PreFlightSeverity::Deny)
    {
        return Err(violation.into());
    }
    ensure!(report.passed, "Rocq source admission failed");
    Ok(())
}

fn invoke(
    tool: &Toolchain,
    room: &Path,
    objects: &Path,
    scratch: &Path,
    relative: &str,
    module: &str,
    deadline: Option<Instant>,
) -> Result<String> {
    let install = tool
        .rocq_install
        .as_ref()
        .context("missing pinned Rocq installation")?;
    let mut reads = tool.reads.clone();
    reads.push(install.runtime.join("plugins"));
    let objects = objects.canonicalize()?;
    let source = room.canonicalize()?.join(relative);
    let scratch = scratch.canonicalize()?;
    let object = scratch.join(format!("{module}.vo"));
    let findlib = tempfile::NamedTempFile::new_in(room)?;
    let findlib_path = findlib.path().canonicalize()?;
    let env = [
        ("ROCQRUNTIMELIB", install.runtime.as_path()),
        ("OCAMLFIND_CONF", findlib_path.as_path()),
    ];
    let args = [
        "--kind=compile",
        "-q",
        "-color",
        "no",
        "-native-compiler",
        "no",
        "-bytecode-compiler",
        "no",
        "-noglob",
        "-coqlib",
        install.data.to_str().context("non-UTF8 Rocq data path")?,
        // Rocq derives the library name from the path given to -o. Map the
        // fresh (empty) destination to precisely the already curated import root.
        "-Q",
        scratch
            .to_str()
            .context("non-UTF8 Rocq scratch directory")?,
        PROJECT,
        "-R",
        objects.to_str().context("non-UTF8 Rocq object directory")?,
        PROJECT,
        "-o",
        object.to_str().context("non-UTF8 Rocq object path")?,
        source.to_str().context("non-UTF8 Rocq source path")?,
    ]
    .map(str::to_owned);
    sandbox::run_grouped_until(
        sandbox::Invocation {
            executable: &tool.executable,
            args: &args,
            room,
            writable: &scratch,
            reads: &reads,
            env: &env,
            timeout: TIMEOUT,
        },
        &[install.data.clone(), install.runtime.join("META"), objects],
        // OCaml inspects these exact directory nodes while initializing. In
        // particular, no descendants of the OCaml package root are granted.
        &[
            install.runtime.clone(),
            install
                .runtime
                .parent()
                .context("missing Rocq runtime parent")?
                .to_path_buf(),
            install.runtime.join("kernel"),
        ],
        tool.sandbox_group,
        deadline,
    )
}

pub(super) fn compile(
    tool: &Toolchain,
    room: &Path,
    objects: &Path,
    relative: &str,
    save_object: bool,
) -> Result<String> {
    compile_until(tool, room, objects, relative, save_object, None)
}

fn require_time(deadline: Option<Instant>) -> Result<()> {
    ensure!(
        deadline.is_none_or(|end| Instant::now() < end),
        "Rocq absolute wall-clock budget exhausted"
    );
    Ok(())
}

pub(super) fn compile_until(
    tool: &Toolchain,
    room: &Path,
    objects: &Path,
    relative: &str,
    save_object: bool,
    deadline: Option<Instant>,
) -> Result<String> {
    require_time(deadline)?;
    let module = path_for(Engine::Rocq, relative)?;
    let scratch = tempfile::tempdir_in(room)?;
    let log = invoke(
        tool,
        room,
        objects,
        scratch.path(),
        relative,
        module,
        deadline,
    )
    .with_context(|| format!("compile Rocq module {relative:?}"))?;
    if save_object {
        // Copy only the expected regular object out of the child's writable
        // scratch. All predecessor objects are read-only in every later step.
        let name = format!("{module}.vo");
        let bytes = golden::bounded_file(&scratch.path().join(&name), golden::ARTIFACT_CAP)
            .context("read fresh Rocq compiler object")?;
        require_time(deadline)?;
        ensure!(!bytes.is_empty(), "Rocq compiler produced an empty object");
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(objects.join(name))?
            .write_all(&bytes)?;
    }
    require_time(deadline)?;
    Ok(log)
}

fn audit_source(module: &str, target: &str) -> Result<String> {
    // Independently check the only interpolated target id before generating a
    // command. The module component came from the broker's exact .v path check.
    ensure!(
        target.starts_with(|c: char| c == '_' || c.is_ascii_alphabetic())
            && target != "_"
            && target
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'\'')),
        "invalid frozen Rocq target identifier for audit"
    );
    let qualified = format!("{PROJECT}.{module}");
    Ok(format!(
        "Require {qualified}.\nPrint Assumptions {qualified}.{target}.\n"
    ))
}

fn require_closed(log: &str) -> Result<()> {
    ensure!(
        log == CLOSED,
        "Rocq assumptions audit did not report an unambiguous closed target: {log:?}"
    );
    Ok(())
}

pub(super) fn audit(tool: &Toolchain, objects: &Path, target: &FrozenTarget) -> Result<()> {
    audit_target(tool, objects, &target.file_path, &target.target_id, None)
}

pub(super) fn audit_target(
    tool: &Toolchain,
    objects: &Path,
    relative: &str,
    theorem: &str,
    deadline: Option<Instant>,
) -> Result<()> {
    require_time(deadline)?;
    let module = path_for(Engine::Rocq, relative)?;
    let source = audit_source(module, theorem)?;
    let room = tempfile::tempdir()?;
    let scratch = tempfile::tempdir_in(room.path())?;
    // Only approved, read-only compiled objects enter this invocation. The
    // candidate and dependency compile logs are not audit evidence. This audit
    // checks assumptions, not the statement: exact committed frozen bytes and
    // the restricted input slot determine the statement checked by the compiler.
    fs::write(room.path().join(AUDIT_SOURCE), source)?;
    let log = invoke(
        tool,
        room.path(),
        objects,
        scratch.path(),
        AUDIT_SOURCE,
        AUDIT_MODULE,
        deadline,
    )
    .context("run standalone Rocq assumptions audit")?;
    require_closed(&log)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserved_audit_checks_only_the_qualified_target() -> Result<()> {
        assert_eq!(
            audit_source("Fixture", "target'")?,
            "Require Ai4mathProject.Fixture.\nPrint Assumptions Ai4mathProject.Fixture.target'.\n"
        );
        assert!(audit_source("Fixture", "Other.target").is_err());
        assert!(audit_source("Fixture", "target. Print Assumptions Else").is_err());
        Ok(())
    }

    #[test]
    fn rejects_a_shell_wrapper_before_resolving_rocq_data() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let wrapper = temp.path().join("rocq");
        fs::write(&wrapper, b"#!/bin/sh\nexit 0\n")?;
        let err = Toolchain::rocq(&wrapper, temp.path())
            .err()
            .expect("shell wrapper accepted");
        assert!(err.to_string().contains("native Mach-O"));
        Ok(())
    }

    #[test]
    fn accepts_only_one_exact_zero_assumptions_report() {
        assert!(require_closed(CLOSED).is_ok());
        for log in [
            "",
            "Axioms:\nfixture : True\n",
            "Closed under the global context \n",
            &format!("{CLOSED}{CLOSED}"),
            &format!("{CLOSED}Axioms:\nfixture : True\n"),
            &format!("compiler noise\n{CLOSED}"),
        ] {
            assert!(require_closed(log).is_err());
        }
    }
}
