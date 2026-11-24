//! Operator-only T2 clean build of a pinned managed Lean snapshot, without proof authority.
use crate::{
    bound_engine, managed_native, promotion::PromotionRepo, trusted_toolchain,
    worker_dispatch::private_state_path,
};
use ai4math_broker::{Broker, LeanBuildAudit};
use ai4math_core::Engine;
use ai4math_store::StateStore;
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use std::path::Path;

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

pub(crate) fn audit(
    root: &Path,
    repo: &Path,
    anchor: &str,
    reviewed: &str,
    executable: &Path,
) -> Result<T2BuildReport> {
    let path =
        private_state_path(root).context("T2 build requires an initialized private project")?;
    let engine = bound_engine(root, anchor)?;
    ensure!(
        engine == Engine::Lean4,
        "T2 clean build currently requires the primary Lean engine"
    );
    let state = StateStore::open_readonly(&path)?;
    let stamp = state.managed_wiki_stamp()?.context(
        "bind the managed repository and recover pending Git transactions before T2 build",
    )?;
    let canonical = repo.canonicalize()?;
    ensure!(
        Path::new(&stamp.0) == canonical,
        "project is not bound to the supplied managed repository"
    );
    // Native consumers take the project gate before the managed Git coordination lock.
    let _native = managed_native(root)?;
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

    let pinned = git.snapshot(anchor, Some(reviewed))?;
    let files = pinned.project()?;
    let ordered: Vec<_> = files
        .iter()
        .map(|(file, source)| (*file, source.as_str()))
        .collect();
    let toolchain = trusted_toolchain(engine, executable, None)?;
    let build = Broker::new()
        .audit_lean_build(&toolchain, &ordered)
        .context("tracked Lean clean build failed")?;
    // An external Git writer need not honor our lock. Suppress stale telemetry rather
    // than labeling a changed head or a different ledger generation as the audited state.
    ensure!(
        git.main_head()? == reviewed && state.managed_wiki_stamp()?.as_ref() == Some(&stamp),
        "managed main or ledger changed during T2 build; retry"
    );
    Ok(T2BuildReport {
        schema: "ai4math.t2.clean-build.v1",
        reviewed_commit: reviewed.into(),
        ledger_event_seq: stamp.1,
        engine: engine.as_str(),
        external_checker_passed: None,
        advisory_only: true,
        build,
    })
}
