//! Asynchronous 3-stage protocol verification lifecycle (`submit` -> `poll` -> `finalize`)
//! over any `ComputeBackend` (local CPU/GPU sandbox or SLURM cluster).
//!
//! Preserves all anti-drift guarantees across asynchronous/remote execution:
//! - enforces protocol `resource_ceiling` and backend epistemic ceilings before submit,
//! - shuffles candidate + hidden controls into opaque `tasks/0..K-1` Job Arrays,
//! - keeps the `BlindedArrayPlan` strictly local (never uploaded to compute nodes),
//! - runs broker-owned diagnostic parsing, sanity gates, and control separation locally,
//! - attaches `"unattested_remote_hpc"` epistemic debt for remote SLURM runs.

use crate::{Broker, ComputeBackend, ComputeJobPoll, SubmittedComputeJob, VerifiedProtocolRun};
use ai4math_core::{
    ComputeJobStatus, EpistemicLevel, ExecutionTargetKind, ResourceRequest, ScientificTrust,
};
use ai4math_evidence::{
    BlindedArrayBundleRequest, BlindedArrayPlan, BlindedArrayRole, BlindedControlPack,
    BlindedControlSummary, CandidateSubmission, ClusterMountEntry, ParsedDiagnostics, ProtocolSpec,
    ProtocolVerdict, VerifierReport, build_blinded_array_bundle, evaluate_acceptance_criteria,
    evaluate_sanity_gates,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

pub const UNATTESTED_REMOTE_HPC_DEBT: &str = "unattested_remote_hpc";

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Request to asynchronously submit one protocol verification job onto a
/// `ComputeBackend` (local GPU host or SLURM cluster).
pub struct AsyncProtocolSubmitRequest<'a> {
    pub job_id: &'a str,
    pub base_commit: &'a str,
    pub protocol: &'a ProtocolSpec,
    pub submission: &'a CandidateSubmission,
    pub blinded_controls: Option<&'a BlindedControlPack>,
    pub resources: ResourceRequest,
    pub verifier_sha256: &'a str,
    pub backend_max_positive_ceiling: ScientificTrust,
    pub backend_max_refutation_ceiling: Option<ScientificTrust>,
    pub backend_default_debts: &'a [&'a str],
    pub container_image_sha256: Option<String>,
    pub cluster_mounts: Vec<ClusterMountEntry>,
    pub output_file_per_task: &'a str,
    /// Optional 32-byte shuffle nonce (when `None`, broker samples fresh OS randomness).
    pub shuffle_nonce: Option<[u8; 32]>,
}

/// Durable local state for an in-flight asynchronous protocol job.
///
/// Includes the broker-private `blinded_plan` so daemon restarts can recover
/// and unblind the completed SLURM/local job without disclosing control indices
/// to worker agents or remote compute nodes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingProtocolJob {
    pub job_id: String,
    pub protocol_id: String,
    pub protocol_sha256: String,
    pub base_commit: String,
    pub candidate_id: String,
    pub candidate_sha256: String,
    pub rendered_artifacts_sha256: String,
    pub pinned_inputs_sha256: String,
    pub verifier_sha256: String,
    pub backend_max_positive_ceiling: ScientificTrust,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend_max_refutation_ceiling: Option<ScientificTrust>,
    #[serde(default)]
    pub backend_default_debts: Vec<String>,
    pub output_file_per_task: String,
    pub output_allowlist: Vec<String>,
    pub resources: ResourceRequest,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container_image_sha256: Option<String>,
    pub submitted: SubmittedComputeJob,
    pub blinded_plan: BlindedArrayPlan,
}

fn sample_shuffle_nonce() -> Result<[u8; 32]> {
    let mut nonce = [0u8; 32];
    #[cfg(unix)]
    {
        use std::{fs::File, io::Read};
        let mut f = File::open("/dev/urandom").context("open /dev/urandom for blinded shuffle")?;
        f.read_exact(&mut nonce)?;
    }
    #[cfg(not(unix))]
    {
        use sha2::{Digest, Sha256};
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let digest = Sha256::digest(nanos.to_be_bytes());
        nonce.copy_from_slice(&digest);
    }
    Ok(nonce)
}

