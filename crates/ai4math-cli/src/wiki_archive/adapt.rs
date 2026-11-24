//! Review a fresh Lean frozen target against the same proposition as a historical
//! sediment entry. Approval is an operator handoff, not task enrollment or evidence.
use super::{Compatibility, prepare_wake, sha};
use crate::{HOLE, bound_engine, print_json, promotion::PromotionRepo, reviewed_spec};
use ai4math_core::{Engine, FrozenTarget, proof::lean_frozen_header};
use ai4math_store::{SedimentAdaptationApproval, SedimentAdaptationPlan, StateStore, Trust};
use anyhow::{Context, Result, ensure};
use std::path::Path;

fn prepare(
    root: &Path,
    git: &PromotionRepo,
    state: &StateStore,
    id: &str,
    head: &str,
    target_claim: &str,
) -> Result<SedimentAdaptationPlan> {
    let seq = state.status()?.last_event_seq;
    let origin = prepare_wake(root, git, state, id, head)?;
    ensure!(
        origin.engine == Engine::Lean4.as_str(),
        "sediment adaptation currently requires Lean"
    );
    ensure!(
        !matches!(origin.compatibility, Compatibility::ProjectBytesMatch),
        "the archived target and prefix are unchanged; use cold recheck, not adaptation"
    );
    ensure!(
        origin.claim_id != target_claim,
        "adaptation requires a distinct reviewed destination Claim"
    );
    let source_claim = state
        .claim(&origin.claim_id)?
        .context("unknown archived Claim")?;
    ensure!(
        source_claim.local_trust != Trust::L2b && source_claim.effective_trust != Trust::L2b,
        "a refuted origin cannot seed an adaptation"
    );
    let old_recipe = state
        .managed_proof_recipe_at(&origin.claim_id, &origin.verified_at_commit)?
        .context("no exact historical promotion for adaptation origin")?;
    let historical = state
        .lean_spec_comparison(&origin.verified_at_commit)?
        .context("historical archived proof lacks a retained Lean comparison")?;
    let old_spec = state
        .lean_spec(&origin.claim_id)?
        .context("historical origin has no reviewed Lean spec")?;
    ensure!(
        historical.spec_file == old_spec.spec_file
            && historical.spec_decl == old_spec.spec_decl
            && historical.spec_prefix_sha256 == old_spec.spec_prefix_sha256
            && state.managed_lean_t1_matches_comparison(&old_recipe, &historical)?,
        "archived proof is not bound to its original reviewed specification and T1 receipt"
    );
    let destination = state
        .claim(target_claim)?
        .context("unknown destination Claim")?;
    ensure!(
        !destination.stale
            && !destination.effective_stale
            && destination.local_trust != Trust::L2b
            && !matches!(destination.effective_trust, Trust::L2b | Trust::L3),
        "destination Claim is stale or invalid; reconcile it before adaptation"
    );
    ensure!(
        state.managed_proof_recipe(target_claim)?.is_none(),
        "destination Claim already has a managed proof"
    );
    let module = destination
        .module_path
        .as_deref()
        .context("destination Claim has no frozen module")?;
    let (registered_module, decl) = destination
        .formal_target
        .as_deref()
        .and_then(|s| s.split_once('#'))
        .context("destination Claim requires an exact module-qualified Lean target")?;
    ensure!(
        destination.engine == Engine::Lean4.as_str()
            && bound_engine(root, module)? == Engine::Lean4
            && registered_module == module
            && !decl.is_empty()
            && !decl.contains('#'),
        "destination Claim engine or target identity does not match its Lean module"
    );
    ensure!(
        !(module.eq_ignore_ascii_case(&old_recipe.file_path) && decl == old_recipe.target_decl),
        "adaptation cannot overwrite or alias the original Lean target"
    );
    let new_spec = state
        .lean_spec(target_claim)?
        .context("destination Claim lacks a reviewed Lean specification")?;
    ensure!(
        new_spec.spec_file == old_spec.spec_file
            && new_spec.spec_decl == old_spec.spec_decl
            && new_spec.spec_prefix_sha256 == old_spec.spec_prefix_sha256,
        "destination must bind the original exact reviewed Lean specification and ordered prefix"
    );
    let snapshot = git.snapshot(module, Some(head))?;
    let files = snapshot.project()?;
    let index = files
        .iter()
        .position(|(name, _)| *name == module)
        .context("destination module is absent from approved manifest")?;
    let source = &files[index].1;
    let (prefix, suffix) = source
        .split_once(HOLE)
        .context("destination requires a strict frozen proof slot")?;
    ensure!(
        !suffix.contains(HOLE),
        "destination has multiple frozen proof slots"
    );
    lean_frozen_header(&FrozenTarget {
        engine: Engine::Lean4,
        target_id: decl.into(),
        file_path: module.into(),
        prefix: prefix.into(),
        suffix: suffix.into(),
    })?;
    let deps = files[..index]
        .iter()
        .map(|(p, s)| (*p, s.as_str()))
        .collect::<Vec<_>>();
    reviewed_spec::check_snapshot(git, &snapshot, &old_spec)?;
    reviewed_spec::check_snapshot(git, &snapshot, &new_spec)?;
    reviewed_spec::check_prefix(&deps, &new_spec)?;
    ensure!(
        git.main_head()? == head
            && state.status()?.last_event_seq == seq
            && state.promotion_intent()?.is_none()
            && state.pivot_intent()?.is_none(),
        "main or ledger changed during adaptation preview; review again"
    );
    Ok(SedimentAdaptationPlan {
        archive_id: id.into(),
        origin_claim: origin.claim_id,
        promoted_commit: origin.verified_at_commit,
        target_claim: target_claim.into(),
        reviewed_commit: head.into(),
        repo: git
            .path()
            .to_str()
            .context("managed repository path is not UTF-8")?
            .into(),
        target_module: module.into(),
        target_decl: decl.into(),
        frozen_source_sha256: sha(source.as_bytes()),
        spec_file: new_spec.spec_file,
        spec_decl: new_spec.spec_decl,
        spec_prefix_sha256: new_spec.spec_prefix_sha256,
        expected_event_seq: seq,
    })
}

