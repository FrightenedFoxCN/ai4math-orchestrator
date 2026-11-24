mod breaker_probe;
pub use breaker_probe::{VerifiedBreakerProbes, lean_breaker_sources_sha256};
mod golden;
mod lean_hammer;
pub use lean_hammer::{
    HammerCandidate, HammerPremises, MAX_HAMMER_DEPENDENCIES, MAX_HAMMER_PREMISES_BYTES,
};
mod lean_spec;
pub use lean_spec::VerifiedLeanComparison;
mod oracle;
pub use oracle::{
    MAX_ORACLE_QUERY_BYTES, OracleObservation, OracleOutcome, OracleQuery, OracleSolvers,
};
mod project_native_gate;
pub use project_native_gate::{ProjectNativeBusy, ProjectNativeGate, ProjectNativeLease};
mod regression_failure;
pub use regression_failure::RegressionFailure;
mod resources;
mod rocq;
pub use golden::{RegressionArtifacts, clone_golden};
mod sandbox;
mod t2_build;
pub use t2_build::{LeanBuildAudit, LeanBuildModuleAudit};
mod t2_nanoda;
pub use t2_nanoda::{LEAN_EXPORT_MAX_BYTES, LeanExportCheck, check_lean_export};
mod t3;
mod task_process;
mod witness;
use resources::{Lane, Pressure, Resources};
pub use task_process::run_task_process;
pub use witness::VerifiedWitness;

pub use resources::BrokerLoad;
pub use sandbox::sandbox_self_test;

use ai4math_core::{Engine, FrozenTarget, ProofBodySubmission, render_proof_body};
use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const TIMEOUT: Duration = Duration::from_secs(30);
const SOURCE_CAP: usize = 2 * 1024 * 1024;
const MAX_MODULES: usize = 128;
const AXIOMS: &[&str] = &["propext", "Classical.choice", "Quot.sound"];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AgdaMode {
    #[default]
    WithoutK,
    Cubical,
}

pub struct Toolchain {
    engine: Engine,
    executable: PathBuf,
    reads: Vec<PathBuf>,
    agda_mode: AgdaMode,
    agda_data: Option<PathBuf>,
    rocq_install: Option<rocq::Install>,
    binary_sha256: String,
    sandbox_group: sandbox::ProcessGroup,
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn file_digest(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)?;
    let mut h = Sha256::new();
    std::io::copy(&mut file, &mut h)?;
    Ok(format!("{:x}", h.finalize()))
}

impl Toolchain {
    fn native_executable(executable: impl AsRef<Path>) -> Result<PathBuf> {
        let executable = executable
            .as_ref()
            .canonicalize()
            .context("resolve native compiler executable")?;
        let mut magic = [0u8; 4];
        fs::File::open(&executable)?.read_exact(&mut magic)?;
        ensure!(
            matches!(
                magic,
                [0xcf, 0xfa, 0xed, 0xfe] | [0xca, 0xfe, 0xba, 0xbe] | [0xca, 0xfe, 0xba, 0xbf]
            ),
            "pass the operator-trusted native Mach-O compiler, not an elan or shell shim"
        );
        Ok(executable)
    }

    fn new(executable: impl AsRef<Path>, engine: Engine, agda_mode: AgdaMode) -> Result<Self> {
        let executable = Self::native_executable(executable)?;
        let binary_sha256 = file_digest(&executable)?;
        Ok(Self {
            executable,
            engine,
            reads: Vec::new(),
            agda_mode,
            agda_data: None,
            rocq_install: None,
            binary_sha256,
            sandbox_group: sandbox::ProcessGroup::OwnSession,
        })
    }

    pub fn lean(executable: impl AsRef<Path>) -> Result<Self> {
        let mut tool = Self::new(executable, Engine::Lean4, AgdaMode::default())?;
        let prefix = tool
            .executable
            .parent()
            .and_then(Path::parent)
            .context("Lean must be in its native toolchain bin directory")?;
        for (relative, required) in [("lib/lean", true), ("lib/glibc", false), ("lib", true)] {
            let path = prefix.join(relative);
            if required || path.exists() {
                tool.reads.push(path.canonicalize()?);
            }
        }
        Ok(tool)
    }

