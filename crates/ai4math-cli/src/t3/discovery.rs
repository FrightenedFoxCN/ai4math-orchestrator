//! One-shot advisory observation of managed main against an explicit baseline
//! or one Claim's current managed T1 or sealed T3 receipt. Neither path confers approval.
use super::{Plan, ProjectDelta, bound_engine, predecessors, print_json, validate_recipe};
use crate::{promotion::PromotionRepo, reviewed_spec};
use ai4math_broker::lean_breaker_sources_sha256;
use ai4math_core::Engine;
use ai4math_project::open_managed_state;
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
    let evidence = session.store.t3_discovery_baseline(claim)?.context(
        "no current exact managed Lean T1 baseline or sealed T3 baseline for this Claim",
    )?;
    let recipe = &evidence.recipe;
    let engine = bound_engine(root, &recipe.file_path)?;
    ensure!(
        engine == Engine::Lean4
            && evidence.engine == engine.as_str()
            && Path::new(&evidence.repo) == session.git.path()
            && evidence.claim_id == claim,
        "selected T3 baseline does not match the managed project and Claim"
    );
    let baseline = evidence.verified_at_commit.as_str();
    // Reproduce the frozen body and prove the recorded promotion changed
    // only that module, with the claimed base as its sole physical parent.
    let (_, _, source) = validate_recipe(&session.git, engine, recipe, baseline)?;
    session.git.assert_exact_proof_commit(
        &recipe.base_commit,
        &recipe.promoted_commit,
        &recipe.file_path,
        source.as_bytes(),
    )?;
    let git_snapshot = session.git.snapshot(&recipe.file_path, Some(baseline))?;
    let files = git_snapshot.project()?;
    let index = files
        .iter()
        .position(|(file, _)| *file == recipe.file_path)
        .context("baseline target absent from Git")?;
    ensure!(
        files[index].1 == source,
        "T3 baseline target differs from the exact promoted proof bytes"
    );
    let dependencies = predecessors(&files, index);
    ensure!(
        lean_breaker_sources_sha256(&dependencies) == evidence.dependencies_sha256,
        "baseline dependency bytes do not match the exact verification receipt"
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
        baseline_scope: evidence.baseline_scope,
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
    print_daemon_summary(&observed, None)
}

#[cfg(unix)]
pub(crate) fn discover_claim_for_daemon(root: &Path, repo: &Path, claim: &str) -> Result<()> {
    super::parent::arm()?;
    let discovery = discover_claim_observation(root, repo, claim)?;
    print_daemon_summary(&discovery.observation, Some(discovery.baseline_scope))
}

#[cfg(unix)]
fn print_daemon_summary(observed: &Observation, scope: Option<&str>) -> Result<()> {
    // Omit Claim details, source bytes, and the unbounded advisory plan from
    // the subprocess protocol. The digest can be used only for human review.
    let mut summary = serde_json::json!({
        "status":observed.status,
        "base_commit":observed.base_commit,
        "observed_head":observed.observed_head,
        "ledger_event_seq":observed.ledger_event_seq,
        "module_order_changed":observed.module_order_changed,
        "changed_module_count":observed.changed_modules.len(),
        "plan_sha256":observed.plan.as_ref().map(|p|&p.plan_sha256),
        "advisory_only":true,
    });
    if let Some(scope) = scope {
        summary["baseline_scope"] = scope.into();
    }
    print_json(summary)
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
        let store = open_managed_state(root, true)?;
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
        if !git.is_ungrafted_ancestor(baseline, head)? {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{init, managed_tests::git};
    use std::{fs, process::Command};

    fn ordinary_is_ancestor(repo: &Path, old: &str, new: &str) -> Result<bool> {
        let output = Command::new("/usr/bin/git")
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .arg("--git-dir")
            .arg(repo)
            .args(["merge-base", "--is-ancestor", old, new])
            .output()?;
        ensure!(
            output.status.success() || output.status.code() == Some(1),
            "fixture Git ancestry failed"
        );
        Ok(output.status.success())
    }

    #[test]
    fn discovery_classifies_physical_history_under_grafts() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path();
        init(root, Engine::Lean4)?;
        let work = root.join("work");
        fs::create_dir(&work)?;
        git(&work, &["init", "--initial-branch=main"]);
        fs::write(work.join("A.lean"), "def unchanged : Nat := 1\n")?;
        git(&work, &["add", "A.lean"]);
        git(&work, &["commit", "-m", "baseline"]);
        let repo = root.join("repo.git");
        git(root, &["clone", "--bare", "work", "repo.git"]);
        let repo = repo.canonicalize()?;
        let base = git(&repo, &["rev-parse", "refs/heads/main"]);
        let tree = git(&repo, &["rev-parse", &format!("{base}^{{tree}}")]);
        let child = git(
            &repo,
            &["commit-tree", &tree, "-p", &base, "-m", "physical child"],
        );
        let unrelated = git(&repo, &["commit-tree", &tree, "-m", "unrelated root"]);
        let mut store = StateStore::open(root.join(".runtime/state.sqlite"))?;
        store.bind_task_repository(repo.to_str().unwrap(), &base)?;
        let seq = store.status()?.last_event_seq;
        drop(store);

        git(&repo, &["update-ref", "refs/heads/main", &child, &base]);
        fs::write(
            repo.join("info/grafts"),
            format!("{child}\n{unrelated} {base}\n"),
        )?;
        assert!(!ordinary_is_ancestor(&repo, &base, &child)?);
        let unchanged =
            DiscoverySession::open(root, &repo, false)?.observe("A.lean", &base, Engine::Lean4)?;
        assert!(matches!(unchanged.status, Status::Unchanged));
        assert!(unchanged.plan.is_none());

        git(
            &repo,
            &["update-ref", "refs/heads/main", &unrelated, &child],
        );
        fs::write(repo.join("info/grafts"), format!("{unrelated} {base}\n"))?;
        assert!(ordinary_is_ancestor(&repo, &base, &unrelated)?);
        let diverged =
            DiscoverySession::open(root, &repo, false)?.observe("A.lean", &base, Engine::Lean4)?;
        assert!(matches!(diverged.status, Status::Diverged));
        assert!(diverged.plan.is_none());
        assert_eq!(
            StateStore::open_readonly(root.join(".runtime/state.sqlite"))?
                .status()?
                .last_event_seq,
            seq
        );
        Ok(())
    }
}
