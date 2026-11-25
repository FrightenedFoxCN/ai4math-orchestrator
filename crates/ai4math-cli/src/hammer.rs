//! Operator-only native Lean reconstruction with an advisory result.
use super::{HOLE, bound_engine, print_json, promotion::PromotionRepo, trusted_toolchain};
use ai4math_broker::{
    Broker, HammerCandidate, HammerPremises, MAX_HAMMER_DEPENDENCIES, MAX_HAMMER_PREMISES_BYTES,
};
use ai4math_core::{Engine, FrozenTarget};
use ai4math_project::open_managed_state;
use ai4math_store::{StateStore, Trust};
use anyhow::{Context, Result, ensure};
use std::{fs, io::Read, path::Path};

struct Snapshot {
    target: FrozenTarget,
    dependencies: Vec<(String, String)>,
}

fn ensure_context(
    root: &Path,
    git: &PromotionRepo,
    store: &StateStore,
    claim_id: &str,
    commit: &str,
) -> Result<(String, String)> {
    ensure!(
        store.managed_repo()?.as_deref().map(Path::new) == Some(git.path()),
        "bind the project ledger to the reviewed repository before using Hammer"
    );
    ensure!(
        git.main_head()? == commit,
        "Hammer reviewed commit no longer matches main"
    );
    ensure!(
        store.promotion_intent()?.is_none() && store.pivot_intent()?.is_none(),
        "recover pending Git operations before Hammer reconstruction"
    );
    let claim = store.claim(claim_id)?.context("unknown Hammer Claim")?;
    ensure!(
        !claim.stale
            && !claim.effective_stale
            && !matches!(claim.local_trust, Trust::L2b | Trust::L5)
            && !matches!(claim.effective_trust, Trust::L2b | Trust::L5),
        "Hammer needs an open, runnable Claim"
    );
    let module = claim
        .module_path
        .context("Hammer Claim has no registered module")?;
    let qualified = claim
        .formal_target
        .context("Hammer Claim has no formal target")?;
    let (file, target) = qualified
        .split_once('#')
        .context("Hammer requires a module-qualified Claim target")?;
    ensure!(
        file == module && !target.is_empty() && !target.contains('#'),
        "invalid Hammer Claim target"
    );
    ensure!(
        bound_engine(root, &module)? == Engine::Lean4 && claim.engine == "lean4",
        "Hammer reconstruction currently requires the Lean primary engine"
    );
    ensure!(
        !store.module_breaker_gated("lean4", &module)?,
        "breaker gates the Hammer target module"
    );
    Ok((module, target.to_owned()))
}

fn capture(root: &Path, repo: &Path, claim: &str, commit: &str) -> Result<Snapshot> {
    let lock = PromotionRepo::lock(repo)?;
    let store = open_managed_state(root, true)?;
    let (module, target) = ensure_context(root, &lock, &store, claim, commit)?;
    let tx = lock.snapshot(&module, None)?;
    ensure!(
        tx.base() == commit,
        "main moved before capturing the Hammer snapshot"
    );
    let project = tx.project()?;
    let index = project
        .iter()
        .position(|(p, _)| *p == module)
        .context("missing Hammer module")?;
    ensure!(
        index <= MAX_HAMMER_DEPENDENCIES,
        "too many predecessor modules for bounded Hammer reconstruction"
    );
    let (prefix, suffix) = project[index]
        .1
        .split_once(HOLE)
        .context("expected a frozen proof slot")?;
    ensure!(!suffix.contains(HOLE), "multiple frozen proof slots");
    let target = FrozenTarget {
        engine: Engine::Lean4,
        target_id: target,
        file_path: module.clone(),
        prefix: prefix.into(),
        suffix: suffix.into(),
    };
    let dependencies = project
        .into_iter()
        .take(index)
        .map(|(path, source)| (path.into(), source))
        .collect();
    Ok(Snapshot {
        target,
        dependencies,
    })
}

fn reconstruct(
    root: &Path,
    repo: &Path,
    claim: &str,
    commit: &str,
    path: &Path,
    exec: &Path,
) -> Result<HammerCandidate> {
    let _native = crate::managed_native(root)?;
    let snap = capture(root, repo, claim, commit)?;
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path).context("open Hammer premise list")?;
    ensure!(
        file.metadata()?.is_file(),
        "Hammer premise list must be a regular file"
    );
    let mut bytes = Vec::new();
    file.take((MAX_HAMMER_PREMISES_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    let premises = HammerPremises::parse(&bytes)?;
    let tool = trusted_toolchain(Engine::Lean4, exec, None)?;
    let deps: Vec<_> = snap
        .dependencies
        .iter()
        .map(|(p, s)| (p.as_str(), s.as_str()))
        .collect();
    let result =
        Broker::new().reconstruct_lean_hammer(&tool, &snap.target, &premises, commit, &deps)?;
    let lock = PromotionRepo::lock(repo)?;
    let store = open_managed_state(root, true)?;
    let (module, decl) = ensure_context(root, &lock, &store, claim, commit)?;
    ensure!(
        module == snap.target.file_path && decl == snap.target.target_id,
        "Hammer Claim target changed during reconstruction"
    );
    ensure!(
        lock.main_head()? == commit,
        "main moved during Hammer reconstruction; discard candidate"
    );
    Ok(result)
}

pub(crate) fn run(
    root: &Path,
    repo: &Path,
    claim: &str,
    commit: &str,
    path: &Path,
    exec: &Path,
) -> Result<()> {
    let suggestion = reconstruct(root, repo, claim, commit, path, exec)?;
    print_json(
        serde_json::json!({"claim_id":claim,"evidence_kind":"advisory_lean_hammer_candidate","promoted":false,"candidate":suggestion}),
    )
}

#[cfg(test)]
mod tests;