    /// Keep Lean and its sealed comparator in the process group of a guarded
    /// operator coordinator. Their Seatbelt and per-invocation limits are unchanged.
    #[cfg(unix)]
    pub fn with_supervised_process_group(mut self) -> Result<Self> {
        ensure!(
            self.engine == Engine::Lean4,
            "daemon-supervised verification supports Lean only"
        );
        ensure!(
            unsafe { libc::getpgrp() == libc::getpid() },
            "supervised verification requires a dedicated coordinator process group"
        );
        self.sandbox_group = sandbox::ProcessGroup::Supervisor;
        Ok(self)
    }

    pub fn agda(executable: impl AsRef<Path>, mode: AgdaMode) -> Result<Self> {
        let mut tool = Self::new(executable, Engine::Agda, mode)?;
        if let Some(libs) = tool
            .executable
            .parent()
            .and_then(Path::parent)
            .map(|p| p.join("lib"))
            .filter(|p| p.is_dir())
        {
            tool.reads.push(libs.canonicalize()?);
        }
        Ok(tool)
    }

    /// A native Rocq 9.3 driver and matching operator-trusted data ('rocq c -where').
    /// The broker pins and hashes the paired Mach-O compiler worker, not a shell shim.
    pub fn rocq(executable: impl AsRef<Path>, rocq_data: impl AsRef<Path>) -> Result<Self> {
        let driver = Self::native_executable(executable)?;
        let (install, worker) = rocq::Install::resolve(&driver, rocq_data.as_ref())?;
        let mut tool = Self::new(worker, Engine::Rocq, AgdaMode::default())?;
        tool.rocq_install = Some(install);
        Ok(tool)
    }

    /// Operator-granted native libraries; never derive this grant from candidate input.
    pub fn with_runtime_read(mut self, path: impl AsRef<Path>) -> Result<Self> {
        let original = path.as_ref();
        ensure!(
            original.is_absolute()
                && !original
                    .components()
                    .any(|c| matches!(c, std::path::Component::ParentDir)),
            "runtime library grant requires an absolute path without traversal"
        );
        let path = original.canonicalize()?;
        ensure!(
            path.components().count() >= 4,
            "runtime read allowlist must name a specific library installation"
        );
        if self.engine == Engine::Rocq {
            ensure!(
                path.is_file() && path.extension().is_some_and(|ext| ext == "dylib"),
                "Rocq external runtime grants must identify a specific native dylib file"
            );
        }
        self.reads.push(original.to_owned());
        Ok(self)
    }

    pub fn with_agda_data(mut self, path: impl AsRef<Path>) -> Result<Self> {
        ensure!(
            self.engine == Engine::Agda,
            "Agda data has no effect on this engine"
        );
        let path = path.as_ref().canonicalize()?;
        ensure!(
            path.is_dir() && path.join("lib/prim/Agda/Primitive.agda").is_file(),
            "expected the trusted Agda data directory containing lib/prim"
        );
        self.agda_data = Some(path);
        Ok(self)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ReceiptTier {
    T1Promotion,
    T3TaintRebuild,
}

impl ReceiptTier {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::T1Promotion => "T1_PROMOTION",
            Self::T3TaintRebuild => "T3_TAINT_REBUILD",
        }
    }
}

#[derive(Debug, Serialize)]
pub struct VerificationReceipt {
    tier: ReceiptTier,
    issued_unix_seconds: u64,
    compile_seconds: f64,
    engine: Engine,
    target_id: String,
    base_commit: String,
    file_path: String,
    patch_sha256: String,
    dependencies_sha256: String,
    compiler_sha256: String,
    observed_axioms: Vec<String>,
    receipt_sha256: String,
}

impl VerificationReceipt {
    pub fn tier(&self) -> ReceiptTier {
        self.tier
    }
    pub fn issued_unix_seconds(&self) -> u64 {
        self.issued_unix_seconds
    }
    pub fn compile_seconds(&self) -> f64 {
        self.compile_seconds
    }
    pub fn engine(&self) -> Engine {
        self.engine
    }
    pub fn target_id(&self) -> &str {
        &self.target_id
    }
    pub fn file_path(&self) -> &str {
        &self.file_path
    }
    pub fn base_commit(&self) -> &str {
        &self.base_commit
    }
    pub fn patch_sha256(&self) -> &str {
        &self.patch_sha256
    }
    pub fn dependencies_sha256(&self) -> &str {
        &self.dependencies_sha256
    }
    pub fn observed_axioms(&self) -> &[String] {
        &self.observed_axioms
    }
    pub fn receipt_sha256(&self) -> &str {
        &self.receipt_sha256
    }

