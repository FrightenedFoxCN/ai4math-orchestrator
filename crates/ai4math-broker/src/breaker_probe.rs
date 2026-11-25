//! Native Lean regression checks for operator-approved, committed probes.
use super::{
    Broker, Engine, Lane, Pressure, TIMEOUT, Toolchain, dependencies_digest, digest,
    lean_objects::LeanObjects, path_for, sandbox, stage_source, validate_modules,
};
use ai4math_evidence::BreakerProbeEvidence;
use anyhow::{Context, Result, ensure};
use std::{
    fs,
    path::Path,
    time::{Duration, Instant},
};

pub(super) use super::lean_kernel::reject_installation_shadows;

const CHECKER_FILE: &str = "Ai4mathSealedBreakerProbe.lean";
const CHECKER: &str = include_str!("breaker_probe_audit.lean");
const SEAL: &str = "ai4math:sealed-breaker-probe-audit:v1\n";
pub(super) const TOTAL_TIMEOUT: Duration = Duration::from_secs(90);
const ARTIFACT_CAP: u64 = 16 * 1024 * 1024;
const TOTAL_ARTIFACT_CAP: u64 = 64 * 1024 * 1024;

/// Sealed, all-or-nothing evidence for the named committed Lean probe set.
/// Only the broker can construct this evidence after native verification.
#[derive(Debug)]
#[must_use]
pub struct VerifiedBreakerProbes {
    base_commit: String,
    file_path: String,
    definition: String,
    cases: Vec<String>,
    sources_sha256: String,
    receipt_sha256: String,
}

impl VerifiedBreakerProbes {
    pub fn base_commit(&self) -> &str {
        &self.base_commit
    }
    pub fn file_path(&self) -> &str {
        &self.file_path
    }
    pub fn definition(&self) -> &str {
        &self.definition
    }
    pub fn cases(&self) -> &[String] {
        &self.cases
    }
    pub fn sources_sha256(&self) -> &str {
        &self.sources_sha256
    }
    pub fn receipt_sha256(&self) -> &str {
        &self.receipt_sha256
    }
}

// SAFETY: Production values are constructed only after the committed Lean source
// builds and the sealed native kernel checker returns the exact audit seal.
unsafe impl BreakerProbeEvidence for VerifiedBreakerProbes {
    fn base_commit(&self) -> &str {
        self.base_commit()
    }

    fn file_path(&self) -> &str {
        self.file_path()
    }

    fn definition(&self) -> &str {
        self.definition()
    }

    fn cases(&self) -> &[String] {
        self.cases()
    }

    fn sources_sha256(&self) -> &str {
        self.sources_sha256()
    }

    fn receipt_sha256(&self) -> &str {
        self.receipt_sha256()
    }
}

/// Digest of the exact source paths and UTF-8 bytes in verification order.
pub fn lean_breaker_sources_sha256(sources: &[(&str, &str)]) -> String {
    dependencies_digest(sources)
}

fn receipt_digest(
    base_commit: &str,
    file_path: &str,
    definition: &str,
    cases: &[&str],
    sources_sha256: &str,
    compiler_sha256: &str,
) -> Result<String> {
    Ok(digest(&serde_json::to_vec(&(
        "ai4math:lean-breaker-probes:v1",
        base_commit,
        file_path,
        definition,
        cases,
        sources_sha256,
        CHECKER_FILE,
        digest(CHECKER.as_bytes()),
        compiler_sha256,
    ))?))
}

pub(super) fn identifier(name: &str) -> bool {
    name != "_"
        && name.starts_with(|c: char| c == '_' || c.is_ascii_alphabetic())
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'\''))
}

fn validate_inputs<'a>(
    tool: &Toolchain,
    base_commit: &str,
    file_path: &'a str,
    definition: &str,
    cases: &[&str],
    sources: &[(&str, &str)],
) -> Result<&'a str> {
    ensure!(
        tool.engine == Engine::Lean4,
        "breaker probes require native Lean"
    );
    ensure!(
        matches!(base_commit.len(), 40 | 64) && base_commit.bytes().all(|c| c.is_ascii_hexdigit()),
        "expected a full Git base commit hash"
    );
    ensure!(
        identifier(definition),
        "expected a root Lean definition name"
    );
    ensure!(
        matches!(cases, [_]) || matches!(cases, [a, b] if a != b),
        "expected one or two distinct Lean probe theorems"
    );
    ensure!(
        cases.iter().all(|name| name.split('.').all(identifier)),
        "expected exact Lean theorem names"
    );
    let module = path_for(Engine::Lean4, file_path)?;
    ensure!(
        sources.last().is_some_and(|&(path, _)| path == file_path),
        "watched module must be last in the committed source slice"
    );
    validate_modules(Engine::Lean4, sources, Some(CHECKER_FILE))?;
    reject_installation_shadows(tool, sources)?;
    Ok(module)
}

pub(super) fn command_timeout(deadline: Instant) -> Result<Duration> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    ensure!(
        !remaining.is_zero(),
        "isolated Lean audit wall-clock budget exhausted"
    );
    Ok(TIMEOUT.min(remaining))
}

