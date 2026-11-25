//! Operator-only T2 clean build of a pinned managed Lean snapshot, without proof authority.
use crate::{
    bound_engine,
    promotion::{Promotion, PromotionRepo},
    trusted_toolchain,
};
use ai4math_broker::{Broker, LeanBuildAudit, LeanHeartbeatAudit, ManagedLeanTheoremAudit};
use ai4math_core::Engine;
use ai4math_project::ManagedProject;
use ai4math_store::{StateStore, TrustDebtScan};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use std::path::Path;

mod managed_heartbeats;
#[cfg(unix)]
pub(crate) use managed_heartbeats::compare_for_daemon as compare_heartbeats_for_daemon;
pub(crate) use managed_heartbeats::{
    HeartbeatBaseline, ManagedHeartbeatRequest, compare_managed_heartbeats,
};

#[derive(Serialize)]
pub(crate) struct T2BuildReport {
    schema: &'static str,
    reviewed_commit: String,
    ledger_event_seq: i64,
    engine: &'static str,
    external_checker_passed: Option<bool>,
    advisory_only: bool,
    #[serde(flatten)]
    build: LeanBuildAudit,
}

#[derive(Serialize)]
pub(crate) struct ManagedT2Audit {
    schema: &'static str,
    advisory_only: bool,
    reviewed_commit: String,
    ledger_event_seq: i64,
    clean_build: T2BuildReport,
    trust_debt: TrustDebtScan,
}

pub(crate) fn audit(
    root: &Path,
    repo: &Path,
    anchor: &str,
    reviewed: &str,
    executable: &Path,
) -> Result<T2BuildReport> {
    let (build, ()) = audit_inner(
        root,
        repo,
        anchor,
        reviewed,
        |_, _| Ok(()),
        |engine, modules| compile(engine, executable, modules, false),
    )?;
    Ok(build)
}

pub(crate) fn audit_with_debt(
    root: &Path,
    repo: &Path,
    anchor: &str,
    reviewed: &str,
    executable: &Path,
) -> Result<ManagedT2Audit> {
    audit_with_debt_mode(root, repo, anchor, reviewed, executable, false)
}

#[cfg(unix)]
pub(crate) fn audit_for_daemon(
    root: &Path,
    repo: &Path,
    anchor: &str,
    reviewed: &str,
    executable: &Path,
) -> Result<()> {
    crate::t3::arm_daemon_lifeline("T2")?;
    let report = audit_with_debt_mode(root, repo, anchor, reviewed, executable, true)?;
    // This private subprocess protocol never publishes strings from the proof,
    // compiler diagnostics, project paths, or raw Claim/debt details.
    crate::print_json(serde_json::json!({
        "schema": "ai4math.t2.resident-sample.v1",
        "advisory_only": true,
        "reviewed_commit": report.reviewed_commit,
        "ledger_event_seq": report.ledger_event_seq,
        "compile_ms": report.clean_build.build.compile_ms,
        "module_count": report.clean_build.build.modules.len(),
        "debt_findings": report.trust_debt.finding_count,
    }))
}

fn audit_with_debt_mode(
    root: &Path,
    repo: &Path,
    anchor: &str,
    reviewed: &str,
    executable: &Path,
    supervised: bool,
) -> Result<ManagedT2Audit> {
    let (clean_build, trust_debt) = audit_inner(
        root,
        repo,
        anchor,
        reviewed,
        |state, seq| {
            let report = state
                .scan_trust_debt()
                .context("T2 trust debt scan failed")?;
            ensure!(
                report.ledger_event_seq == seq,
                "managed ledger changed before T2 build; retry"
            );
            Ok(report)
        },
        |engine, modules| compile(engine, executable, modules, supervised),
    )?;
    Ok(ManagedT2Audit {
        schema: "ai4math.t2.audit.v1",
        advisory_only: true,
        reviewed_commit: clean_build.reviewed_commit.clone(),
        ledger_event_seq: clean_build.ledger_event_seq,
        clean_build,
        trust_debt,
    })
}

#[derive(Serialize)]
pub(crate) struct ManagedTheoremReport {
    schema: &'static str,
    advisory_only: bool,
    reviewed_commit: String,
    ledger_event_seq: i64,
    target_module: String,
    declaration: String,
    #[serde(flatten)]
    result: ManagedLeanTheoremAudit,
}