    pub fn matches(&self, base_commit: &str, file_path: &str, candidate_bytes: &[u8]) -> bool {
        self.base_commit == base_commit
            && self.file_path == file_path
            && self.patch_sha256 == digest(candidate_bytes)
    }
}

#[derive(Debug)]
pub struct VerifiedCandidate {
    source: String,
    receipt: VerificationReceipt,
    // Keep the native verification path sealed and in-memory; the receipt format is durable.
    mode: VerificationMode,
}
impl VerifiedCandidate {
    /// Distinguish typed native T3 assessment from a caller-selected legacy tier.
    pub fn is_assessed_t3(&self) -> bool {
        matches!(self.mode, VerificationMode::AssessT3)
    }

    pub fn source(&self) -> &str {
        &self.source
    }
    pub fn receipt(&self) -> &VerificationReceipt {
        &self.receipt
    }
}

#[derive(Debug, Clone, Copy, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeAssessment {
    Checked,
    Rejected,
    Unavailable,
}

/// Native T3 outcome. Only an attributable proof mismatch is a rejection;
/// unavailable verification never supplies evidence for downgrading a claim.
#[derive(Debug)]
#[must_use]
#[allow(clippy::large_enum_variant)] // Returned individually; avoid boxing the verified candidate.
pub enum RecheckAssessment {
    Verified(VerifiedCandidate),
    Rejected { diagnostics: String },
    Unavailable { diagnostics: String },
}

#[derive(Clone, Copy)]
enum DiagnosticOutput {
    Text,
    LeanJson,
}

enum CheckMode<'a> {
    Default {
        save_object: bool,
        diagnostic_output: DiagnosticOutput,
    },
    IsolatedLean {
        predecessors: &'a PathBuf,
        timeout: Duration,
    },
}

#[derive(Clone, Copy, Debug)]
enum VerificationMode {
    Legacy(ReceiptTier),
    AssessT3,
}

#[derive(Default)]
pub struct Broker {
    resources: Resources,
}

fn path_for(engine: Engine, path: &str) -> Result<&str> {
    let (name, suffix) = path
        .split_once('.')
        .context("expect root-level CaseSensitiveModule.lean, .agda or .v")?;
    ensure!(
        matches!(
            (engine, suffix),
            (Engine::Lean4, "lean") | (Engine::Agda, "agda") | (Engine::Rocq, "v")
        ) && name.starts_with(|ch: char| ch.is_ascii_uppercase())
            && name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_'),
        "unsupported module path {path:?}"
    );
    ensure!(
        engine != Engine::Rocq || !name.eq_ignore_ascii_case(rocq::AUDIT_MODULE),
        "reserved Rocq audit module path {path:?}"
    );
    Ok(name)
}

/// All staged paths are root-level ASCII, so ASCII case folding covers aliases
/// on both case-sensitive and case-insensitive filesystems. Validate the entire
/// set before staging any input (and reserve the later target even if absent).
fn validate_modules(engine: Engine, modules: &[(&str, &str)], target: Option<&str>) -> Result<()> {
    ensure!(
        modules.len() <= MAX_MODULES - usize::from(target.is_some()),
        "too many modules"
    );
    let mut seen = HashMap::new();
    let mut reserve = |relative| {
        let module = path_for(engine, relative)?;
        if let Some(prior) = seen.insert(module.to_ascii_lowercase(), relative) {
            bail!("module name collision: {relative:?} conflicts with {prior:?}");
        }
        Ok(())
    };
    if let Some(relative) = target {
        reserve(relative)?;
    }
    for &(relative, source) in modules {
        reserve(relative)?;
        ensure!(
            source.len() <= SOURCE_CAP,
            "oversized source module {relative:?}"
        );
        if engine == Engine::Rocq {
            rocq::validate_source(relative, source)
                .with_context(|| format!("Rocq preflight for {relative:?}"))?;
        }
    }
    Ok(())
}

fn stage_source(
    engine: Engine,
    room: &Path,
    output: &Path,
    relative: &str,
    source: &str,
) -> Result<()> {
    let dir = if engine == Engine::Agda { output } else { room };
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dir.join(relative))
        .with_context(|| format!("stage fresh source module {relative:?}"))?
        .write_all(source.as_bytes())?;
    Ok(())
}

