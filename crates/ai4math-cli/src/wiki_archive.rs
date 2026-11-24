//! Reviewed, source-only archival; never removes a module or promotes an artifact.
use crate::{bound_engine, print_json, promotion::PromotionRepo, t3::validate_recipe};
use ai4math_store::{
    ManagedProofRecipe, SedimentArtifact, SedimentInput, StateStore, Trust, publish_sediment,
    read_sediment, render_sediment,
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

type Prefix = (Vec<String>, BTreeMap<String, String>);

fn prefix(rows: &[(&str, String)], module: &str) -> Result<Prefix> {
    let end = rows
        .iter()
        .position(|(path, _)| *path == module)
        .context("verified module is missing")?;
    let order = rows[..end]
        .iter()
        .map(|(path, _)| path.to_string())
        .collect();
    let hashes = rows[..end]
        .iter()
        .map(|(path, source)| (path.to_string(), sha(source.as_bytes())))
        .collect();
    Ok((order, hashes))
}

fn historical(
    root: &Path,
    git: &PromotionRepo,
    recipe: &ManagedProofRecipe,
    head: &str,
) -> Result<(SedimentArtifact, String, Prefix)> {
    let engine = bound_engine(root, &recipe.file_path)?;
    let (_, _, source) = validate_recipe(git, engine, recipe, head)?;
    let snapshot = git.snapshot(&recipe.file_path, Some(&recipe.promoted_commit))?;
    let (dependency_order, dependency_hashes) = prefix(&snapshot.project()?, &recipe.file_path)?;
    let artifact = render_sediment(SedimentInput {
        claim_id: &recipe.claim_id,
        engine: engine.as_str(),
        verified_at_commit: &recipe.promoted_commit,
        source_module: &recipe.file_path,
        target_decl: &recipe.target_decl,
        source: &source,
        source_sha256: &recipe.source_sha256,
        dependency_hashes: &dependency_hashes,
        dependency_order: &dependency_order,
    })?;
    Ok((
        artifact,
        engine.as_str().into(),
        (dependency_order, dependency_hashes),
    ))
}

fn require_reviewed_main(
    state: &StateStore,
    git: &PromotionRepo,
    reviewed_main: &str,
) -> Result<()> {
    ensure!(
        state.managed_repo()?.as_deref().map(Path::new) == Some(git.path()),
        "project is not bound to the supplied managed repository"
    );
    ensure!(
        state.promotion_intent()?.is_none() && state.pivot_intent()?.is_none(),
        "recover the pending managed Git transaction before examining sediment"
    );
    git.require_commit(reviewed_main)?;
    ensure!(
        git.main_head()? == reviewed_main,
        "reviewed archive base is not current main"
    );
    Ok(())
}

fn prepare(
    root: &Path,
    git: &PromotionRepo,
    state: &StateStore,
    claim_id: &str,
    reviewed_main: &str,
) -> Result<(Preview, SedimentArtifact)> {
    require_reviewed_main(state, git, reviewed_main)?;
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
    let (artifact, engine_name, (dependency_order, dependency_hashes)) =
        historical(root, git, &recipe, reviewed_main)?;
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
            engine: engine_name,
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

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum Compatibility {
    ModuleAbsent,
    ProjectBytesMatch,
    TargetChanged,
    PrefixChanged,
    TargetAndPrefixChanged,
}

#[derive(Debug, Serialize)]
struct PrefixDiff {
    added: Vec<String>,
    removed: Vec<String>,
    modified: Vec<String>,
    order_matches: bool,
}

fn compare_prefix(old: &Prefix, current: &Prefix) -> PrefixDiff {
    PrefixDiff {
        added: current
            .1
            .keys()
            .filter(|key| !old.1.contains_key(*key))
            .cloned()
            .collect(),
        removed: old
            .1
            .keys()
            .filter(|key| !current.1.contains_key(*key))
            .cloned()
            .collect(),
        modified: old
            .1
            .iter()
            .filter(|(key, sha)| current.1.get(*key).is_some_and(|now| now != *sha))
            .map(|(key, _)| key.clone())
            .collect(),
        order_matches: old.0 == current.0,
    }
}

#[derive(Debug, Serialize)]
struct WakePreview {
    archive_id: String,
    current_main: String,
    claim_id: String,
    engine: String,
    verified_at_commit: String,
    source_module: String,
    target_decl: String,
    source_sha256: String,
    current_source_sha256: Option<String>,
    prefix: Option<PrefixDiff>,
    compatibility: Compatibility,
    requires_review_and_verification: bool,
    current_proof_authority: bool,
}

fn prepare_wake(
    root: &Path,
    git: &PromotionRepo,
    state: &StateStore,
    id: &str,
    reviewed_main: &str,
) -> Result<WakePreview> {
    require_reviewed_main(state, git, reviewed_main)?;
    let seq = state.status()?.last_event_seq;
    let archived = read_sediment(root, id)?;
    // A content digest can be computed by anyone. Prove the entry matches an
    // exact historical kernel-backed promotion in this project's ledger and Git.
    let recipe = state
        .managed_proof_recipe_at(&archived.claim_id, &archived.verified_at_commit)?
        .context("sediment has no exact managed promotion in this project")?;
    let (expected, _, old) = historical(root, git, &recipe, reviewed_main)?;
    ensure!(
        expected.id() == id,
        "sediment differs from the historical managed proof"
    );
    let (current_source_sha256, prefix, compatibility) =
        if git.contains_module_at(reviewed_main, &recipe.file_path)? {
            let snapshot = git.snapshot(&recipe.file_path, Some(reviewed_main))?;
            let rows = snapshot.project()?;
            let (_, source) = rows
                .iter()
                .find(|(module, _)| *module == recipe.file_path)
                .context("current proof module missing from reviewed manifest")?;
            let current_sha = sha(source.as_bytes());
            let diff = compare_prefix(&old, &self::prefix(&rows, &recipe.file_path)?);
            let prefix_matches = diff.order_matches
                && diff.added.is_empty()
                && diff.removed.is_empty()
                && diff.modified.is_empty();
            let compatibility = match (current_sha == recipe.source_sha256, prefix_matches) {
                (true, true) => Compatibility::ProjectBytesMatch,
                (false, true) => Compatibility::TargetChanged,
                (true, false) => Compatibility::PrefixChanged,
                (false, false) => Compatibility::TargetAndPrefixChanged,
            };
            (Some(current_sha), Some(diff), compatibility)
        } else {
            (None, None, Compatibility::ModuleAbsent)
        };
    ensure!(
        state.status()?.last_event_seq == seq && git.main_head()? == reviewed_main,
        "state or main changed during wake preview; rerun against reviewed main"
    );
    Ok(WakePreview {
        archive_id: id.into(),
        current_main: reviewed_main.into(),
        claim_id: archived.claim_id,
        engine: archived.engine,
        verified_at_commit: archived.verified_at_commit,
        source_module: archived.source_module,
        target_decl: archived.target_decl,
        source_sha256: archived.source_sha256,
        current_source_sha256,
        prefix,
        compatibility,
        requires_review_and_verification: true,
        current_proof_authority: false,
    })
}

pub(crate) fn wake_preview(root: &Path, repo: &Path, id: &str, reviewed_main: &str) -> Result<()> {
    let git = PromotionRepo::try_lock(repo)?;
    let state = StateStore::open_readonly(root.join(".runtime/state.sqlite"))?;
    print_json(prepare_wake(root, &git, &state, id, reviewed_main)?)
}

pub(crate) mod adapt;
pub(crate) mod lineage;
pub(crate) mod recheck;
#[cfg(test)]
mod tests;
