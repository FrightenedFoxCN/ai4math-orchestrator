//! One-shot advisory observation of managed main against an explicit baseline
//! or one Claim's current managed T1 receipt. Neither path confers approval.
use super::{Plan, ProjectDelta, bound_engine, predecessors, print_json, validate_recipe};
use crate::{promotion::PromotionRepo, reviewed_spec, worker_dispatch::private_state_path};
use ai4math_broker::lean_breaker_sources_sha256;
use ai4math_core::Engine;
use ai4math_store::StateStore;
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use std::path::Path;

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum Status {
    Unchanged,
    StructuralChange,
    Diverged,
}

#[derive(Serialize)]
struct Observation {
    status: Status,
    base_commit: String,
    observed_head: String,
    ledger_event_seq: i64,
    changed_modules: Vec<String>,
    module_order_changed: bool,
    plan: Option<Plan>,
    advisory_only: bool,
}

#[derive(Serialize)]
pub(crate) struct ClaimDiscovery {
    baseline_claim: String,
    baseline_receipt_sha256: String,
    baseline_scope: &'static str,
    #[serde(flatten)]
    observation: Observation,
}

pub(crate) fn discover(root: &Path, repo: &Path, anchor: &str, baseline: &str) -> Result<()> {
    print_json(observe(root, repo, anchor, baseline, false)?)
}

pub(crate) fn discover_claim(root: &Path, repo: &Path, claim: &str) -> Result<()> {
    print_json(discover_claim_observation(root, repo, claim)?)
}

pub(crate) fn discover_claim_observation(
    root: &Path,
    repo: &Path,
    claim: &str,
) -> Result<ClaimDiscovery> {
    let mut session = DiscoverySession::open(root, repo, true)?;
    let evidence = session
        .store
        .t3_discovery_baseline(claim)?
        .context("no current exact managed Lean T1 baseline for this Claim")?;
    let recipe = &evidence.recipe;
    let engine = bound_engine(root, &recipe.file_path)?;
    ensure!(
        engine == Engine::Lean4
            && evidence.engine == engine.as_str()
            && Path::new(&evidence.repo) == session.git.path()
            && evidence.claim_id == claim,
        "selected T3 baseline does not match the managed project and Claim"
    );
    let baseline = recipe.promoted_commit.as_str();
    // Reproduce the frozen body and prove the recorded promotion changed
    // only that module, with the claimed base as its sole physical parent.
    let (_, _, source) = validate_recipe(&session.git, engine, recipe, baseline)?;
    session.git.assert_exact_proof_commit(
        &recipe.base_commit,
        baseline,
        &recipe.file_path,
        source.as_bytes(),
    )?;
    let git_snapshot = session.git.snapshot(&recipe.file_path, Some(baseline))?;
    let files = git_snapshot.project()?;
    let index = files
        .iter()
        .position(|(file, _)| *file == recipe.file_path)
        .context("baseline target absent from Git")?;
    let dependencies = predecessors(&files, index);
    ensure!(
        lean_breaker_sources_sha256(&dependencies) == evidence.dependencies_sha256,
        "baseline dependency bytes do not match the exact T1 receipt"
    );
    if let Some(spec) = session.store.lean_spec(claim)? {
        reviewed_spec::check_snapshot(&session.git, &git_snapshot, &spec)?;
        reviewed_spec::check_prefix(&dependencies, &spec)?;
    }
    // Do not retain the verified snapshot's source buffers while planning.
    drop(dependencies);
    drop(files);
    drop(source);
    let observation = session.observe(&recipe.file_path, baseline, engine)?;
    Ok(ClaimDiscovery {
        baseline_claim: evidence.claim_id,
        baseline_receipt_sha256: evidence.receipt_sha256,
        baseline_scope: "claim_t1",
        observation,
    })
}

#[cfg(unix)]
pub(crate) fn discover_for_daemon(
    root: &Path,
    repo: &Path,
    anchor: &str,
    baseline: &str,
) -> Result<()> {
    super::parent::arm()?;
    let observed = observe(root, repo, anchor, baseline, true)?;
    // Omit Claim details, source bytes, and the unbounded advisory plan from
    // the subprocess protocol. The digest can be used only for human review.
    print_json(serde_json::json!({
        "status":observed.status,
        "base_commit":observed.base_commit,
        "observed_head":observed.observed_head,
        "ledger_event_seq":observed.ledger_event_seq,
        "module_order_changed":observed.module_order_changed,
        "changed_module_count":observed.changed_modules.len(),
        "plan_sha256":observed.plan.as_ref().map(|p|&p.plan_sha256),
        "advisory_only":true,
    }))
}

fn observe(
    root: &Path,
    repo: &Path,
    anchor: &str,
    baseline: &str,
    existing_lock: bool,
) -> Result<Observation> {
    let engine = bound_engine(root, anchor)?;
    DiscoverySession::open(root, repo, existing_lock)?.observe(anchor, baseline, engine)
}

struct DiscoverySession {
    store: StateStore,
    git: PromotionRepo,
    stamp: (String, i64),
    head: String,
}

impl DiscoverySession {
    fn open(root: &Path, repo: &Path, existing_lock: bool) -> Result<Self> {
        let store = StateStore::open_readonly(private_state_path(root)?)?;
        let stamp = store.managed_wiki_stamp()?.context(
            "bind the managed repository and recover pending Git transactions before T3 discovery",
        )?;
        let canonical = repo.canonicalize()?;
        ensure!(
            Path::new(&stamp.0) == canonical,
            "project is not bound to the supplied managed repository"
        );
        let git = if existing_lock {
            PromotionRepo::try_lock_existing(&canonical)?
        } else {
            PromotionRepo::try_lock(&canonical)?
        };
        ensure!(
            git.path() == canonical,
            "managed repository moved while obtaining its lock"
        );
        let head = git.main_head()?;
        Ok(Self {
            store,
            git,
            stamp,
            head,
        })
    }

    fn observe(&mut self, anchor: &str, baseline: &str, engine: Engine) -> Result<Observation> {
        let Self {
            store,
            git,
            stamp,
            head,
        } = self;
        git.require_commit(baseline)?;
        // Validate the requested baseline and anchor even if main has diverged.
        let before = git.snapshot(anchor, Some(baseline))?;
        let before_rows = before.project()?;
        let mut observed = Observation {
            status: Status::Unchanged,
            base_commit: baseline.into(),
            observed_head: head.clone(),
            ledger_event_seq: stamp.1,
            changed_modules: vec![],
            module_order_changed: false,
            plan: None,
            advisory_only: true,
        };
        if !git.is_ancestor(baseline, head)? {
            // Do not pick a merge base or claim an equivalent source tree after a forced rewrite.
            observed.status = Status::Diverged;
        } else if baseline != head {
            let after = git.snapshot(anchor, Some(head))?;
            let after_rows = after.project()?;
            let delta = ProjectDelta::between(&before_rows, &after_rows);
            if delta.changed() {
                observed.status = Status::StructuralChange;
                observed.changed_modules = delta.changed_modules.clone();
                observed.module_order_changed = delta.module_order_changed;
                let plan = delta.build_plan(store, git, baseline, engine, &after, &after_rows)?;
                ensure!(
                    plan.ledger_event_seq == stamp.1,
                    "ledger changed during T3 discovery; retry"
                );
                observed.plan = Some(plan);
            }
        }
        ensure!(
            git.main_head()? == *head && store.managed_wiki_stamp()?.as_ref() == Some(&*stamp),
            "managed main or ledger changed during T3 discovery; retry"
        );
        Ok(observed)
    }
}
