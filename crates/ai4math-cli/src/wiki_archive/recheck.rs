//! Sandboxed cold replay: an operator observation, not ledger evidence or promotion.
use super::{Compatibility, WakePreview, prepare_wake};
use crate::{
    print_json, promotion::PromotionRepo, reviewed_spec, t3::validate_recipe, trusted_toolchain,
};
use ai4math_broker::{Broker, RecheckAssessment};
use ai4math_core::Engine;
use ai4math_evidence::LeanComparisonEvidence;
use ai4math_project::{ManagedProject, open_managed_state};
use ai4math_store::{RetainedSedimentReplay, SedimentReplayScope, Trust, read_sediment};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Status {
    Verified,
    Deferred,
    Rejected,
    Unavailable,
}

#[derive(Debug, Serialize)]
struct Report {
    preview: WakePreview,
    status: Status,
    reason: &'static str,
    candidate_receipt_sha256: Option<String>,
    comparison_receipt_sha256: Option<String>,
    persisted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    observation: Option<RetainedSedimentReplay>,
    current_proof_authority: bool,
}

impl Report {
    fn new(preview: WakePreview) -> Self {
        Self {
            preview,
            status: Status::Deferred,
            reason: "not_attempted",
            candidate_receipt_sha256: None,
            comparison_receipt_sha256: None,
            persisted: false,
            observation: None,
            current_proof_authority: false,
        }
    }
    fn finish(mut self, status: Status, reason: &'static str) -> Self {
        self.status = status;
        self.reason = reason;
        self
    }
}

fn run(
    root: &Path,
    repo: &Path,
    id: &str,
    reviewed_main: &str,
    executable: &Path,
) -> Result<Report> {
    run_with_retention(root, repo, id, reviewed_main, executable, false)
}