pub(crate) struct ManagedTheoremRequest<'a> {
    pub root: &'a Path,
    pub repo: &'a Path,
    pub module: &'a str,
    pub declaration: &'a str,
    pub reviewed: &'a str,
    pub executable: &'a Path,
    pub exporter_sha256: &'a str,
    pub checker_sha256: &'a str,
}

pub(crate) fn audit_managed_theorem(
    request: ManagedTheoremRequest<'_>,
) -> Result<ManagedTheoremReport> {
    let (result, (), ledger_event_seq, _) = at_generation(
        request.root,
        request.repo,
        request.module,
        request.reviewed,
        |_, _| Ok(()),
        |engine, modules| {
            let toolchain = trusted_toolchain(engine, request.executable, None)?;
            Broker::new()
                .audit_lean_theorem(
                    &toolchain,
                    modules,
                    request.module,
                    request.declaration,
                    request.exporter_sha256,
                    request.checker_sha256,
                )
                .context("managed Lean independent theorem check failed")
        },
    )?;
    Ok(ManagedTheoremReport {
        schema: "ai4math.t2.managed-theorem-check.v1",
        advisory_only: true,
        reviewed_commit: request.reviewed.into(),
        ledger_event_seq,
        target_module: request.module.into(),
        declaration: request.declaration.into(),
        result,
    })
}

#[derive(Serialize)]
pub(crate) struct HeartbeatReport {
    schema: &'static str,
    advisory_only: bool,
    reviewed_commit: String,
    ledger_event_seq: i64,
    #[serde(flatten)]
    observation: LeanHeartbeatAudit,
}

pub(crate) fn audit_heartbeats(
    root: &Path,
    repo: &Path,
    module: &str,
    reviewed: &str,
    executable: &Path,
    compiler_sha256: &str,
) -> Result<HeartbeatReport> {
    let (observation, (), ledger_event_seq, _) = at_generation(
        root,
        repo,
        module,
        reviewed,
        |_, _| Ok(()),
        |engine, modules| {
            let toolchain = trusted_toolchain(engine, executable, None)?;
            Broker::new()
                .audit_lean_heartbeats(&toolchain, modules, module, compiler_sha256)
                .context("managed Lean heartbeat observation failed")
        },
    )?;
    Ok(HeartbeatReport::new(
        reviewed,
        ledger_event_seq,
        observation,
    ))
}

impl HeartbeatReport {
    fn new(reviewed: &str, ledger_event_seq: i64, observation: LeanHeartbeatAudit) -> Self {
        Self {
            schema: "ai4math.t2.lean-heartbeats.v1",
            advisory_only: true,
            reviewed_commit: reviewed.into(),
            ledger_event_seq,
            observation,
        }
    }
}

fn audit_inner<T>(
    root: &Path,
    repo: &Path,
    anchor: &str,
    reviewed: &str,
    observe: impl FnOnce(&StateStore, i64) -> Result<T>,
    build: impl FnOnce(Engine, &[(&str, &str)]) -> Result<LeanBuildAudit>,
) -> Result<(T2BuildReport, T)> {
    let (build, observation, ledger_event_seq, engine) =
        at_generation(root, repo, anchor, reviewed, observe, build)?;
    Ok((
        T2BuildReport {
            schema: "ai4math.t2.clean-build.v1",
            reviewed_commit: reviewed.into(),
            ledger_event_seq,
            engine: engine.as_str(),
            external_checker_passed: None,
            advisory_only: true,
            build,
        },
        observation,
    ))
}

pub(crate) fn at_generation<T, B>(
    root: &Path,
    repo: &Path,
    anchor: &str,
    reviewed: &str,
    observe: impl FnOnce(&StateStore, i64) -> Result<T>,
    build: impl FnOnce(Engine, &[(&str, &str)]) -> Result<B>,
) -> Result<(B, T, i64, Engine)> {
    at_snapshot_generation(
        root,
        repo,
        anchor,
        reviewed,
        observe,
        |engine, _, modules| build(engine, modules),
    )
}