/// Bind ordered paths and exact bytes without concatenation ambiguity.
fn dependencies_digest(modules: &[(&str, &str)]) -> String {
    let mut hash = Sha256::new();
    hash.update(b"ai4math:ordered-source-dependencies:v1");
    hash.update((modules.len() as u64).to_be_bytes());
    for &(path, source) in modules {
        hash.update((path.len() as u64).to_be_bytes());
        hash.update(path.as_bytes());
        hash.update((source.len() as u64).to_be_bytes());
        hash.update(source.as_bytes());
    }
    format!("{:x}", hash.finalize())
}

fn check_regression_binding(candidate: &VerifiedCandidate, modules: &[(&str, &str)]) -> Result<()> {
    let index = modules
        .iter()
        .position(|(name, source)| {
            *name == candidate.receipt.file_path && *source == candidate.source
        })
        .context("regression must contain the exact verified candidate")?;
    ensure!(
        dependencies_digest(&modules[..index]) == candidate.receipt.dependencies_sha256,
        "regression dependencies differ from the verified source snapshot"
    );
    Ok(())
}

fn lean_axiom_report(output: &str, target: &str) -> Result<Vec<String>> {
    let no = format!("'{target}' does not depend on any axioms");
    let yes = format!("'{target}' depends on axioms: [");
    let mut observations = output.lines().filter_map(|l| {
        if l.trim() == no {
            Some(Ok(Vec::new()))
        } else {
            l.trim().strip_prefix(&yes).map(|l| {
                l.strip_suffix(']')
                    .context("malformed Lean axiom report")
                    .map(|x| x.split(',').map(|a| a.trim().to_owned()).collect())
            })
        }
    });
    let axioms = observations
        .next()
        .context("expected Lean axiom report for the frozen target")??;
    ensure!(observations.next().is_none(), "ambiguous Lean axiom report");
    Ok(axioms)
}

fn lean_axioms(output: &str, target: &str) -> Result<Vec<String>> {
    let axioms = lean_axiom_report(output, target)?;
    ensure!(
        axioms.iter().all(|a| AXIOMS.contains(&a.as_str())),
        "non-L5 Lean axioms: {axioms:?}"
    );
    Ok(axioms)
}

impl Broker {
    pub fn new() -> Self {
        Self::default()
    }

    fn check(
        &self,
        tool: &Toolchain,
        room: &Path,
        output: &Path,
        relative: &str,
        save_object: bool,
    ) -> Result<String> {
        self.check_with_output(
            tool,
            room,
            output,
            relative,
            save_object,
            DiagnosticOutput::Text,
        )
    }

    fn check_with_output(
        &self,
        tool: &Toolchain,
        room: &Path,
        output: &Path,
        relative: &str,
        save_object: bool,
        diagnostic_output: DiagnosticOutput,
    ) -> Result<String> {
        self.check_with_output_timeout(
            tool,
            room,
            output,
            relative,
            CheckMode::Default {
                save_object,
                diagnostic_output,
            },
        )
    }

