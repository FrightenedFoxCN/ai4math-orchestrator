//! Operator-only witness verification bound to the managed repository's current main.
use super::{bound_engine, print_json, promotion::PromotionRepo, read_proof, trusted_toolchain};
use ai4math_broker::Broker;
use ai4math_core::{Engine, ProofBodySubmission};
use ai4math_project::open_managed_state;
use ai4math_store::{RefutationRecord, WitnessInput, WitnessSpecBinding};
use anyhow::{Context, Result, ensure};
use std::path::Path;

pub(crate) fn bind(
    root: &Path,
    repo: &Path,
    id: &str,
    module: &str,
    proposition: &str,
    commit: &str,
) -> Result<()> {
    ensure!(
        bound_engine(root, module)? == Engine::Lean4,
        "managed witness verification currently requires Lean 4"
    );
    let locked = PromotionRepo::lock(repo)?;
    locked.require_commit(commit)?;
    let snapshot = locked.snapshot(module, None)?;
    ensure!(
        snapshot.base() == commit && locked.main_head()? == commit,
        "reviewed witness snapshot is not current main"
    );
    let binding = WitnessSpecBinding {
        repo: locked
            .path()
            .to_str()
            .context("repository path is not UTF-8")?
            .into(),
        commit_id: commit.into(),
        file_path: module.into(),
        proposition_name: proposition.into(),
    };
    let mut store = open_managed_state(root, false)?;
    store.bind_witness_spec(id, &binding)?;
    print_json(binding)
}

pub(crate) fn pending(root: &Path) -> Result<()> {
    let store = open_managed_state(root, true)?;
    print_json(serde_json::json!({"pending":store.pending_refutation()?}))
}

pub(crate) fn reconcile(root: &Path, claim_id: Option<&str>) -> Result<()> {
    let mut store = open_managed_state(root, false)?;
    match claim_id {
        Some(claim_id) => print_json(store.reconcile_refutation(claim_id)?),
        None => print_json(store.reconcile_refutations()?),
    }
}

pub(crate) fn run(
    root: &Path,
    repo: &Path,
    claim_id: &str,
    module: &str,
    proposition: &str,
    exec: &Path,
    body: &Path,
) -> Result<()> {
    let record = verify_and_record(root, repo, claim_id, module, proposition, exec, body)?;
    print_json(record)
}

fn verify_and_record(
    root: &Path,
    repo: &Path,
    claim_id: &str,
    module: &str,
    proposition: &str,
    exec: &Path,
    body: &Path,
) -> Result<RefutationRecord> {
    let _native = crate::managed_native(root)?;
    let engine = bound_engine(root, module)?;
    ensure!(
        engine == Engine::Lean4,
        "managed witness verification currently requires Lean 4"
    );
    let locked = PromotionRepo::lock(repo)?;
    let snapshot = locked.snapshot(module, None)?;
    let mut store = open_managed_state(root, false)?;
    if let Some(registered) = store.managed_repo()? {
        ensure!(
            Path::new(&registered) == locked.path(),
            "project belongs to another repository"
        );
    }
    ensure!(
        store.promotion_intent()?.is_none(),
        "recover the pending promotion before refuting a claim"
    );
    let claim = store.claim(claim_id)?.context("unknown claim")?;
    let expected = format!("{module}#{proposition}");
    ensure!(
        claim.formal_target.as_deref() == Some(expected.as_str()) && claim.engine == "lean4",
        "witness must name the claim's registered proposition"
    );
    let binding = store
        .witness_spec_binding(claim_id)?
        .context("approve the exact Git snapshot with bind-witness-spec first")?;
    ensure!(
        Path::new(&binding.repo) == locked.path()
            && binding.commit_id == snapshot.base()
            && binding.file_path == module
            && binding.proposition_name == proposition,
        "current Git snapshot does not match the approved witness spec"
    );
    let sources = snapshot.project()?;
    let index = sources
        .iter()
        .position(|(name, _)| *name == module)
        .context("registered module is missing")?;
    let dependencies = sources[..index]
        .iter()
        .map(|(name, source)| (*name, source.as_str()))
        .collect::<Vec<_>>();
    let committed_source = &sources[index].1;
    let proof_body = read_proof(body)?;
    let submission = ProofBodySubmission {
        target_id: "witness".into(),
        proof_body,
    };
    let tool = trusted_toolchain(engine, exec, None)?;
    let witness = Broker::new().verify_lean_witness_with_dependencies(
        &tool,
        module,
        proposition,
        committed_source,
        &submission,
        snapshot.base(),
        &dependencies,
    )?;
    ensure!(
        locked.main_head()? == snapshot.base(),
        "main changed while verifying witness; discard result"
    );
    store.record_refutation(
        WitnessInput {
            project_root: root,
            repository: locked.path(),
            claim_id,
            committed_source,
            proof_body: &submission.proof_body,
        },
        &witness,
    )
}

#[cfg(test)]
mod tests;
