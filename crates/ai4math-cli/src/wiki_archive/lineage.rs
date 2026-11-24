//! Record a historical replacement edge only after the reviewed adaptation's
//! separate managed promotion. This does not create or publish a CORE entry.
use super::{prepare_wake, require_reviewed_main, sha};
use crate::{
    print_json, promotion::PromotionRepo, reviewed_spec, t3::validate_recipe,
    worker_dispatch::private_state_path,
};
use ai4math_core::Engine;
use ai4math_store::{SedimentLineage, SedimentLineageInput, StateStore};
use anyhow::{Context, Result, ensure};
use serde_json::json;
use std::path::Path;

pub(crate) fn retain(
    root: &Path,
    repo: &Path,
    approval: &str,
    head: &str,
) -> Result<SedimentLineage> {
    let git = PromotionRepo::try_lock(repo)?;
    let path = private_state_path(root)?;
    let state = StateStore::open_readonly(&path)?;
    let saved = state
        .sediment_adaptation(approval)?
        .context("no retained sediment adaptation approval")?
        .plan;
    let canonical_repo = git
        .path()
        .to_str()
        .context("managed repository path is not UTF-8")?;
    require_reviewed_main(&state, &git, head)?;
    ensure!(
        saved.repo == canonical_repo,
        "adaptation approval belongs to a different managed repository"
    );
    let seq = state.status()?.last_event_seq;
    let recipe = state
        .managed_proof_recipe_at(&saved.target_claim, head)?
        .context("no exact managed promotion for the adaptation target at this commit")?;
    ensure!(
        recipe.base_commit == saved.reviewed_commit
            && recipe.promoted_commit == head
            && recipe.file_path == saved.target_module
            && recipe.target_decl == saved.target_decl,
        "promoted proof does not derive from this adaptation's reviewed frozen target"
    );
    let archive = prepare_wake(root, &git, &state, &saved.archive_id, head)?;
    ensure!(
        archive.claim_id == saved.origin_claim
            && archive.verified_at_commit == saved.promoted_commit,
        "adaptation source differs from the authenticated historical archive"
    );
    let frozen = git.snapshot(&saved.target_module, Some(&saved.reviewed_commit))?;
    let files = frozen.project()?;
    let index = files
        .iter()
        .position(|(module, _)| *module == saved.target_module)
        .context("missing reviewed frozen module")?;
    ensure!(
        sha(files[index].1.as_bytes()) == saved.frozen_source_sha256,
        "frozen module differs from approved adaptation bytes"
    );
    let current_spec = state
        .lean_spec(&saved.target_claim)?
        .context("adaptation target no longer has a reviewed Lean spec")?;
    ensure!(
        current_spec.spec_file == saved.spec_file
            && current_spec.spec_decl == saved.spec_decl
            && current_spec.spec_prefix_sha256 == saved.spec_prefix_sha256,
        "adaptation target does not match the approved reviewed specification"
    );
    let deps = files[..index]
        .iter()
        .map(|(file, src)| (*file, src.as_str()))
        .collect::<Vec<_>>();
    reviewed_spec::check_snapshot(&git, &frozen, &current_spec)?;
    reviewed_spec::check_prefix(&deps, &current_spec)?;
    let (_, _, replayed) = validate_recipe(&git, Engine::Lean4, &recipe, head)?;
    git.assert_exact_proof_commit(
        &saved.reviewed_commit,
        head,
        &saved.target_module,
        replayed.as_bytes(),
    )?;
    ensure!(
        git.main_head()? == head && state.status()?.last_event_seq == seq,
        "main or ledger changed while validating sediment adaptation lineage"
    );
    let mut writable = StateStore::open(&path)?;
    ensure!(
        git.main_head()? == head,
        "main changed before retention of sediment adaptation lineage"
    );
    writable.retain_sediment_lineage(SedimentLineageInput {
        approval_sha256: approval,
        repo: canonical_repo,
        promoted_commit: head,
        expected_event_seq: seq,
    })
}

fn print_lineage(lineage: SedimentLineage) -> Result<()> {
    print_json(
        json!({"edge":{"edge_type":"superseded_by","src_id":lineage.archive_id,"dst_id":lineage.core_entry_id},
        "lineage":lineage,"core_projected":false,"current_proof_authority":false}),
    )
}

pub(crate) fn retain_command(root: &Path, repo: &Path, approval: &str, head: &str) -> Result<()> {
    print_lineage(retain(root, repo, approval, head)?)
}

pub(crate) fn result(root: &Path, approval: &str) -> Result<()> {
    let state = StateStore::open_readonly(private_state_path(root)?)?;
    let lineage = state
        .sediment_lineage(approval)?
        .context("no retained sediment adaptation lineage")?;
    print_lineage(lineage)
}