    fn check_with_output_timeout(
        &self,
        tool: &Toolchain,
        room: &Path,
        output: &Path,
        relative: &str,
        mode: CheckMode<'_>,
    ) -> Result<String> {
        let (save_object, diagnostic_output, timeout, predecessors) = match mode {
            CheckMode::Default {
                save_object,
                diagnostic_output,
            } => (save_object, diagnostic_output, TIMEOUT, &[][..]),
            CheckMode::IsolatedLean {
                predecessors,
                timeout,
            } => {
                ensure!(
                    tool.engine == Engine::Lean4,
                    "isolated build requires native Lean"
                );
                (
                    true,
                    DiagnosticOutput::Text,
                    timeout,
                    std::slice::from_ref(predecessors),
                )
            }
        };
        let module = path_for(tool.engine, relative)?;
        let mut reads = tool.reads.clone();
        let output = output.canonicalize()?;
        let mut env: Vec<(&str, &Path)> = Vec::new();
        let mut args = Vec::<String>::new();
        match tool.engine {
            Engine::Lean4 => {
                let imports = predecessors
                    .first()
                    .map_or(output.as_path(), PathBuf::as_path);
                env.push(("LEAN_PATH", imports));
                if !predecessors.is_empty() {
                    args.extend([
                        "-Ddebug.skipKernelTC=false".into(),
                        "-DElab.async=false".into(),
                    ]);
                }
                if matches!(diagnostic_output, DiagnosticOutput::LeanJson) {
                    args.push("--json".into());
                }
                if save_object {
                    args.extend([
                        "-o".into(),
                        output
                            .join(format!("{module}.olean"))
                            .to_str()
                            .context("non-UTF8 output")?
                            .into(),
                    ]);
                }
                args.push(relative.into());
            }
            Engine::Agda => {
                let data = tool
                    .agda_data
                    .as_deref()
                    .context("native Agda requires a trusted explicit data directory")?;
                reads.push(data.to_path_buf());
                env.push(("Agda_datadir", data));
                args.extend([
                    "--safe".into(),
                    "--no-libraries".into(),
                    "--no-default-libraries".into(),
                    "-i".into(),
                    "output".into(),
                    format!("output/{relative}"),
                ]);
                args.push(
                    match tool.agda_mode {
                        AgdaMode::WithoutK => "--without-K",
                        AgdaMode::Cubical => "--cubical",
                    }
                    .into(),
                );
            }
            Engine::Rocq => return rocq::compile(tool, room, &output, relative, save_object),
        }
        sandbox::run_grouped(
            sandbox::Invocation {
                executable: &tool.executable,
                args: &args,
                room,
                writable: &output,
                reads: &reads,
                env: &env,
                timeout,
            },
            predecessors,
            &[],
            tool.sandbox_group,
        )
    }

    pub fn load(&self) -> Result<BrokerLoad> {
        self.resources.snapshot()
    }

    /// Structured interactive result. Infrastructure failures do not imply a mathematical rejection.
    pub fn assess_probe(
        &self,
        tool: &Toolchain,
        target: &FrozenTarget,
        submission: &ProofBodySubmission,
    ) -> ProbeAssessment {
        match self.probe(tool, target, submission) {
            Ok(_) => ProbeAssessment::Checked,
            Err(e) if e.is::<ai4math_core::ProofBodyError>() => ProbeAssessment::Rejected,
            Err(e)
                if e.downcast_ref::<sandbox::ChildExit>().is_some_and(|exit| {
                    match tool.engine {
                        Engine::Lean4 => {
                            exit.status.code() == Some(1) && exit.diagnostics.contains(": error:")
                        }
                        Engine::Agda => {
                            exit.status.code() == Some(42)
                                && !exit.diagnostics.contains("sandbox-exec:")
                        }
                        Engine::Rocq => false,
                    }
                }) =>
            {
                ProbeAssessment::Rejected
            }
            Err(_) => ProbeAssessment::Unavailable,
        }
    }

    /// Interaction-only check. It never emits a receipt or a reusable artifact.
    pub fn probe(
        &self,
        tool: &Toolchain,
        target: &FrozenTarget,
        submission: &ProofBodySubmission,
    ) -> Result<String> {
        ensure!(
            tool.engine == target.engine,
            "compiler/target engine mismatch"
        );
        path_for(tool.engine, &target.file_path)?;
        let source = render_proof_body(target, submission)?;
        ensure!(source.len() <= SOURCE_CAP, "module source exceeds limit");
        let _lease = self
            .resources
            .acquire(Lane::Fast, resources::host_pressure()?)?;
        let room = tempfile::tempdir()?;
        let output = room.path().join("output");
        fs::create_dir(&output)?;
        stage_source(
            tool.engine,
            room.path(),
            &output,
            &target.file_path,
            &source,
        )?;
        self.check(tool, room.path(), &output, &target.file_path, false)
    }

    pub fn verify(
        &self,
        tool: &Toolchain,
        target: &FrozenTarget,
        submission: &ProofBodySubmission,
        base_commit: &str,
    ) -> Result<VerifiedCandidate> {
        self.verify_with_dependencies(tool, target, submission, base_commit, &[])
    }

    /// Dependencies are immutable, operator-approved source blobs preceding the target.
    pub fn verify_with_dependencies(
        &self,
        tool: &Toolchain,
        target: &FrozenTarget,
        submission: &ProofBodySubmission,
        base_commit: &str,
        dependencies: &[(&str, &str)],
    ) -> Result<VerifiedCandidate> {
        self.verify_at_tier(
            tool,
            target,
            submission,
            base_commit,
            dependencies,
            VerificationMode::Legacy(ReceiptTier::T1Promotion),
        )
    }

