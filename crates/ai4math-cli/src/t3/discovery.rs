//! One-shot advisory observation of the managed main against an explicit
//! operator baseline. No observation creates a plan approval or taints a claim.
use super::{Plan, ProjectDelta, bound_engine, print_json};
use crate::{promotion::PromotionRepo, worker_dispatch::private_state_path};
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

pub(crate) fn discover(root: &Path, repo: &Path, anchor: &str, baseline: &str) -> Result<()> {
    print_json(observe(root, repo, anchor, baseline, false)?)
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
    let mut store = StateStore::open_readonly(private_state_path(root)?)?;
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
    git.require_commit(baseline)?;
    let head = git.main_head()?;
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
    if !git.is_ancestor(baseline, &head)? {
        // Do not pick a merge base or claim an equivalent source tree after a forced rewrite.
        observed.status = Status::Diverged;
    } else if baseline != head {
        let after = git.snapshot(anchor, Some(&head))?;
        let after_rows = after.project()?;
        let delta = ProjectDelta::between(&before_rows, &after_rows);
        if delta.changed() {
            observed.status = Status::StructuralChange;
            observed.changed_modules = delta.changed_modules.clone();
            observed.module_order_changed = delta.module_order_changed;
            let plan = delta.build_plan(&mut store, &git, baseline, engine, &after, &after_rows)?;
            ensure!(
                plan.ledger_event_seq == stamp.1,
                "ledger changed during T3 discovery; retry"
            );
            observed.plan = Some(plan);
        }
    }
    ensure!(
        git.main_head()? == head && store.managed_wiki_stamp()?.as_ref() == Some(&stamp),
        "managed main or ledger changed during T3 discovery; retry"
    );
    Ok(observed)
}
