//! Operator entry points for bounded committed Lean smoke checks.
use crate::{
    bound_engine, print_json,
    promotion::{Promotion, PromotionRepo},
};
use ai4math_broker::{Broker, Toolchain, lean_breaker_sources_sha256};
use ai4math_core::Engine;
use ai4math_store::{BreakerProbePlan, BreakerProbeScope, Change, NewTask, StateStore, TaskTrack};
use anyhow::{Context, Result, ensure};
use serde_json::json;
use std::path::Path;

fn repo_name(git: &PromotionRepo) -> Result<&str> {
    git.path().to_str().context("repository path is not UTF-8")
}
fn through<'a>(project: &'a [(&'a str, String)], target: &str) -> Result<Vec<(&'a str, &'a str)>> {
    let idx = project
        .iter()
        .position(|(path, _)| *path == target)
        .context("probe module missing from Git manifest")?;
    Ok(project[..=idx]
        .iter()
        .map(|(name, source)| (*name, source.as_str()))
        .collect())
}
fn reviewed_snapshot<'a>(
    git: &'a PromotionRepo,
    root: &Path,
    scope: &BreakerProbeScope,
) -> Result<Promotion<'a>> {
    ensure!(
        bound_engine(root, &scope.module_path)? == Engine::Lean4,
        "only Lean supports resumed breaker probes"
    );
    let snap = git.snapshot(&scope.module_path, None)?;
    ensure!(
        git.is_ancestor(&scope.adr_commit, snap.base())?,
        "approved ADR does not belong to current main"
    );
    let reviewed_snapshot = git.snapshot(&scope.module_path, Some(&scope.adr_commit))?;
    let reviewed = reviewed_snapshot.project()?;
    let current = snap.project()?;
    ensure!(
        through(&reviewed, &scope.module_path)? == through(&current, &scope.module_path)?,
        "the approved definition module or ordered predecessor sources changed after the ADR; keep the breaker frozen"
    );
    Ok(snap)
}
fn compile_plan(
    root: &Path,
    git: &PromotionRepo,
    store: &mut StateStore,
    id: &str,
    cases: &[&str],
) -> Result<BreakerProbePlan> {
    let scope = store.breaker_probe_scope(repo_name(git)?, id)?;
    let snap = reviewed_snapshot(git, root, &scope)?;
    let project = snap.project()?;
    let hash = lean_breaker_sources_sha256(&through(&project, &scope.module_path)?);
    ensure!(
        git.main_head()? == snap.base(),
        "main changed during probe preview"
    );
    store.preview_breaker_probes(repo_name(git)?, &scope, snap.base(), &hash, cases)
}