impl Broker {
    /// Stage 1: Validate ceilings, compile candidate + hidden controls into an
    /// opaque blinded Job Array bundle, and submit onto `backend`.
    pub fn submit_protocol_job(
        &self,
        backend: &dyn ComputeBackend,
        request: AsyncProtocolSubmitRequest<'_>,
    ) -> Result<PendingProtocolJob> {
        request.protocol.validate()?;
        ensure!(
            is_sha256_hex(request.verifier_sha256),
            "verifier_sha256 must be lowercase 64-hex"
        );
        let claimed_rank = request
            .protocol
            .epistemic_ceiling
            .positive_rank()
            .context("protocol ceiling must be positive")?;
        let allowed_rank = request
            .backend_max_positive_ceiling
            .positive_rank()
            .context("backend max positive ceiling must be positive")?;
        ensure!(
            claimed_rank <= allowed_rank,
            "anti-laundering violation: protocol requests {} but backend ceiling is {}",
            request.protocol.epistemic_ceiling,
            request.backend_max_positive_ceiling
        );
        ensure!(
            request.protocol.orthogonal_policy.min_verifiers == 1,
            "single-backend async submit supports min_verifiers == 1; submit per verifier for orthogonal E4b"
        );

        let protocol_sha256 = request.protocol.protocol_sha256()?;
        let pinned_inputs_sha256 = request.protocol.pinned_inputs_sha256()?;
        let compiled_candidate = request.protocol.compile_submission(request.submission)?;
        let compiled_controls = if request.protocol.control_policy.requires_controls() {
            let pack = request.blinded_controls.context(
                "protocol requires broker-exclusive BlindedControlPack matching control_pack_sha256",
            )?;
            Some(pack.compile_for(request.protocol)?)
        } else {
            None
        };

        let nonce = match request.shuffle_nonce {
            Some(n) => n,
            None => sample_shuffle_nonce()?,
        };
        let (bundle, blinded_plan) = build_blinded_array_bundle(BlindedArrayBundleRequest {
            job_id: request.job_id,
            base_commit: request.base_commit,
            protocol: request.protocol,
            candidate: &compiled_candidate,
            controls: compiled_controls.as_ref(),
            resources: request.resources.clone(),
            container_image_sha256: request.container_image_sha256.clone(),
            cluster_mounts: request.cluster_mounts,
            output_file_per_task: request.output_file_per_task,
            shuffle_nonce: &nonce,
        })?;

        let submitted = backend.submit(&bundle)?;
        Ok(PendingProtocolJob {
            job_id: bundle.job_id,
            protocol_id: request.protocol.protocol_id.clone(),
            protocol_sha256,
            base_commit: request.base_commit.to_owned(),
            candidate_id: compiled_candidate.candidate_id().to_owned(),
            candidate_sha256: compiled_candidate.candidate_sha256().to_owned(),
            rendered_artifacts_sha256: compiled_candidate.rendered_artifacts_sha256().to_owned(),
            pinned_inputs_sha256,
            verifier_sha256: request.verifier_sha256.to_owned(),
            backend_max_positive_ceiling: request.backend_max_positive_ceiling,
            backend_max_refutation_ceiling: request.backend_max_refutation_ceiling,
            backend_default_debts: request
                .backend_default_debts
                .iter()
                .map(|s| (*s).to_owned())
                .collect(),
            output_file_per_task: request.output_file_per_task.to_owned(),
            output_allowlist: bundle.output_allowlist,
            resources: request.resources,
            container_image_sha256: request.container_image_sha256,
            submitted,
            blinded_plan,
        })
    }

    /// Stage 2: Poll job execution state on `backend`.
    pub fn poll_protocol_job(
        &self,
        backend: &dyn ComputeBackend,
        pending: &PendingProtocolJob,
    ) -> Result<ComputeJobPoll> {
        backend.poll(&pending.submitted)
    }

