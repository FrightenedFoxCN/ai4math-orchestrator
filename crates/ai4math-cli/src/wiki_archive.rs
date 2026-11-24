//! Reviewed, source-only archival; never removes a module or promotes an artifact.
use crate::{bound_engine, print_json, promotion::PromotionRepo, t3::validate_recipe};
use ai4math_store::{
    SedimentArtifact, SedimentInput, StateStore, Trust, publish_sediment, render_sediment,
};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

#[derive(Debug, Serialize)]
struct Preview {
    id: String,
    relative_path: PathBuf,
    claim_id: String,
    engine: String,
    current_main: String,
    verified_at_commit: String,
    source_module: String,
    target_decl: String,
    source_sha256: String,
    dependency_order: Vec<String>,
    dependency_hashes: BTreeMap<String, String>,
    approval_sha256: String,
}

fn sha(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn prepare(
    root: &Path,
    git: &PromotionRepo,
    state: &StateStore,
    claim_id: &str,
    reviewed_main: &str,
) -> Result<(Preview, SedimentArtifact)> {
    ensure!(
        state.managed_repo()?.as_deref().map(Path::new) == Some(git.path()),
        "project is not bound to the supplied managed repository"
    );
    ensure!(
        state.promotion_intent()?.is_none() && state.pivot_intent()?.is_none(),
        "recover the pending managed Git transaction before archiving"
    );
    git.require_commit(reviewed_main)?;
    ensure!(
        git.main_head()? == reviewed_main,
        "reviewed archive base is not current main"
    );
    let seq = state.status()?.last_event_seq;
    let claim = state.claim(claim_id)?.context("unknown archive Claim")?;
    ensure!(
        !claim.stale
            && !claim.effective_stale
            && claim.local_trust == Trust::L5
            && claim.effective_trust == Trust::L5,
        "only a nonstale effective L5 Claim may enter managed sediment"
    );
    let recipe = state
        .managed_proof_recipe(claim_id)?
        .context("claim lacks a historical managed promotion recipe")?;
    let engine = bound_engine(root, &recipe.file_path)?;
    let target = format!("{}#{}", recipe.file_path, recipe.target_decl);
    ensure!(
        claim.engine == engine.as_str()
            && claim.module_path.as_deref() == Some(recipe.file_path.as_str())
            && claim.formal_target.as_deref() == Some(target.as_str()),
        "registered Claim no longer matches its historical managed proof"
    );
    let (_, _, source) = validate_recipe(git, engine, &recipe, reviewed_main)?;
    let snapshot = git.snapshot(&recipe.file_path, Some(&recipe.promoted_commit))?;
    let rows = snapshot.project()?;
    let end = rows
        .iter()
        .position(|(module, _)| *module == recipe.file_path)
        .context("historical verified module is missing")?;
    let dependency_order = rows[..end]
        .iter()
        .map(|(p, _)| p.to_string())
        .collect::<Vec<_>>();
    let dependency_hashes = rows[..end]
        .iter()
        .map(|(p, source)| (p.to_string(), sha(source.as_bytes())))
        .collect::<BTreeMap<_, _>>();
    let artifact = render_sediment(SedimentInput {
        claim_id,
        engine: engine.as_str(),
        verified_at_commit: &recipe.promoted_commit,
        source_module: &recipe.file_path,
        target_decl: &recipe.target_decl,
        source: &source,
        source_sha256: &recipe.source_sha256,
        dependency_hashes: &dependency_hashes,
        dependency_order: &dependency_order,
    })?;
    let project = root.canonicalize()?;
    let approval_sha256 = sha(&serde_json::to_vec(&(
        "ai4math-wiki-sediment-v1",
        project,
        git.path(),
        reviewed_main,
        artifact.id(),
        sha(artifact.markdown().as_bytes()),
    ))?);
    ensure!(
        state.status()?.last_event_seq == seq && git.main_head()? == reviewed_main,
        "state or main changed while validating archive provenance; rerun preview"
    );
    Ok((
        Preview {
            id: artifact.id().into(),
            relative_path: artifact.relative_path(),
            claim_id: claim_id.into(),
            engine: engine.as_str().into(),
            current_main: reviewed_main.into(),
            verified_at_commit: recipe.promoted_commit,
            source_module: recipe.file_path,
            target_decl: recipe.target_decl,
            source_sha256: recipe.source_sha256,
            dependency_order,
            dependency_hashes,
            approval_sha256,
        },
        artifact,
    ))
}

pub(crate) fn preview(root: &Path, repo: &Path, claim_id: &str, reviewed_main: &str) -> Result<()> {
    let git = PromotionRepo::try_lock(repo)?;
    let state = StateStore::open_readonly(root.join(".runtime/state.sqlite"))?;
    print_json(prepare(root, &git, &state, claim_id, reviewed_main)?.0)
}

pub(crate) fn archive(
    root: &Path,
    repo: &Path,
    claim_id: &str,
    reviewed_main: &str,
    approval: &str,
) -> Result<()> {
    let git = PromotionRepo::try_lock(repo)?;
    let state = StateStore::open_readonly(root.join(".runtime/state.sqlite"))?;
    let (expected, artifact) = prepare(root, &git, &state, claim_id, reviewed_main)?;
    ensure!(
        approval == expected.approval_sha256,
        "archive approval differs from current preview"
    );
    ensure!(
        git.main_head()? == reviewed_main,
        "main changed before archive publication"
    );
    let created = publish_sediment(root, &artifact)?;
    print_json(
        serde_json::json!({"archive":expected,"created":created,"requires_wiki_rebuild":true,
        "git_main_changed":false,"current_proof_authority":false}),
    )
}

#[cfg(test)]
mod tests;
