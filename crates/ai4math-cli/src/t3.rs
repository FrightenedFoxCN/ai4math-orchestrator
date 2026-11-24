//! Operator T3 coordination. Resolves reviewed Git objects, never treats a ledger row as proof.
#[cfg(test)]
mod tests;
use super::{
    HOLE, bound_engine, print_json,
    promotion::{Promotion, PromotionRepo},
    reviewed_spec, trusted_toolchain,
};
use ai4math_broker::{Broker, RecheckAssessment};
use ai4math_core::{Engine, FrozenTarget, ProofBodySubmission, render_proof_body};
use ai4math_store::{
    ClaimState, ManagedProofRecipe, RecheckOutcome, ReviewedLeanSpec, StateStore, T3PlanApproval,
    Trust, VerifiedReceipt,
};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

#[derive(Debug, Serialize)]
pub(crate) struct Plan {
    pub base_commit: String,
    pub reviewed_commit: String,
    pub ledger_event_seq: i64,
    pub changed_modules: Vec<String>,
    pub affected_modules: Vec<String>,
    pub claims: Vec<ClaimPlan>,
    pub plan_sha256: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct ClaimPlan {
    pub claim_id: String,
    pub prior_trust: Trust,
    pub old_proof: ProofDisposition,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProofDisposition {
    NotL5,
    ReviewedSpecChangedOrMissing,
    MissingManagedRecipe,
    TargetChangedOrMissing,
    InvalidProvenance,
    Ready {
        base_commit: String,
        promoted_commit: String,
        source_sha256: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        reviewed_spec_prefix_sha256: Option<String>,
    },
}

pub(crate) fn dry_run(
    root: &Path,
    repo: &Path,
    anchor: &str,
    old: &str,
    reviewed: &str,
) -> Result<()> {
    let engine = bound_engine(root, anchor)?;
    let locked = PromotionRepo::lock(repo)?;
    let mut state = StateStore::open_readonly(root.join(".runtime/state.sqlite"))?;
    ensure!(
        state.managed_repo()?.as_deref().map(Path::new) == Some(locked.path()),
        "project is not bound to the supplied managed repository"
    );
    print_json(plan(&mut state, &locked, anchor, old, reviewed, engine)?)
}

pub(crate) fn apply(
    root: &Path,
    repo: &Path,
    anchor: &str,
    old: &str,
    reviewed: &str,
    hash: &str,
) -> Result<()> {
    let engine = bound_engine(root, anchor)?;
    let locked = PromotionRepo::lock(repo)?;
    let mut state = StateStore::open(root.join(".runtime/state.sqlite"))?;
    ensure!(
        state.managed_repo()?.as_deref().map(Path::new) == Some(locked.path()),
        "project is not bound to the supplied managed repository"
    );
    ensure!(
        locked.main_head()? == reviewed,
        "reviewed commit is not current main; refusing to taint the wrong snapshot"
    );
    if let Some(prior) = state.applied_t3_plan(hash)? {
        ensure!(
            prior.base_commit == old
                && prior.reviewed_commit == reviewed
                && prior.anchor_module == anchor,
            "conflicting T3 replay"
        );
        return print_json(serde_json::json!({"already_applied":true,"approval":prior}));
    }
    let planned = plan(&mut state, &locked, anchor, old, reviewed, engine)?;
    ensure!(
        planned.plan_sha256 == hash,
        "reviewed T3 plan changed; rerun t3-preview"
    );
    ensure!(
        locked.main_head()? == reviewed,
        "main changed during T3 planning"
    );
    let approval = T3PlanApproval {
        plan_sha256: hash.into(),
        base_commit: old.into(),
        reviewed_commit: reviewed.into(),
        anchor_module: anchor.into(),
        preview_event_seq: planned.ledger_event_seq,
    };
    let modules = planned
        .affected_modules
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    let impact = state.apply_t3_plan(&approval, &modules)?;
    print_json(serde_json::json!({"approval":approval,"impact":impact}))
}

/// An operator-only snapshot of the approved replay scope. No replay, leases or
/// Git changes. A plan for an older main is not a runnable worklist.
pub(crate) fn worklist(
    root: &Path,
    repo: &Path,
    hash: &str,
    limit: usize,
    after: Option<&str>,
) -> Result<()> {
    let locked = PromotionRepo::try_lock(repo)?;
    let mut state = StateStore::open_readonly(root.join(".runtime/state.sqlite"))?;
    let approval = current_approval(&state, &locked, hash)?;
    let page = state.t3_worklist(hash, limit, after)?;
    ensure!(
        locked.main_head()? == approval.reviewed_commit,
        "approved T3 plan no longer matches main"
    );
    print_json(page)
}

pub(crate) fn recheck(
    root: &Path,
    repo: &Path,
    hash: &str,
    exec: &Path,
    data: Option<&str>,
) -> Result<()> {
    recheck_selected(root, repo, hash, None, exec, data)
}

pub(crate) fn recheck_one(
    root: &Path,
    repo: &Path,
    hash: &str,
    claim: &str,
    exec: &Path,
    data: Option<&str>,
) -> Result<()> {
    recheck_selected(root, repo, hash, Some(claim), exec, data)
}

fn current_approval(
    state: &StateStore,
    locked: &PromotionRepo,
    hash: &str,
) -> Result<T3PlanApproval> {
    ensure!(
        state.managed_repo()?.as_deref().map(Path::new) == Some(locked.path()),
        "project is not bound to the supplied managed repository"
    );
    ensure!(
        state.promotion_intent()?.is_none() && state.pivot_intent()?.is_none(),
        "recover pending managed Git transaction before T3 replay"
    );
    let approval = state
        .applied_t3_plan(hash)?
        .context("T3 plan has not been applied")?;
    let at = &approval.reviewed_commit;
    ensure!(
        approval.base_commit != *at && locked.is_ancestor(&approval.base_commit, at)?,
        "applied T3 plan no longer describes a descendant review commit"
    );
    ensure!(
        locked.main_head()? == *at,
        "approved T3 plan no longer matches main"
    );
    Ok(approval)
}

fn recheck_selected(
    root: &Path,
    repo: &Path,
    hash: &str,
    only: Option<&str>,
    exec: &Path,
    data: Option<&str>,
) -> Result<()> {
    let _native = crate::managed_native(root)?;
    let locked = PromotionRepo::lock(repo)?;
    let path = root.join(".runtime/state.sqlite");
    let mut state = StateStore::open(&path)?;
    let approval = current_approval(&state, &locked, hash)?;
    let at = &approval.reviewed_commit;
    let engine = bound_engine(root, &approval.anchor_module)?;
    let tool = trusted_toolchain(engine, exec, data)?;
    let repo_snapshot = locked.snapshot(&approval.anchor_module, Some(at))?;
    let files = repo_snapshot.project()?;
    let provenance = StateStore::open_readonly(&path)?;
    let broker = Broker::new();
    let check = |claim: &ClaimState| {
        let defer = |reason: &str| {
            Ok(RecheckOutcome::Deferred {
                reason: reason.into(),
            })
        };
        let spec = provenance.lean_spec(&claim.id)?;
        let Some(recipe) = provenance.managed_proof_recipe(&claim.id)? else {
            return defer("missing managed proof provenance");
        };
        if spec.is_some()
            && provenance
                .lean_spec_comparison(&recipe.promoted_commit)?
                .as_ref()
                .is_none_or(|c| c.claim_id != claim.id)
        {
            return defer("baseline is missing retained Lean T1 comparison evidence");
        }
        let Ok((target, old_proof, original)) =
            validate_recipe(&locked, engine, &recipe, &approval.base_commit)
        else {
            return defer("old proof no longer matches its managed Git provenance");
        };
        let Some(index) = files.iter().position(|(name, _)| *name == recipe.file_path) else {
            return defer("target module absent at approved commit");
        };
        if files[index].1 != original {
            return defer("target source changed at approved commit; requires review");
        }
        let deps = predecessors(&files, index);
        if let Some(spec) = spec.as_ref() {
            if engine != Engine::Lean4
                || reviewed_spec::check_snapshot(&locked, &repo_snapshot, spec).is_err()
            {
                return defer(
                    "reviewed Lean specification has incompatible Git or engine provenance",
                );
            }
            if reviewed_spec::check_prefix(&deps, spec).is_err() {
                return defer(
                    "reviewed Lean specification or its ordered prefix changed; restore the approved bytes or register a new reviewed target",
                );
            }
        }
        ensure!(
            locked.main_head()? == *at,
            "main changed before T3 verification"
        );
        let result = broker.assess_recheck_with_dependencies(&tool, &target, &old_proof, at, &deps);
        ensure!(
            locked.main_head()? == *at,
            "main changed during T3 verification; discard result"
        );
        match result {
            RecheckAssessment::Verified(candidate) => {
                ensure!(
                    candidate.source() == files[index].1,
                    "T3 verified source differs from approved Git blob"
                );
                let receipt = if let Some(spec) = spec.as_ref() {
                    let compared = broker.compare_lean_spec(
                        &tool,
                        &candidate,
                        &spec.spec_file,
                        &spec.spec_decl,
                        &deps,
                    );
                    ensure!(
                        locked.main_head()? == *at,
                        "main changed during T3 comparison; discard result"
                    );
                    // A different theorem type is not a rejected proof of the approved
                    // statement, and ambiguity is not evidence. Keep the taint.
                    let Ok(compared) = compared else {
                        return defer(
                            "native comparator could not confirm the previous proof still matches the reviewed Lean spec",
                        );
                    };
                    VerifiedReceipt::from_compared_recheck(&candidate, &compared)?
                } else {
                    VerifiedReceipt::from_broker(&candidate)?
                };
                Ok(RecheckOutcome::Verified(Box::new(receipt)))
            }
            RecheckAssessment::Rejected { .. } if spec.is_some() => defer(
                "previous proof did not recheck; no positive comparison evidence for the reviewed Lean spec",
            ),
            RecheckAssessment::Rejected { diagnostics } => {
                Ok(RecheckOutcome::Failed { diagnostics })
            }
            RecheckAssessment::Unavailable { .. } => defer(
                "verification infrastructure, dependency or policy prevented a conclusive recheck",
            ),
        }
    };
    let report = match only {
        Some(id) => state.recheck_one_stale_for_plan(hash, id, "broker:t3", check)?,
        None => state.recheck_stale_for_plan(hash, "broker:t3", check)?,
    };
    print_json(report)
}

pub(crate) fn plan(
    state: &mut StateStore,
    locked: &PromotionRepo,
    anchor: &str,
    old: &str,
    reviewed: &str,
    engine: Engine,
) -> Result<Plan> {
    ensure!(
        old != reviewed && locked.is_ancestor(old, reviewed)?,
        "T3 requires an explicit descendant review commit"
    );
    let before = locked.snapshot(anchor, Some(old))?;
    let after_snapshot = locked.snapshot(anchor, Some(reviewed))?;
    let before_rows = before.project()?;
    let after_rows = after_snapshot.project()?;
    let before = before_rows
        .iter()
        .map(|(p, s)| (*p, s.as_str()))
        .collect::<BTreeMap<_, _>>();
    let after = after_rows
        .iter()
        .map(|(p, s)| (*p, s.as_str()))
        .collect::<BTreeMap<_, _>>();
    let all_modules = before
        .keys()
        .chain(after.keys())
        .copied()
        .collect::<BTreeSet<_>>();
    let changed_modules = all_modules
        .iter()
        .copied()
        .filter(|p| before.get(p) != after.get(p))
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let order_changed = before_rows
        .iter()
        .map(|(p, _)| p)
        .ne(after_rows.iter().map(|(p, _)| p));
    ensure!(
        !changed_modules.is_empty() || order_changed,
        "the review contains no source or module-order change"
    );
    // The DDL does not record whether dependency discovery is complete. Mark the
    // bounded whole snapshot, not just graph edges that happen to be registered.
    let preview = state.preview_t3(&[], &all_modules.iter().copied().collect::<Vec<_>>())?;
    let mut claims = Vec::new();
    for id in preview.impact.claims {
        let current = state
            .claim(&id)?
            .context("T3 claim disappeared while planning")?;
        let prior_trust = current.prior_trust.unwrap_or(current.local_trust);
        let old_proof = if prior_trust != Trust::L5 {
            ProofDisposition::NotL5
        } else if let Some(recipe) = state.managed_proof_recipe(&id)? {
            let spec = state.lean_spec(&id)?;
            if spec.is_some()
                && state
                    .lean_spec_comparison(&recipe.promoted_commit)?
                    .as_ref()
                    .is_none_or(|c| c.claim_id != id)
            {
                ProofDisposition::InvalidProvenance
            } else {
                diagnose_recipe(
                    locked,
                    engine,
                    &recipe,
                    old,
                    &after_rows,
                    &after_snapshot,
                    spec.as_ref(),
                )
            }
        } else {
            ProofDisposition::MissingManagedRecipe
        };
        claims.push(ClaimPlan {
            claim_id: id,
            prior_trust,
            old_proof,
        });
    }
    // If any claim, provenance, or graph changed between snapshots the digest
    // could mix generations. Re-run the dry-run instead of guessing.
    ensure!(
        state.status()?.last_event_seq == preview.event_seq,
        "ledger changed while computing T3 plan; rerun preview"
    );
    let affected_modules = preview.impact.modules;
    let plan_sha256 = sha(&serde_json::to_vec(&(
        "ai4math-t3-plan-v1",
        anchor,
        engine.as_str(),
        old,
        reviewed,
        preview.event_seq,
        &changed_modules,
        &affected_modules,
        &claims,
    ))?);
    Ok(Plan {
        base_commit: old.into(),
        reviewed_commit: reviewed.into(),
        ledger_event_seq: preview.event_seq,
        changed_modules,
        affected_modules,
        claims,
        plan_sha256,
    })
}

fn predecessors<'a>(sources: &'a [(&str, String)], index: usize) -> Vec<(&'a str, &'a str)> {
    sources[..index]
        .iter()
        .map(|(p, s)| (*p, s.as_str()))
        .collect()
}

fn diagnose_recipe(
    repo: &PromotionRepo,
    engine: Engine,
    recipe: &ManagedProofRecipe,
    old: &str,
    reviewed: &[(&str, String)],
    snapshot: &Promotion<'_>,
    spec: Option<&ReviewedLeanSpec>,
) -> ProofDisposition {
    let Some(index) = reviewed
        .iter()
        .position(|(name, _)| *name == recipe.file_path)
    else {
        return ProofDisposition::TargetChangedOrMissing;
    };
    let Ok((_, _, source)) = validate_recipe(repo, engine, recipe, old) else {
        return ProofDisposition::InvalidProvenance;
    };
    if source != reviewed[index].1 {
        return ProofDisposition::TargetChangedOrMissing;
    }
    if let Some(spec) = spec {
        if engine != Engine::Lean4 || reviewed_spec::check_snapshot(repo, snapshot, spec).is_err() {
            return ProofDisposition::InvalidProvenance;
        }
        if reviewed_spec::check_prefix(&predecessors(reviewed, index), spec).is_err() {
            return ProofDisposition::ReviewedSpecChangedOrMissing;
        }
    }
    ProofDisposition::Ready {
        base_commit: recipe.base_commit.clone(),
        promoted_commit: recipe.promoted_commit.clone(),
        source_sha256: recipe.source_sha256.clone(),
        reviewed_spec_prefix_sha256: spec.map(|s| s.spec_prefix_sha256.clone()),
    }
}

pub(crate) fn validate_recipe(
    repo: &PromotionRepo,
    engine: Engine,
    recipe: &ManagedProofRecipe,
    old: &str,
) -> Result<(FrozenTarget, ProofBodySubmission, String)> {
    ensure!(
        repo.is_ancestor(&recipe.promoted_commit, old)?
            && repo.is_ancestor(&recipe.base_commit, &recipe.promoted_commit)?,
        "managed recipe does not belong to the approved history"
    );
    let baseline = repo.snapshot(&recipe.file_path, Some(&recipe.base_commit))?;
    let promoted = repo.snapshot(&recipe.file_path, Some(&recipe.promoted_commit))?;
    let before = baseline
        .project()?
        .into_iter()
        .find(|(p, _)| *p == recipe.file_path)
        .context("frozen target missing")?
        .1;
    let actual = promoted
        .project()?
        .into_iter()
        .find(|(p, _)| *p == recipe.file_path)
        .context("promoted target missing")?
        .1;
    let (prefix, suffix) = before
        .split_once(HOLE)
        .context("recipe has no frozen proof marker")?;
    ensure!(
        !suffix.contains(HOLE),
        "recipe has ambiguous frozen proof markers"
    );
    let target = FrozenTarget {
        engine,
        target_id: recipe.target_decl.clone(),
        file_path: recipe.file_path.clone(),
        prefix: prefix.into(),
        suffix: suffix.into(),
    };
    let body = ProofBodySubmission {
        target_id: recipe.target_decl.clone(),
        proof_body: recipe.proof_body.clone(),
    };
    let rendered = render_proof_body(&target, &body)?;
    ensure!(
        rendered == actual && sha(actual.as_bytes()) == recipe.source_sha256,
        "recipe does not reproduce the approved proof blob"
    );
    Ok((target, body, rendered))
}
fn sha(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