pub(crate) fn preview(root: &Path, repo: &Path, id: &str, cases: &[&str]) -> Result<()> {
    let git = PromotionRepo::lock(repo)?;
    let mut store = StateStore::open_readonly(root.join(".runtime/state.sqlite"))?;
    let plan = compile_plan(root, &git, &mut store, id, cases)?;
    print_json(
        json!({"probe_status":"AWAITING_REVIEW","approval_id":plan.approval_id()?,"plan":plan,"verification_performed":false}),
    )
}
pub(crate) fn arm(
    root: &Path,
    repo: &Path,
    id: &str,
    approval: &str,
    cases: &[&str],
) -> Result<()> {
    let git = PromotionRepo::lock(repo)?;
    let mut store = StateStore::open(root.join(".runtime/state.sqlite"))?;
    let p = compile_plan(root, &git, &mut store, id, cases)?;
    ensure!(
        git.main_head()? == p.git_commit,
        "main changed before registering the reviewed probe batch"
    );
    let seq = store.arm_breaker_probes(&p, approval)?;
    print_json(
        json!({"probe_status":"READY","approval_id":approval,"event_seq":seq,"probe_required":cases.len(),"verification_performed":false}),
    )
}
pub(crate) fn run(root: &Path, repo: &Path, id: &str, approval: &str, exe: &Path) -> Result<()> {
    let _native = crate::managed_native(root)?;
    // Holding the coordinating lock across native runs prevents competing main
    // writes and allows an interrupted RUNNING batch to be retried safely.
    let git = PromotionRepo::lock(repo)?;
    let mut store = StateStore::open(root.join(".runtime/state.sqlite"))?;
    let batch = store
        .current_breaker_probe_batch(id)?
        .context("approve a breaker probe batch first")?;
    ensure!(
        batch.approval_id == approval,
        "probe approval is not the latest one for this breaker"
    );
    let p = &batch.plan;
    if batch.status == "PASSED" {
        ensure!(
            store.managed_repo()?.as_deref() == Some(repo_name(&git)?),
            "wrong managed repository"
        );
        ensure!(
            git.is_ancestor(&p.git_commit, &git.main_head()?)?,
            "checked probe is no longer in main history"
        );
        return print_json(
            json!({"probe_status":"PASSED","approval_id":approval,"receipt_sha256":batch.receipt_sha256,"replayed":true,"historical":true}),
        );
    }
    let scope = store.breaker_probe_scope(repo_name(&git)?, id)?;
    let snap = reviewed_snapshot(&git, root, &scope)?;
    ensure!(
        snap.base() == p.git_commit,
        "main moved since probe approval; preview again"
    );
    let project = snap.project()?;
    let sources = through(&project, &scope.module_path)?;
    ensure!(
        lean_breaker_sources_sha256(&sources) == p.sources_sha256,
        "Git source bytes do not match the approved probe plan"
    );
    let attempt = store.start_breaker_probes(id, approval, &git.main_head()?)?;
    let case_names = p.cases.iter().map(String::as_str).collect::<Vec<_>>();
    let result = Toolchain::lean(exe).and_then(|tool| {
        Broker::new().verify_lean_breaker_probes(
            &tool,
            &p.git_commit,
            &scope.module_path,
            &scope.definition,
            &case_names,
            &sources,
        )
    });
    let head = git.main_head()?;
    match result {
        Ok(evidence) if head == p.git_commit => {
            let event_seq = store.finish_breaker_probes(&attempt, &head, Some(&evidence))?;
            print_json(
                json!({"probe_status":"PASSED","breaker_state":"CLOSED","approval_id":approval,"event_seq":event_seq,"receipt_sha256":evidence.receipt_sha256(),"probe_passed":p.cases.len()}),
            )
        }
        other => {
            store.finish_breaker_probes(&attempt, &head, None)?;
            let cause = other
                .err()
                .unwrap_or_else(|| anyhow::anyhow!("main changed during probe verification"));
            Err(cause.context(
                "no verified batch; breaker stays HALF_OPEN and ordinary leases stay blocked",
            ))
        }
    }
}

pub(crate) fn enroll(
    root: &Path,
    repo: &Path,
    id: &str,
    track: TaskTrack,
    module: &str,
    target: &str,
    deps: &[&str],
) -> Result<()> {
    let engine = bound_engine(root, module)?;
    let git = PromotionRepo::lock(repo)?;
    let snap = git.snapshot(module, None)?;
    ensure!(
        git.main_head()? == snap.base(),
        "Git main changed during task registration"
    );
    let mut store = StateStore::open(root.join(".runtime/state.sqlite"))?;
    // Enrolling metadata is not permission to dispatch or run a compiler.
    ensure!(
        store.managed_repo()?.as_deref() == Some(repo_name(&git)?),
        "operator task registration requires an already bound managed repository"
    );
    store.register_task(
        NewTask {
            id,
            track,
            engine: engine.as_str(),
            module_path: module,
            target,
            dependencies: deps,
        },
        Change {
            actor: "operator:task",
            git_commit: None,
        },
    )?;
    print_json(json!({"id":id,"track":track,"dispatched":false}))
}

#[cfg(test)]
mod tests;