    /// Rechecks an existing proof after an operator-approved definition change.
    pub fn recheck_with_dependencies(
        &self,
        tool: &Toolchain,
        target: &FrozenTarget,
        old_proof: &ProofBodySubmission,
        taint_commit: &str,
        dependencies: &[(&str, &str)],
    ) -> Result<VerifiedCandidate> {
        self.verify_at_tier(
            tool,
            target,
            old_proof,
            taint_commit,
            dependencies,
            VerificationMode::Legacy(ReceiptTier::T3TaintRebuild),
        )
    }

    /// Rechecks an existing proof after a definition change. Ambiguous, policy,
    /// dependency and infrastructure failures are unavailable, not rejections.
    /// Only selected native Lean diagnostics are attributable; failed Agda or
    /// Rocq checks currently remain unavailable.
    pub fn assess_recheck_with_dependencies(
        &self,
        tool: &Toolchain,
        target: &FrozenTarget,
        old_proof: &ProofBodySubmission,
        taint_commit: &str,
        dependencies: &[(&str, &str)],
    ) -> RecheckAssessment {
        match self.verify_at_tier(
            tool,
            target,
            old_proof,
            taint_commit,
            dependencies,
            VerificationMode::AssessT3,
        ) {
            Ok(candidate) => RecheckAssessment::Verified(candidate),
            Err(error) => match error.downcast::<t3::TargetCompilerRejection>() {
                Ok(rejection) => RecheckAssessment::Rejected {
                    diagnostics: rejection.0,
                },
                Err(error) => RecheckAssessment::Unavailable {
                    diagnostics: format!("{error:#}"),
                },
            },
        }
    }

    fn verify_at_tier(
        &self,
        tool: &Toolchain,
        target: &FrozenTarget,
        submission: &ProofBodySubmission,
        base_commit: &str,
        dependencies: &[(&str, &str)],
        mode: VerificationMode,
    ) -> Result<VerifiedCandidate> {
        let tier = match mode {
            VerificationMode::Legacy(tier) => tier,
            VerificationMode::AssessT3 => ReceiptTier::T3TaintRebuild,
        };
        let classify_lean =
            matches!(mode, VerificationMode::AssessT3) && tool.engine == Engine::Lean4;
        ensure!(
            matches!(base_commit.len(), 40 | 64)
                && base_commit.bytes().all(|c| c.is_ascii_hexdigit()),
            "expected a full Git base commit hash"
        );
        ensure!(
            tool.engine == target.engine,
            "compiler/target engine mismatch"
        );
        validate_modules(tool.engine, dependencies, Some(&target.file_path))?;
        let source = render_proof_body(target, submission)?;
        ensure!(source.len() <= SOURCE_CAP, "module source exceeds limit");
        let _lease = self.resources.acquire(Lane::Heavy, Pressure::Red)?;
        let room = tempfile::tempdir()?;
        let output = room.path().join("output");
        fs::create_dir(&output)?;
        let start = Instant::now();
        let build = self.build_modules(tool, room.path(), &output, dependencies);
        if matches!(mode, VerificationMode::AssessT3) {
            build.context("build verification dependencies")?;
        } else {
            build?;
        }
        let checked = if tool.engine == Engine::Lean4 {
            format!("{source}\n#print axioms {}\n", target.target_id)
        } else {
            source.clone()
        };
        stage_source(
            tool.engine,
            room.path(),
            &output,
            &target.file_path,
            &checked,
        )?;
        let log = if classify_lean {
            match self.check_with_output(
                tool,
                room.path(),
                &output,
                &target.file_path,
                false,
                DiagnosticOutput::LeanJson,
            ) {
                Ok(log) => t3::axiom_report(&log, target, &source)?,
                Err(error) => {
                    if let Some(rejection) = error
                        .downcast_ref::<sandbox::ChildExit>()
                        .and_then(|exit| t3::target_rejection(exit, target, &source))
                    {
                        return Err(rejection.into());
                    }
                    return Err(error);
                }
            }
        } else {
            self.check(
                tool,
                room.path(),
                &output,
                &target.file_path,
                tool.engine == Engine::Rocq,
            )?
        };
        let observed_axioms = match tool.engine {
            Engine::Lean4 => lean_axioms(&log, &target.target_id)?,
            Engine::Agda => vec![],
            Engine::Rocq => {
                rocq::audit(tool, &output, target)?;
                vec![]
            }
        };
        let mut receipt = VerificationReceipt {
            tier,
            issued_unix_seconds: SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
            compile_seconds: start.elapsed().as_secs_f64(),
            engine: tool.engine,
            target_id: target.target_id.clone(),
            base_commit: base_commit.into(),
            file_path: target.file_path.clone(),
            patch_sha256: digest(source.as_bytes()),
            dependencies_sha256: dependencies_digest(dependencies),
            compiler_sha256: tool.binary_sha256.clone(),
            observed_axioms,
            receipt_sha256: String::new(),
        };
        receipt.receipt_sha256 = digest(&serde_json::to_vec(&receipt)?);
        Ok(VerifiedCandidate {
            source,
            receipt,
            mode,
        })
    }