/// Run each source with only its own source and read-only predecessor objects.
/// When requested, preserve a second read-only view before later modules exist.
pub(super) fn build_committed_sources(
    tool: &Toolchain,
    room: &Path,
    sources: &[(&str, &str)],
    deadline: Instant,
    prefix: Option<(usize, &Path)>,
) -> Result<()> {
    let objects = room.join("objects");
    let mut pool = LeanObjects::create(&objects, ARTIFACT_CAP, TOTAL_ARTIFACT_CAP)?;
    for (index, &(path, source)) in sources.iter().enumerate() {
        let module = path_for(Engine::Lean4, path)?;
        pool.compile(path, source, true, |room, writable, predecessors| {
            let object = writable.join(format!("{module}.olean"));
            let log = sandbox::run_with_data(
                sandbox::Invocation {
                    executable: &tool.executable,
                    args: &[
                        "-Ddebug.skipKernelTC=false".into(),
                        "-DElab.async=false".into(),
                        "-o".into(),
                        object.to_str().context("non-UTF-8 output path")?.into(),
                        path.into(),
                    ],
                    room,
                    writable,
                    reads: &tool.reads,
                    env: &[("LEAN_PATH", predecessors)],
                    timeout: command_timeout(deadline)?,
                },
                std::slice::from_ref(predecessors),
                &[],
            )
            .with_context(|| format!("build isolated Lean module {path:?}"))?;
            command_timeout(deadline)?;
            Ok(log)
        })?;
        if let Some((len, snapshot)) = prefix.filter(|(len, _)| *len == index + 1) {
            debug_assert_eq!(len, index + 1);
            fs::create_dir(snapshot)?;
            for entry in fs::read_dir(&objects)? {
                let entry = entry?;
                fs::hard_link(entry.path(), snapshot.join(entry.file_name()))?;
            }
        }
    }
    Ok(())
}

impl Broker {
    /// Verify one or two committed Lean theorem declarations in a clean room.
    /// The trusted caller supplies immutable bytes from the named commit in
    /// build order, with the watched module last; no branch or checkout is read
    /// by the broker. The cases must directly mention the exact watched root
    /// definition in their elaborated statements and pass kernel/axiom checks.
    pub fn verify_lean_breaker_probes(
        &self,
        tool: &Toolchain,
        base_commit: &str,
        file_path: &str,
        definition: &str,
        cases: &[&str],
        sources: &[(&str, &str)],
    ) -> Result<VerifiedBreakerProbes> {
        let module = validate_inputs(tool, base_commit, file_path, definition, cases, sources)?;
        let deadline = Instant::now() + TOTAL_TIMEOUT;
        let _lease = self.resources.acquire(Lane::Heavy, Pressure::Red)?;

        let build = tempfile::tempdir()?;
        let build_path = build.path().canonicalize()?;
        build_committed_sources(tool, &build_path, sources, deadline, None)?;
        let objects = build_path.join("objects");
        let audit = tempfile::tempdir()?;
        let writable = audit.path().join("scratch");
        fs::create_dir(&writable)?;
        stage_source(
            Engine::Lean4,
            audit.path(),
            &writable,
            CHECKER_FILE,
            CHECKER,
        )?;
        let mut reads = tool.reads.clone();
        reads.push(objects.clone());
        let mut args = vec![
            "--run".into(),
            CHECKER_FILE.into(),
            objects.to_str().context("non-UTF-8 object path")?.into(),
            module.into(),
            definition.into(),
        ];
        args.extend(cases.iter().map(|name| (*name).to_owned()));
        // No LEAN_PATH on startup: only the installed Lean modules can supply
        // syntax or run import hooks while the broker checker is elaborated.
        let log = sandbox::run(sandbox::Invocation {
            executable: &tool.executable,
            args: &args,
            room: audit.path(),
            writable: &writable,
            reads: &reads,
            env: &[],
            timeout: command_timeout(deadline)?,
        })
        .context("kernel-check sealed Lean breaker probes")?;
        command_timeout(deadline)?;
        ensure!(
            log == SEAL,
            "missing or inexact sealed Lean breaker audit: {log:?}"
        );

        let sources_sha256 = lean_breaker_sources_sha256(sources);
        let receipt_sha256 = receipt_digest(
            base_commit,
            file_path,
            definition,
            cases,
            &sources_sha256,
            &tool.binary_sha256,
        )?;
        Ok(VerifiedBreakerProbes {
            base_commit: base_commit.into(),
            file_path: file_path.into(),
            definition: definition.into(),
            cases: cases.iter().map(|name| (*name).to_owned()).collect(),
            sources_sha256,
            receipt_sha256,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deadline_budget_fails_closed() -> Result<()> {
        assert_eq!(command_timeout(Instant::now() + TOTAL_TIMEOUT)?, TIMEOUT);
        assert!(command_timeout(Instant::now()).is_err());
        Ok(())
    }

    #[test]
    fn receipt_binds_ordered_sources_and_case_names() -> Result<()> {
        let first = ("First.lean", "def first := 0\n");
        let second = ("Second.lean", "def second := 0\n");
        let target = ("Target.lean", "def watched := 0\n");
        let sources = lean_breaker_sources_sha256(&[first, second, target]);
        assert_ne!(
            sources,
            lean_breaker_sources_sha256(&[second, first, target])
        );
        assert_ne!(
            sources,
            lean_breaker_sources_sha256(&[first, second, (target.0, "def watched := 0\r\n")])
        );
        let commit = "a".repeat(40);
        let compiler = "b".repeat(64);
        let cases = ["firstCase", "secondCase"];
        let id = receipt_digest(&commit, target.0, "watched", &cases, &sources, &compiler)?;
        assert_eq!(
            id,
            receipt_digest(&commit, target.0, "watched", &cases, &sources, &compiler)?
        );
        assert_ne!(
            id,
            receipt_digest(
                &commit,
                target.0,
                "watched",
                &[cases[1], cases[0]],
                &sources,
                &compiler
            )?
        );
        assert_ne!(
            id,
            receipt_digest(
                &commit,
                target.0,
                "watched",
                &cases,
                &sources,
                &"c".repeat(64)
            )?
        );
        Ok(())
    }
}