pub(crate) fn preview(
    root: &Path,
    repo: &Path,
    id: &str,
    head: &str,
    target_claim: &str,
) -> Result<()> {
    let git = PromotionRepo::try_lock(repo)?;
    let state = StateStore::open_readonly(root.join(".runtime/state.sqlite"))?;
    let plan = prepare(root, &git, &state, id, head, target_claim)?;
    print_json(
        serde_json::json!({"approval_sha256":plan.approval_sha256()?,"plan":plan,
        "task_enqueued":false,"current_proof_authority":false}),
    )
}

fn apply(
    root: &Path,
    repo: &Path,
    id: &str,
    head: &str,
    target_claim: &str,
    approval: &str,
) -> Result<SedimentAdaptationApproval> {
    let git = PromotionRepo::try_lock(repo)?;
    let path = crate::worker_dispatch::private_state_path(root)?;
    let readonly = StateStore::open_readonly(&path)?;
    let plan = prepare(root, &git, &readonly, id, head, target_claim)?;
    ensure!(
        plan.approval_sha256()? == approval,
        "adaptation approval differs from current preview"
    );
    let mut state = StateStore::open(&path)?;
    ensure!(
        git.main_head()? == head,
        "main changed before recording adaptation approval"
    );
    state.approve_sediment_adaptation(&plan, approval)
}

pub(crate) fn approve(
    root: &Path,
    repo: &Path,
    id: &str,
    head: &str,
    target_claim: &str,
    approval: &str,
) -> Result<()> {
    let approval = apply(root, repo, id, head, target_claim, approval)?;
    print_json(
        serde_json::json!({"approval":approval,"task_enqueued":false,"current_proof_authority":false}),
    )
}

pub(crate) fn result(root: &Path, id: &str) -> Result<()> {
    let state = StateStore::open_readonly(root.join(".runtime/state.sqlite"))?;
    let approval = state
        .sediment_adaptation(id)?
        .context("no retained sediment adaptation approval")?;
    print_json(
        serde_json::json!({"approval":approval,"task_enqueued":false,"current_proof_authority":false}),
    )
}

#[cfg(test)]
mod tests;