    pub fn check_modules(&self, tool: &Toolchain, modules: &[(&str, &str)]) -> Result<()> {
        ensure!(!modules.is_empty(), "empty local regression slice");
        validate_modules(tool.engine, modules, None)?;
        let _lease = self.resources.acquire(Lane::Heavy, Pressure::Red)?;
        let room = tempfile::tempdir()?;
        let output = room.path().join("output");
        fs::create_dir(&output)?;
        self.build_modules(tool, room.path(), &output, modules)
    }

    pub fn regress(
        &self,
        tool: &Toolchain,
        candidate: &VerifiedCandidate,
        modules: &[(&str, &str)],
    ) -> Result<RegressionArtifacts> {
        ensure!(
            candidate.receipt.engine == tool.engine
                && candidate.receipt.compiler_sha256 == tool.binary_sha256,
            "regression toolchain does not match receipt"
        );
        validate_modules(tool.engine, modules, None)?;
        check_regression_binding(candidate, modules)?;
        let _lease = self.resources.acquire(Lane::Heavy, Pressure::Red)?;
        let room = tempfile::tempdir()?;
        let output = room.path().join("output");
        fs::create_dir(&output)?;
        let mut downstream = false;
        for &(relative, source) in modules {
            stage_source(tool.engine, room.path(), &output, relative, source)?;
            if tool.engine == Engine::Lean4
                && candidate.receipt.tier == ReceiptTier::T1Promotion
                && downstream
            {
                self.check_with_output(
                    tool,
                    room.path(),
                    &output,
                    relative,
                    true,
                    DiagnosticOutput::LeanJson,
                )
                .map_err(|err| regression_failure::classify(err, candidate, relative, source))?;
            } else {
                self.check(tool, room.path(), &output, relative, true)?;
            }
            downstream |= relative == candidate.receipt.file_path;
        }
        let extension = match tool.engine {
            Engine::Lean4 => "olean",
            Engine::Agda => "agdai",
            Engine::Rocq => "vo",
        };
        let names = modules
            .iter()
            .map(|(name, _)| {
                path_for(tool.engine, name).map(|module| format!("{module}.{extension}"))
            })
            .collect::<Result<Vec<_>>>()?;
        RegressionArtifacts::collect(&output, names, candidate)
    }