    /// Stage 3: Fetch allowlisted raw outputs from `backend`, unblind the
    /// Job Array locally, evaluate deterministic physical/numerical sanity gates
    /// and hidden control separation, and seal a broker `VerifiedProtocolRun`.
    pub fn finalize_protocol_job<F>(
        &self,
        backend: &dyn ComputeBackend,
        protocol: &ProtocolSpec,
        pending: &PendingProtocolJob,
        parser: F,
        max_fetch_bytes: u64,
    ) -> Result<VerifiedProtocolRun>
    where
        F: Fn(&[u8]) -> Result<ParsedDiagnostics>,
    {
        protocol.validate()?;
        pending.blinded_plan.validate()?;
        let protocol_sha256 = protocol.protocol_sha256()?;
        ensure!(
            protocol_sha256 == pending.protocol_sha256,
            "protocol_sha256 mismatch when finalizing async protocol job"
        );
        let poll = backend.poll(&pending.submitted)?;
        ensure!(
            poll.status.is_terminal(),
            "cannot finalize protocol job {:?} while status is {}",
            pending.job_id,
            poll.status
        );

        let mut debt_set: BTreeSet<String> = protocol.default_debts.iter().cloned().collect();
        for d in &pending.backend_default_debts {
            debt_set.insert(d.clone());
        }
        if pending.submitted.target_kind == ExecutionTargetKind::Slurm {
            debt_set.insert(UNATTESTED_REMOTE_HPC_DEBT.to_owned());
        }
        let epistemic_debts: Vec<String> = debt_set.into_iter().collect();
        let total_elapsed: f64 = poll
            .task_observations
            .iter()
            .map(|o| o.elapsed_seconds)
            .sum();

        if poll.status != ComputeJobStatus::Completed {
            let reason = format!(
                "compute job {:?} on {} ({}) terminated with status {}{}",
                pending.job_id,
                pending.submitted.cluster_or_host_id,
                pending.submitted.target_kind,
                poll.status,
                poll.detail
                    .as_deref()
                    .map(|d| format!(": {d}"))
                    .unwrap_or_default()
            );
            let control_summary = if pending.blinded_plan.control_pack_sha256.is_some() {
                let pos_total = pending
                    .blinded_plan
                    .slots
                    .iter()
                    .filter(|r| matches!(r, BlindedArrayRole::PositiveControl { .. }))
                    .count() as u32;
                let neg_total = pending
                    .blinded_plan
                    .slots
                    .iter()
                    .filter(|r| matches!(r, BlindedArrayRole::NegativeControl { .. }))
                    .count() as u32;
                BlindedControlSummary {
                    control_pack_sha256: pending.blinded_plan.control_pack_sha256.clone(),
                    positive_total: pos_total,
                    positive_passed: 0,
                    negative_total: neg_total,
                    negative_properly_rejected: 0,
                    all_controls_passed: false,
                }
            } else {
                BlindedControlSummary::not_configured()
            };
            return VerifiedProtocolRun::seal_from_broker(
                protocol.domain_id.clone(),
                protocol.backend_id.clone(),
                protocol.protocol_id.clone(),
                pending.protocol_sha256.clone(),
                pending.base_commit.clone(),
                pending.candidate_id.clone(),
                pending.candidate_sha256.clone(),
                pending.rendered_artifacts_sha256.clone(),
                pending.pinned_inputs_sha256.clone(),
                pending.verifier_sha256.clone(),
                ProtocolVerdict::VerifierError,
                ScientificTrust::E0,
                epistemic_debts,
                control_summary,
                Vec::new(),
                ParsedDiagnostics::default(),
                vec![reason],
                total_elapsed,
            );
        }

        let fetched = backend.fetch_outputs(
            &pending.submitted,
            &pending.output_allowlist,
            max_fetch_bytes,
        )?;

        // Parse and evaluate every array slot locally via broker-owned parser.
        let mut positive_total = 0u32;
        let mut positive_passed = 0u32;
        let mut negative_total = 0u32;
        let mut negative_properly_rejected = 0u32;
        let mut candidate_report: Option<VerifierReport> = None;
        let mut parse_errors: Vec<String> = Vec::new();

        for (array_idx, role) in pending.blinded_plan.slots.iter().enumerate() {
            let rel_path = format!("tasks/{array_idx}/{}", pending.output_file_per_task);
            let Some(raw_bytes) = fetched.files.get(&rel_path) else {
                parse_errors.push(format!("missing allowlisted output file {rel_path:?}"));
                match role {
                    BlindedArrayRole::PositiveControl { .. } => positive_total += 1,
                    BlindedArrayRole::NegativeControl { .. } => negative_total += 1,
                    BlindedArrayRole::Candidate => {}
                }
                continue;
            };
            let diag = match parser(raw_bytes).and_then(|d| {
                d.validate()?;
                Ok(d)
            }) {
                Ok(d) => d,
                Err(err) => {
                    parse_errors.push(format!(
                        "broker parser failed on array slot {array_idx}: {err}"
                    ));
                    match role {
                        BlindedArrayRole::PositiveControl { .. } => positive_total += 1,
                        BlindedArrayRole::NegativeControl { .. } => negative_total += 1,
                        BlindedArrayRole::Candidate => {}
                    }
                    continue;
                }
            };
            let sanity_failures = evaluate_sanity_gates(protocol, &diag);
            let sanity_passed = sanity_failures.is_empty();
            let acceptance_failures = if sanity_passed {
                evaluate_acceptance_criteria(protocol, &diag)
            } else {
                Vec::new()
            };
            let acceptance_passed = sanity_passed && acceptance_failures.is_empty();
            let task_elapsed = poll
                .task_observations
                .iter()
                .find(|o| o.array_task_id == array_idx as u32)
                .map_or(0.0, |o| o.elapsed_seconds);

            match role {
                BlindedArrayRole::PositiveControl { .. } => {
                    positive_total += 1;
                    if sanity_passed && acceptance_passed {
                        positive_passed += 1;
                    }
                }
                BlindedArrayRole::NegativeControl { .. } => {
                    negative_total += 1;
                    if sanity_passed && !acceptance_passed {
                        negative_properly_rejected += 1;
                    }
                }
                BlindedArrayRole::Candidate => {
                    candidate_report = Some(VerifierReport {
                        backend_id: protocol.backend_id.clone(),
                        verifier_sha256: pending.verifier_sha256.clone(),
                        elapsed_seconds: task_elapsed,
                        sanity_passed,
                        acceptance_passed,
                        sanity_failures,
                        acceptance_failures,
                        diagnostics: diag,
                    });
                }
            }
        }

        let control_summary = if pending.blinded_plan.control_pack_sha256.is_some() {
            BlindedControlSummary {
                control_pack_sha256: pending.blinded_plan.control_pack_sha256.clone(),
                positive_total,
                positive_passed,
                negative_total,
                negative_properly_rejected,
                all_controls_passed: positive_passed == positive_total
                    && negative_properly_rejected == negative_total,
            }
        } else {
            BlindedControlSummary::not_configured()
        };

        let mut failure_reasons = Vec::new();
        let (verdict, epistemic_level, reports, agg_diag) = if !control_summary.all_controls_passed
        {
            failure_reasons.push(format!(
                "blinded control separation failed in async Job Array (positive {}/{}, negative true-reject {}/{})",
                control_summary.positive_passed,
                control_summary.positive_total,
                control_summary.negative_properly_rejected,
                control_summary.negative_total
            ));
            (
                ProtocolVerdict::ControlDriftDetected,
                ScientificTrust::E0,
                Vec::new(),
                ParsedDiagnostics::default(),
            )
        } else if !parse_errors.is_empty() || candidate_report.is_none() {
            failure_reasons.extend(parse_errors);
            if failure_reasons.is_empty() {
                failure_reasons.push("missing parsed candidate report".to_owned());
            }
            (
                ProtocolVerdict::VerifierError,
                ScientificTrust::E0,
                Vec::new(),
                ParsedDiagnostics::default(),
            )
        } else {
            let report = candidate_report.expect("checked Some");
            let diag = report.diagnostics.clone();
            if !report.sanity_passed {
                for fail in &report.sanity_failures {
                    failure_reasons.push(format!(
                        "[{}] sanity gate {:?} ({}): {}",
                        report.backend_id, fail.rule_id, fail.metric, fail.reason
                    ));
                }
                (
                    ProtocolVerdict::SanityGateFailed,
                    ScientificTrust::E0,
                    vec![report],
                    diag,
                )
            } else if report.acceptance_passed {
                (
                    ProtocolVerdict::Passed,
                    protocol.epistemic_ceiling,
                    vec![report],
                    diag,
                )
            } else {
                for fail in &report.acceptance_failures {
                    failure_reasons.push(format!(
                        "[{}] criterion {:?} ({}): {}",
                        report.backend_id, fail.rule_id, fail.metric, fail.reason
                    ));
                }
                let ref_level = match pending
                    .backend_max_refutation_ceiling
                    .and_then(EpistemicLevel::refutation_rank)
                {
                    Some(rank)
                        if rank >= 2 && protocol.epistemic_ceiling == ScientificTrust::E5 =>
                    {
                        ScientificTrust::E2b
                    }
                    Some(rank) if rank >= 1 => ScientificTrust::E2a,
                    _ => ScientificTrust::E0,
                };
                (ProtocolVerdict::Rejected, ref_level, vec![report], diag)
            }
        };

        VerifiedProtocolRun::seal_from_broker(
            protocol.domain_id.clone(),
            protocol.backend_id.clone(),
            protocol.protocol_id.clone(),
            pending.protocol_sha256.clone(),
            pending.base_commit.clone(),
            pending.candidate_id.clone(),
            pending.candidate_sha256.clone(),
            pending.rendered_artifacts_sha256.clone(),
            pending.pinned_inputs_sha256.clone(),
            pending.verifier_sha256.clone(),
            verdict,
            epistemic_level,
            epistemic_debts,
            control_summary,
            reports,
            agg_diag,
            failure_reasons,
            total_elapsed,
        )
    }
}
