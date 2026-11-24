//! Operator-owned reviewed Lean specification binding for managed promotion.
use super::{
    HOLE, bound_engine, print_json,
    promotion::{Promotion, PromotionRepo},
};
use ai4math_broker::lean_breaker_sources_sha256;
use ai4math_core::Engine;
use ai4math_store::{ReviewedLeanSpec, StateStore};
use anyhow::{Context, Result, ensure};
use std::path::Path;

fn prefix_sha256(dependencies: &[(&str, &str)], spec_file: &str) -> Result<String> {
    let end = dependencies
        .iter()
        .position(|(name, _)| *name == spec_file)
        .context("reviewed specification must be a distinct module before the candidate")?;
    ensure!(
        !dependencies[..=end]
            .iter()
            .any(|(_, src)| src.contains(HOLE)),
        "reviewed specification prefix contains an unfilled proof slot"
    );
    Ok(lean_breaker_sources_sha256(&dependencies[..=end]))
}

pub(crate) fn bind(
    root: &Path,
    repo: &Path,
    id: &str,
    file: &str,
    decl: &str,
    commit: &str,
) -> Result<()> {
    ensure!(
        bound_engine(root, file)? == Engine::Lean4,
        "reviewed specification binding requires a Lean project"
    );
    let locked = PromotionRepo::lock(repo)?;
    locked.require_commit(commit)?;
    let mut store = StateStore::open(root.join(".runtime/state.sqlite"))?;
    let claim = store.claim(id)?.context("unknown claim")?;
    let (module, _) = claim
        .formal_target
        .as_deref()
        .and_then(|t| t.split_once('#'))
        .context("Claim does not have a registered formal target")?;
    let tx = locked.snapshot(module, None)?;
    ensure!(
        tx.base() == commit,
        "reviewed specification snapshot is not current main"
    );
    let project = tx.project()?;
    let target = project
        .iter()
        .position(|(p, _)| *p == module)
        .context("candidate module absent from the reviewed commit")?;
    let dependencies: Vec<_> = project[..target]
        .iter()
        .map(|(p, s)| (*p, s.as_str()))
        .collect();
    let binding = ReviewedLeanSpec {
        repo: locked
            .path()
            .to_str()
            .context("repository path is not UTF-8")?
            .into(),
        claim_id: id.into(),
        approved_git_commit: commit.into(),
        spec_file: file.into(),
        spec_decl: decl.into(),
        spec_prefix_sha256: prefix_sha256(&dependencies, file)?,
    };
    ensure!(
        locked.main_head()? == commit,
        "main changed while binding reviewed specification"
    );
    store.bind_lean_spec(&binding)?;
    print_json(
        serde_json::json!({"claim_id":id,"approved_git_commit":commit,"spec_file":file,"spec_decl":decl,"spec_prefix_sha256":binding.spec_prefix_sha256}),
    )
}

pub(crate) fn check_snapshot(
    locked: &PromotionRepo,
    tx: &Promotion<'_>,
    binding: &ReviewedLeanSpec,
) -> Result<()> {
    ensure!(
        Path::new(&binding.repo) == locked.path() && tx.repo() == locked.path(),
        "reviewed Lean specification belongs to a different repository"
    );
    ensure!(
        locked.is_ancestor(&binding.approved_git_commit, tx.base())?,
        "candidate base does not descend from the reviewed Lean specification snapshot"
    );
    Ok(())
}

pub(crate) fn check_prefix(
    dependencies: &[(&str, &str)],
    binding: &ReviewedLeanSpec,
) -> Result<()> {
    ensure!(
        prefix_sha256(dependencies, &binding.spec_file)? == binding.spec_prefix_sha256,
        "reviewed Lean specification or its ordered dependency prefix changed"
    );
    Ok(())
}

pub(crate) fn read_comparison(root: &Path, commit: &str) -> Result<()> {
    ensure!(
        matches!(commit.len(), 40 | 64)
            && commit
                .bytes()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
        "expected a full lowercase Git commit hash"
    );
    let store = StateStore::open_readonly(root.join(".runtime/state.sqlite"))?;
    let row = store
        .lean_spec_comparison(commit)?
        .context("no retained Lean specification comparison for this commit")?;
    print_json(serde_json::json!({
        "promoted_commit":row.promoted_commit,"claim_id":row.claim_id,"base_commit":row.base_commit,
        "spec_file":row.spec_file,"spec_decl":row.spec_decl,"spec_prefix_sha256":row.spec_prefix_sha256,
        "candidate_receipt_sha256":row.candidate_receipt_sha256,"comparison_receipt_sha256":row.receipt_sha256
    }))
}

pub(crate) fn read_recheck(root: &Path, claim: &str, plan: &str) -> Result<()> {
    ensure!(
        plan.len() == 64
            && plan
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
        "expected a lowercase T3 plan SHA-256"
    );
    let store = StateStore::open_readonly(root.join(".runtime/state.sqlite"))?;
    let row = store
        .lean_spec_recheck(claim, plan)?
        .context("no retained Lean specification recheck for this Claim and T3 plan")?;
    print_json(row)
}

#[cfg(test)]
mod tests;