    fn build_modules(
        &self,
        tool: &Toolchain,
        room: &Path,
        output: &Path,
        modules: &[(&str, &str)],
    ) -> Result<()> {
        // All callers validate the complete set (including any reserved target)
        // before acquiring resources or creating the clean room.
        // A module sees only its predecessors; later regression modules must
        // not widen the verified target's original import closure (notably in Agda).
        for &(relative, source) in modules {
            stage_source(tool.engine, room, output, relative, source)?;
            self.check(tool, room, output, relative, true)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_non_kernel_axioms_and_missing_report() {
        assert_eq!(
            lean_axioms("'p' does not depend on any axioms\n", "p").unwrap(),
            Vec::<String>::new()
        );
        assert!(lean_axioms("'p' depends on axioms: [propext, Quot.sound]", "p").is_ok());
        assert!(lean_axioms("'p' depends on axioms: [sorryAx]", "p").is_err());
        assert!(lean_axioms("'other' does not depend on any axioms", "p").is_err());
    }
    pub(super) fn candidate_fixture(dependencies: &[(&str, &str)]) -> VerifiedCandidate {
        let source = "theorem fixture : True := True.intro\n".to_owned();
        VerifiedCandidate {
            receipt: VerificationReceipt {
                tier: ReceiptTier::T1Promotion,
                issued_unix_seconds: 0,
                compile_seconds: 0.0,
                engine: Engine::Lean4,
                target_id: "fixture".into(),
                base_commit: "a".repeat(40),
                file_path: "Fixture.lean".into(),
                patch_sha256: digest(source.as_bytes()),
                dependencies_sha256: dependencies_digest(dependencies),
                compiler_sha256: "b".repeat(64),
                observed_axioms: Vec::new(),
                receipt_sha256: "c".repeat(64),
            },
            source,
            mode: VerificationMode::Legacy(ReceiptTier::T1Promotion),
        }
    }

    #[test]
    fn t3_tier_does_not_confer_the_typed_assessment_seal() {
        let mut candidate = candidate_fixture(&[]);
        assert!(!candidate.is_assessed_t3());
        candidate.receipt.tier = ReceiptTier::T3TaintRebuild;
        candidate.mode = VerificationMode::Legacy(ReceiptTier::T3TaintRebuild);
        assert!(!candidate.is_assessed_t3());
        candidate.mode = VerificationMode::AssessT3;
        assert!(candidate.is_assessed_t3());
    }

    #[test]
    fn denies_exact_and_case_fold_module_collisions_including_the_target() {
        for (engine, ext) in [(Engine::Lean4, "lean"), (Engine::Agda, "agda")] {
            let upper = format!("MathCore.{ext}");
            let folded = format!("Mathcore.{ext}");
            let distinct = format!("MathCore2.{ext}");
            for colliding in [&upper, &folded] {
                let standalone = [(upper.as_str(), ""), (colliding.as_str(), "")];
                let dependency = [(colliding.as_str(), "")];
                for err in [
                    validate_modules(engine, &standalone, None).unwrap_err(),
                    validate_modules(engine, &dependency, Some(&upper)).unwrap_err(),
                ] {
                    assert!(err.to_string().contains("module name collision"));
                }
            }
            assert!(validate_modules(engine, &[(&distinct, "")], Some(&upper)).is_ok());
        }
    }

    #[test]
    fn staged_source_is_never_overwritten() -> Result<()> {
        let room = tempfile::tempdir()?;
        let output = room.path().join("output");
        fs::create_dir(&output)?;
        let relative = "Fixture.lean";
        let original = "theorem p : True := True.intro\n";
        stage_source(Engine::Lean4, room.path(), &output, relative, original)?;
        assert!(stage_source(Engine::Lean4, room.path(), &output, relative, "").is_err());
        assert_eq!(fs::read_to_string(room.path().join(relative))?, original);
        Ok(())
    }

    #[test]
    fn regression_preserves_ordered_dependency_paths_and_bytes() -> Result<()> {
        let a = ("First.lean", "def one := 1\n");
        let b = ("Second.lean", "def two := 2\n");
        let candidate = candidate_fixture(&[a, b]);
        let target = (candidate.receipt().file_path(), candidate.source());
        let later = ("Later.lean", "theorem p : True := True.intro\n");
        check_regression_binding(&candidate, &[a, b, target, later])?;
        for modified in [
            [b, a, target],
            [("FirST.lean", a.1), b, target],
            [(a.0, "def one := 1\r\n"), b, target],
            [a, b, (target.0, "")],
        ] {
            assert!(check_regression_binding(&candidate, &modified).is_err());
        }
        assert!(check_regression_binding(&candidate, &[a, target, b]).is_err());
        Ok(())
    }

    #[test]
    fn rejects_unsafe_paths() {
        assert!(path_for(Engine::Lean4, "Main.lean").is_ok());
        assert!(path_for(Engine::Lean4, "../Main.lean").is_err());
        assert!(path_for(Engine::Rocq, "Main.v").is_ok());
        assert!(path_for(Engine::Rocq, "Module.Inner.v").is_err());
        assert!(path_for(Engine::Rocq, "Ai4mathRocqAudit.v").is_err());
        assert!(path_for(Engine::Rocq, "Ai4MathRocqAudit.v").is_err());
    }
}