fn run_with_retention(
    root: &Path,
    repo: &Path,
    id: &str,
    reviewed_main: &str,
    executable: &Path,
    retain: bool,
) -> Result<Report> {
    let managed = ManagedProject::new(root)?;
    let _native = managed.try_native_heavy()?;
    let git = PromotionRepo::try_lock(repo)?;
    let state = managed.open_state_readonly()?;
    let seq = state.status()?.last_event_seq;
    let still_current = || -> Result<()> {
        ensure!(
            git.main_head()? == reviewed_main
                && state.status()?.last_event_seq == seq
                && state.promotion_intent()?.is_none()
                && state.pivot_intent()?.is_none(),
            "main or project state changed during sediment replay; discard result"
        );
        Ok(())
    };
    let mut report = Report::new(prepare_wake(root, &git, &state, id, reviewed_main)?);
    let claim = state
        .claim(&report.preview.claim_id)?
        .context("unknown archived Claim")?;
    let recipe = state
        .managed_proof_recipe_at(&report.preview.claim_id, &report.preview.verified_at_commit)?
        .context("historical promotion no longer available")?;
    let formal_target = format!("{}#{}", recipe.file_path, recipe.target_decl);
    let defer = |report: Report, reason| {
        still_current()?;
        Ok(report.finish(Status::Deferred, reason))
    };
    if claim.local_trust == Trust::L2b || claim.effective_trust == Trust::L2b {
        return defer(report, "claim_refuted");
    }
    if claim.engine != report.preview.engine
        || claim.module_path.as_deref() != Some(recipe.file_path.as_str())
        || claim.formal_target.as_deref() != Some(formal_target.as_str())
    {
        return defer(report, "current_claim_target_changed");
    }
    if report.preview.engine != Engine::Lean4.as_str() {
        return defer(report, "lean_only");
    }
    match report.preview.compatibility {
        Compatibility::ModuleAbsent => return defer(report, "target_module_missing"),
        Compatibility::TargetChanged | Compatibility::TargetAndPrefixChanged => {
            return defer(report, "target_source_changed");
        }
        Compatibility::ProjectBytesMatch | Compatibility::PrefixChanged => (),
    }
    let binding = state.lean_spec(&claim.id)?;
    let owner = state.lean_spec_target_owner(&recipe.file_path, &recipe.target_decl)?;
    if owner.as_deref().is_some_and(|id| id != claim.id) {
        return defer(report, "reviewed_spec_owned_by_another_claim");
    }
    let original = state.lean_spec_comparison(&recipe.promoted_commit)?;
    if binding.is_none() && original.is_some() {
        return defer(report, "reviewed_spec_binding_missing");
    }
    if binding.is_none() && matches!(report.preview.compatibility, Compatibility::PrefixChanged) {
        return defer(report, "unbound_prefix_changed");
    }
    let snapshot = git.snapshot(&recipe.file_path, Some(reviewed_main))?;
    let project = snapshot.project()?;
    let index = project
        .iter()
        .position(|(module, _)| *module == recipe.file_path)
        .context("target missing from reviewed manifest")?;
    let dependencies: Vec<_> = project[..index]
        .iter()
        .map(|(p, src)| (*p, src.as_str()))
        .collect();
    let (target, proof, expected_source) =
        validate_recipe(&git, Engine::Lean4, &recipe, reviewed_main)?;
    ensure!(
        expected_source == project[index].1,
        "historical proof differs from reviewed target bytes"
    );
    if let Some(spec) = binding.as_ref() {
        let Some(row) = original.as_ref() else {
            return defer(report, "historical_spec_comparison_missing_or_different");
        };
        if row.spec_file != spec.spec_file
            || row.spec_decl != spec.spec_decl
            || row.spec_prefix_sha256 != spec.spec_prefix_sha256
            || !state.managed_lean_t1_matches_comparison(&recipe, row)?
        {
            return defer(report, "historical_spec_comparison_missing_or_different");
        }
        if reviewed_spec::check_snapshot(&git, &snapshot, spec).is_err()
            || reviewed_spec::check_prefix(&dependencies, spec).is_err()
        {
            return defer(report, "current_reviewed_spec_or_prefix_changed");
        }
    }
    let Ok(tool) = trusted_toolchain(Engine::Lean4, executable, None) else {
        still_current()?;
        return Ok(report.finish(Status::Unavailable, "lean_toolchain_unavailable"));
    };
    let broker = Broker::new();
    still_current()?;
    let assessed = broker.assess_recheck_with_dependencies(
        &tool,
        &target,
        &proof,
        reviewed_main,
        &dependencies,
    );
    still_current()?;
    let candidate = match assessed {
        RecheckAssessment::Verified(candidate) => candidate,
        RecheckAssessment::Rejected { .. } if binding.is_some() => {
            return defer(report, "no_positive_reviewed_spec_evidence");
        }
        RecheckAssessment::Rejected { .. } => {
            return Ok(report.finish(Status::Rejected, "native_proof_rejected"));
        }
        RecheckAssessment::Unavailable { .. } => {
            return Ok(report.finish(Status::Unavailable, "native_verification_unavailable"));
        }
    };
    ensure!(
        candidate.source() == expected_source
            && candidate.receipt().matches(
                reviewed_main,
                &recipe.file_path,
                project[index].1.as_bytes()
            ),
        "native replay differs from reviewed source"
    );
    let comparison = if let Some(spec) = binding.as_ref() {
        let compared = broker.compare_lean_spec(
            &tool,
            &candidate,
            &spec.spec_file,
            &spec.spec_decl,
            &dependencies,
        );
        still_current()?;
        let Ok(compared) = compared else {
            return defer(report, "reviewed_spec_comparison_not_confirmed");
        };
        report.comparison_receipt_sha256 = Some(compared.receipt_sha256().into());
        Some(compared)
    } else {
        None
    };
    report.candidate_receipt_sha256 = Some(candidate.receipt().receipt_sha256().into());
    still_current()?;
    if retain {
        // A report or a digest is never input to retention. Keep both real broker
        // values and both project locks alive through the SQLite write CAS.
        read_sediment(root, id).context("archive changed before observation retention")?;
        let mut writable = managed.open_state()?;
        still_current()?;
        report.observation = Some(
            writable.retain_sediment_replay(
                SedimentReplayScope {
                    repo: git
                        .path()
                        .to_str()
                        .context("managed repository path is not UTF-8")?,
                    archive_id: id,
                    claim_id: &claim.id,
                    promoted_commit: &recipe.promoted_commit,
                    reviewed_commit: reviewed_main,
                    expected_event_seq: seq,
                },
                &candidate,
                comparison
                    .as_ref()
                    .map(|c| c as &dyn LeanComparisonEvidence),
            )?,
        );
        report.persisted = true;
    }
    Ok(report.finish(Status::Verified, "replayed_in_isolation"))
}

pub(crate) fn command(root: &Path, repo: &Path, id: &str, head: &str, exec: &Path) -> Result<()> {
    print_json(run(root, repo, id, head, exec)?)
}

pub(crate) fn retain(root: &Path, repo: &Path, id: &str, head: &str, exec: &Path) -> Result<()> {
    print_json(run_with_retention(root, repo, id, head, exec, true)?)
}

pub(crate) fn result(root: &Path, id: &str) -> Result<()> {
    let state = open_managed_state(root, true)?;
    let observation = state
        .sediment_replay(id)?
        .context("no retained sediment replay observation")?;
    print_json(serde_json::json!({"observation":observation,"current_proof_authority":false}))
}

#[cfg(test)]
mod tests;