pub(crate) fn at_snapshot_generation<T, B>(
    root: &Path,
    repo: &Path,
    anchor: &str,
    reviewed: &str,
    observe: impl FnOnce(&StateStore, i64) -> Result<T>,
    build: impl FnOnce(Engine, &Promotion<'_>, &[(&str, &str)]) -> Result<B>,
) -> Result<(B, T, i64, Engine)> {
    let (result, observation, seq, engine, ()) = at_snapshot_generation_then(
        root,
        repo,
        anchor,
        reviewed,
        observe,
        build,
        |_, _, _, _, _| Ok(()),
    )?;
    Ok((result, observation, seq, engine))
}

/// The finalizer still holds the project native lease and managed Git lock.
/// A finalizer that mutates Git must additionally reserve the ledger generation
/// and use a ref transaction that compares the reviewed main.
pub(crate) fn at_snapshot_generation_then<T, B, F>(
    root: &Path,
    repo: &Path,
    anchor: &str,
    reviewed: &str,
    observe: impl FnOnce(&StateStore, i64) -> Result<T>,
    build: impl FnOnce(Engine, &Promotion<'_>, &[(&str, &str)]) -> Result<B>,
    finish: impl FnOnce(&PromotionRepo, &Path, i64, &B, &T) -> Result<F>,
) -> Result<(B, T, i64, Engine, F)> {
    with_generation(root, repo, anchor, reviewed, |generation| {
        let observation = observe(generation.state, generation.seq())?;
        generation.check_current("managed main or ledger changed before T2 build; retry")?;
        let pinned = generation.git.snapshot(anchor, Some(reviewed))?;
        let files = pinned.project()?;
        let ordered: Vec<_> = files
            .iter()
            .map(|(file, source)| (*file, source.as_str()))
            .collect();
        let build = build(generation.engine, &pinned, &ordered)?;
        generation.check_current("managed main or ledger changed during T2 build; retry")?;
        let finished = finish(
            generation.git,
            generation.path,
            generation.seq(),
            &build,
            &observation,
        )?;
        Ok((
            build,
            observation,
            generation.seq(),
            generation.engine,
            finished,
        ))
    })
}

struct ManagedGeneration<'a> {
    state: &'a StateStore,
    git: &'a PromotionRepo,
    path: &'a Path,
    reviewed: &'a str,
    stamp: &'a (String, i64),
    engine: Engine,
}

impl ManagedGeneration<'_> {
    fn seq(&self) -> i64 {
        self.stamp.1
    }

    fn check_current(&self, reject: &'static str) -> Result<()> {
        // External writers need not honor either project lock.
        ensure!(
            self.git.main_head()? == self.reviewed
                && self.state.managed_wiki_stamp()?.as_ref() == Some(self.stamp),
            "{reject}"
        );
        Ok(())
    }
}

fn with_generation<R>(
    root: &Path,
    repo: &Path,
    anchor: &str,
    reviewed: &str,
    run: impl FnOnce(&ManagedGeneration<'_>) -> Result<R>,
) -> Result<R> {
    let project = ManagedProject::new(root)?;
    let path = project.state_path().to_path_buf();
    let engine = bound_engine(root, anchor)?;
    ensure!(
        engine == Engine::Lean4,
        "T2 clean build currently requires the primary Lean engine"
    );
    let state = project.open_state_readonly()?;
    let stamp = state.managed_wiki_stamp()?.context(
        "bind the managed repository and recover pending Git transactions before T2 build",
    )?;
    let canonical = repo.canonicalize()?;
    ensure!(
        Path::new(&stamp.0) == canonical,
        "project is not bound to the supplied managed repository"
    );
    // Native consumers take the project gate before the managed Git coordination lock.
    let _native = project.try_native_heavy()?;
    let git = PromotionRepo::try_lock_existing(&canonical)?;
    ensure!(
        git.path() == canonical,
        "managed repository moved while obtaining its lock"
    );
    git.require_commit(reviewed)?;
    ensure!(
        git.main_head()? == reviewed,
        "reviewed T2 commit is not current main"
    );
    ensure!(
        state.managed_wiki_stamp()?.as_ref() == Some(&stamp),
        "managed ledger changed before T2 build; retry"
    );

    run(&ManagedGeneration {
        state: &state,
        git: &git,
        path: &path,
        reviewed,
        stamp: &stamp,
        engine,
    })
}

fn compile(
    engine: Engine,
    executable: &Path,
    modules: &[(&str, &str)],
    supervised: bool,
) -> Result<LeanBuildAudit> {
    let toolchain = trusted_toolchain(engine, executable, None)?;
    let toolchain = if supervised {
        toolchain.with_supervised_process_group()?
    } else {
        toolchain
    };
    Broker::new()
        .audit_lean_build(&toolchain, modules)
        .context("tracked Lean clean build failed")
}

#[cfg(all(test, unix))]
mod tests;
