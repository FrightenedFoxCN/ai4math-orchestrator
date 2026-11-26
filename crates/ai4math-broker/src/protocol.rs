//! Broker-sealed anti-drift protocol execution engine.
//!
//! Generalizes `oracle.rs` from two-solver SMT probing to arbitrary domain
//! protocols (e.g. computational chemistry / computational biology) with:
//! - hard backend epistemic ceilings (anti-laundering),
//! - single-pass typed slot compilation (no raw untrusted scripts/inputs),
//! - broker-owned deterministic diagnostic parsing & physical sanity gates,
//! - broker-exclusive blinded positive/negative controls (control-drift gate),
//! - orthogonal multi-verifier consensus & explicit disagreement verdicts, and
//! - sealed `VerifiedProtocolRun` receipts implementing `ProtocolRunEvidence`.

use super::{Broker, Lane, Pressure, digest, file_digest, resources, sandbox};
use ai4math_core::{EpistemicLevel, ScientificTrust};
use ai4math_evidence::{
    BlindedControlPack, BlindedControlSummary, CandidateSubmission, CompiledProtocolRun,
    DiagnosticValue, ParsedDiagnostics, ProtocolRunEvidence, ProtocolSpec, ProtocolVerdict,
    SealedReceipt, VerifierReport, evaluate_acceptance_criteria, evaluate_orthogonal_agreement,
    evaluate_sanity_gates,
};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// Resource lane requested for a protocol verification run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolLane {
    /// Interactive / fast probes subject to live host memory backpressure.
    Fast,
    /// Heavy clean-room evaluations serialized through the Heavy lane.
    Heavy,
}

/// Raw output captured from a backend runner prior to broker-owned parsing.
#[derive(Debug, Clone)]
pub struct RawBackendOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
    pub elapsed_seconds: f64,
    pub structured_payload: Option<Value>,
}

/// Operator-trusted protocol verifier backend executed exclusively by the broker.
pub trait ProtocolBackendRunner: Sync {
    fn backend_id(&self) -> &str;
    /// Lowercase 64-hex SHA-256 of the trusted verifier executable/container/parser.
    fn verifier_sha256(&self) -> Result<String>;
    /// Hard positive epistemic ceiling this backend may ever mint.
    fn max_positive_ceiling(&self) -> ScientificTrust;
    /// Highest refutation level this backend may mint (`None` = advisory only, cannot refute).
    fn max_refutation_ceiling(&self) -> Option<ScientificTrust>;
    /// Mandatory unresolved assumption/fidelity debt codes attached to receipts.
    fn default_debts(&self) -> &'static [&'static str];
    /// Execute one compiled protocol bundle inside an isolated working context.
    fn execute(&self, compiled: &CompiledProtocolRun) -> Result<RawBackendOutput>;
    /// Broker-owned deterministic parser extracting structured diagnostics.
    fn parse_diagnostics(&self, raw: &RawBackendOutput) -> Result<ParsedDiagnostics>;
}

/// Reusable OS-sandboxed native executable runner built on `crate::sandbox`.
///
/// Verifies the executable SHA-256 both before and after execution (matching
/// `oracle.rs`), stages compiled protocol artifacts safely into a fresh clean-room
/// directory, runs with zero network access, and delegates stdout parsing to a
/// broker-owned deterministic parser function.
pub struct SandboxedExecutableRunner<F> {
    backend_id: String,
    executable: PathBuf,
    binary_sha256: String,
    args: Vec<String>,
    reads: Vec<PathBuf>,
    timeout: Duration,
    max_positive_ceiling: ScientificTrust,
    max_refutation_ceiling: Option<ScientificTrust>,
    default_debts: &'static [&'static str],
    parser: F,
}

impl<F> SandboxedExecutableRunner<F>
where
    F: Fn(&RawBackendOutput) -> Result<ParsedDiagnostics> + Sync,
{
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        backend_id: impl Into<String>,
        executable: impl AsRef<Path>,
        args: Vec<String>,
        reads: Vec<PathBuf>,
        timeout: Duration,
        max_positive_ceiling: ScientificTrust,
        max_refutation_ceiling: Option<ScientificTrust>,
        default_debts: &'static [&'static str],
        parser: F,
    ) -> Result<Self> {
        let executable = executable
            .as_ref()
            .canonicalize()
            .context("resolve trusted protocol verifier executable")?;
        let binary_sha256 = file_digest(&executable)?;
        let mut canonical_reads = Vec::with_capacity(reads.len());
        for path in reads {
            canonical_reads.push(path.canonicalize().context("resolve protocol read mount")?);
        }
        Ok(Self {
            backend_id: backend_id.into(),
            executable,
            binary_sha256,
            args,
            reads: canonical_reads,
            timeout,
            max_positive_ceiling,
            max_refutation_ceiling,
            default_debts,
            parser,
        })
    }
}

impl<F> ProtocolBackendRunner for SandboxedExecutableRunner<F>
where
    F: Fn(&RawBackendOutput) -> Result<ParsedDiagnostics> + Sync,
{
    fn backend_id(&self) -> &str {
        &self.backend_id
    }

    fn verifier_sha256(&self) -> Result<String> {
        let current = file_digest(&self.executable)?;
        ensure!(
            current == self.binary_sha256,
            "verifier executable changed since registration"
        );
        Ok(current)
    }

    fn max_positive_ceiling(&self) -> ScientificTrust {
        self.max_positive_ceiling
    }

    fn max_refutation_ceiling(&self) -> Option<ScientificTrust> {
        self.max_refutation_ceiling
    }

    fn default_debts(&self) -> &'static [&'static str] {
        self.default_debts
    }

    fn execute(&self, compiled: &CompiledProtocolRun) -> Result<RawBackendOutput> {
        ensure!(
            file_digest(&self.executable)? == self.binary_sha256,
            "verifier executable changed before sandbox execution"
        );
        let room = tempfile::tempdir()?;
        let scratch = room.path().join("scratch");
        fs::create_dir(&scratch)?;
        for (rel_path, content) in compiled.artifacts() {
            let dest = room.path().join(rel_path);
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&dest, content)?;
        }
        let started = Instant::now();
        let output = sandbox::run(sandbox::Invocation {
            executable: &self.executable,
            args: &self.args,
            room: room.path(),
            writable: &scratch,
            reads: &self.reads,
            env: &[],
            timeout: self.timeout,
        });
        let elapsed_seconds = started.elapsed().as_secs_f64();
        ensure!(
            file_digest(&self.executable)? == self.binary_sha256,
            "verifier executable changed while running"
        );
        match output {
            Ok(stdout) => Ok(RawBackendOutput {
                stdout,
                stderr: String::new(),
                exit_code: Some(0),
                elapsed_seconds,
                structured_payload: None,
            }),
            Err(err) => {
                let code = err
                    .downcast_ref::<sandbox::ChildExit>()
                    .and_then(|exit| exit.status.code());
                Ok(RawBackendOutput {
                    stdout: String::new(),
                    stderr: err.to_string(),
                    exit_code: code,
                    elapsed_seconds,
                    structured_payload: None,
                })
            }
        }
    }

    fn parse_diagnostics(&self, raw: &RawBackendOutput) -> Result<ParsedDiagnostics> {
        (self.parser)(raw)
    }
}

/// Request passed to `Broker::verify_protocol`.
pub struct ProtocolVerificationRequest<'a> {
    pub base_commit: &'a str,
    pub protocol: &'a ProtocolSpec,
    pub submission: &'a CandidateSubmission,
    pub blinded_controls: Option<&'a BlindedControlPack>,
    pub runners: &'a [&'a dyn ProtocolBackendRunner],
    pub lane: ProtocolLane,
}

/// Broker-sealed verification receipt for one protocol run.
///
/// Fields are private and no `Deserialize` implementation exists, so callers and
/// subagents cannot fabricate a `VerifiedProtocolRun` from JSON.
#[derive(Debug, Serialize)]
#[must_use]
pub struct VerifiedProtocolRun {
    domain_id: String,
    backend_id: String,
    protocol_id: String,
    protocol_sha256: String,
    base_commit: String,
    candidate_id: String,
    candidate_sha256: String,
    rendered_artifacts_sha256: String,
    pinned_inputs_sha256: String,
    verifier_sha256: String,
    verdict: ProtocolVerdict,
    epistemic_level: ScientificTrust,
    epistemic_debts: Vec<String>,
    control_summary: BlindedControlSummary,
    verifier_reports: Vec<VerifierReport>,
    aggregated_diagnostics: ParsedDiagnostics,
    failure_reasons: Vec<String>,
    issued_unix_seconds: u64,
    elapsed_seconds: f64,
    receipt_sha256: String,
}

impl VerifiedProtocolRun {
    pub fn verdict(&self) -> ProtocolVerdict {
        self.verdict
    }

    pub fn epistemic_level_typed(&self) -> ScientificTrust {
        self.epistemic_level
    }

    pub fn epistemic_debts(&self) -> &[String] {
        &self.epistemic_debts
    }

    pub fn control_summary(&self) -> &BlindedControlSummary {
        &self.control_summary
    }

    pub fn verifier_reports(&self) -> &[VerifierReport] {
        &self.verifier_reports
    }

    pub fn failure_reasons(&self) -> &[String] {
        &self.failure_reasons
    }

    pub fn receipt_sha256(&self) -> &str {
        &self.receipt_sha256
    }
}

// SAFETY: Constructed exclusively inside `Broker::verify_protocol` after validating
// backend ceilings, compiling typed candidate slots, evaluating hidden blinded
// controls, enforcing physical/numerical sanity gates, and checking orthogonal
// agreement. No deserializer is exposed.
unsafe impl SealedReceipt for VerifiedProtocolRun {
    fn domain_id(&self) -> &str {
        &self.domain_id
    }

    fn backend_id(&self) -> &str {
        &self.backend_id
    }

    fn target_id(&self) -> &str {
        &self.candidate_id
    }

    fn base_commit(&self) -> &str {
        &self.base_commit
    }

    fn spec_sha256(&self) -> &str {
        &self.protocol_sha256
    }

    fn candidate_sha256(&self) -> &str {
        &self.candidate_sha256
    }

    fn inputs_sha256(&self) -> &str {
        &self.pinned_inputs_sha256
    }

    fn verifier_sha256(&self) -> &str {
        &self.verifier_sha256
    }

    fn epistemic_level(&self) -> &str {
        self.epistemic_level.as_str()
    }

    fn epistemic_debts(&self) -> &[String] {
        &self.epistemic_debts
    }

    fn issued_unix_seconds(&self) -> u64 {
        self.issued_unix_seconds
    }

    fn elapsed_seconds(&self) -> f64 {
        self.elapsed_seconds
    }

    fn receipt_sha256(&self) -> &str {
        &self.receipt_sha256
    }

    fn raw_json(&self) -> Result<Value> {
        Ok(serde_json::to_value(self)?)
    }
}

// SAFETY: Same broker-exclusive construction invariant as `SealedReceipt`.
unsafe impl ProtocolRunEvidence for VerifiedProtocolRun {
    fn protocol_id(&self) -> &str {
        &self.protocol_id
    }

    fn protocol_sha256(&self) -> &str {
        &self.protocol_sha256
    }

    fn candidate_id(&self) -> &str {
        &self.candidate_id
    }

    fn rendered_artifacts_sha256(&self) -> &str {
        &self.rendered_artifacts_sha256
    }

    fn verdict(&self) -> ProtocolVerdict {
        self.verdict
    }

    fn control_summary(&self) -> &BlindedControlSummary {
        &self.control_summary
    }

    fn verifier_reports(&self) -> &[VerifierReport] {
        &self.verifier_reports
    }

    fn aggregated_diagnostics(&self) -> &ParsedDiagnostics {
        &self.aggregated_diagnostics
    }

    fn failure_reasons(&self) -> &[String] {
        &self.failure_reasons
    }
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn is_git_commit_oid(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn run_single_verifier(
    protocol: &ProtocolSpec,
    runner: &dyn ProtocolBackendRunner,
    verifier_sha256: String,
    compiled: &CompiledProtocolRun,
) -> std::result::Result<VerifierReport, String> {
    let raw = runner
        .execute(compiled)
        .map_err(|e| format!("runner {:?} execution failed: {e}", runner.backend_id()))?;
    if !raw.elapsed_seconds.is_finite() || raw.elapsed_seconds < 0.0 {
        return Err(format!(
            "runner {:?} reported invalid elapsed_seconds",
            runner.backend_id()
        ));
    }
    if let Some(code) = raw.exit_code
        && code != 0
    {
        return Err(format!(
            "runner {:?} exited with non-zero status {code}",
            runner.backend_id()
        ));
    }
    let diagnostics = runner.parse_diagnostics(&raw).map_err(|e| {
        format!(
            "runner {:?} diagnostic parsing failed: {e}",
            runner.backend_id()
        )
    })?;
    diagnostics.validate().map_err(|e| {
        format!(
            "runner {:?} produced invalid diagnostics: {e}",
            runner.backend_id()
        )
    })?;

    let sanity_failures = evaluate_sanity_gates(protocol, &diagnostics);
    let sanity_passed = sanity_failures.is_empty();
    let acceptance_failures = if sanity_passed {
        evaluate_acceptance_criteria(protocol, &diagnostics)
    } else {
        Vec::new()
    };
    let acceptance_passed = sanity_passed && acceptance_failures.is_empty();

    Ok(VerifierReport {
        backend_id: runner.backend_id().to_owned(),
        verifier_sha256,
        elapsed_seconds: raw.elapsed_seconds,
        sanity_passed,
        acceptance_passed,
        sanity_failures,
        acceptance_failures,
        diagnostics,
    })
}

fn aggregate_diagnostics(reports: &[VerifierReport]) -> ParsedDiagnostics {
    if reports.is_empty() {
        return ParsedDiagnostics::default();
    }
    if reports.len() == 1 {
        return reports[0].diagnostics.clone();
    }
    let mut keys: BTreeSet<&str> = BTreeSet::new();
    for report in reports {
        for key in report.diagnostics.metrics.keys() {
            keys.insert(key.as_str());
        }
    }
    let mut metrics = BTreeMap::new();
    for key in keys {
        let values: Vec<&DiagnosticValue> = reports
            .iter()
            .filter_map(|r| r.diagnostics.metrics.get(key))
            .collect();
        if values.len() != reports.len() {
            continue;
        }
        if values
            .iter()
            .all(|v| matches!(v, DiagnosticValue::Float(_)))
        {
            let sum: f64 = values.iter().filter_map(|v| v.as_finite_f64()).sum();
            let mean = sum / (values.len() as f64);
            if mean.is_finite() {
                metrics.insert(
                    key.to_owned(),
                    DiagnosticValue::Float(if mean == 0.0 { 0.0 } else { mean }),
                );
            }
        } else if values.windows(2).all(|w| w[0] == w[1]) {
            metrics.insert(key.to_owned(), values[0].clone());
        }
    }
    ParsedDiagnostics { metrics }
}

fn refutation_level_for_clean_rejection(
    protocol: &ProtocolSpec,
    runners: &[&dyn ProtocolBackendRunner],
) -> ScientificTrust {
    let mut min_ref_rank: Option<u8> = None;
    for runner in runners {
        let Some(rank) = runner
            .max_refutation_ceiling()
            .and_then(EpistemicLevel::refutation_rank)
        else {
            return ScientificTrust::E0;
        };
        min_ref_rank = Some(match min_ref_rank {
            None => rank,
            Some(curr) => curr.min(rank),
        });
    }
    match min_ref_rank {
        Some(rank)
            if rank >= 2
                && (runners.len() >= 2 || protocol.epistemic_ceiling == ScientificTrust::E5) =>
        {
            ScientificTrust::E2b
        }
        Some(rank) if rank >= 1 => ScientificTrust::E2a,
        _ => ScientificTrust::E0,
    }
}

impl Broker {
    /// Execute a frozen `ProtocolSpec` in the broker clean-room, enforcing
    /// backend epistemic ceilings, blinded control separation, deterministic
    /// physical/numerical sanity gates, and orthogonal multi-verifier consensus.
    pub fn verify_protocol(
        &self,
        request: ProtocolVerificationRequest<'_>,
    ) -> Result<VerifiedProtocolRun> {
        ensure!(
            is_git_commit_oid(request.base_commit),
            "expected a lowercase 40- or 64-hex Git commit OID"
        );
        request.protocol.validate()?;
        ensure!(
            !request.runners.is_empty(),
            "at least one trusted ProtocolBackendRunner is required"
        );
        ensure!(
            request.runners.len() >= usize::from(request.protocol.orthogonal_policy.min_verifiers),
            "protocol requires at least {} verifiers, received {}",
            request.protocol.orthogonal_policy.min_verifiers,
            request.runners.len()
        );

        let protocol_sha256 = request.protocol.protocol_sha256()?;
        let pinned_inputs_sha256 = request.protocol.pinned_inputs_sha256()?;

        // 1. Enforce backend epistemic ceilings and verifier digest integrity BEFORE execution.
        let claimed_rank = request
            .protocol
            .epistemic_ceiling
            .positive_rank()
            .context("protocol ceiling must be positive")?;
        let mut runner_digests = Vec::with_capacity(request.runners.len());
        let mut distinct_backends = BTreeSet::new();
        let mut distinct_digests = BTreeSet::new();
        let mut debt_set: BTreeSet<String> =
            request.protocol.default_debts.iter().cloned().collect();

        for runner in request.runners {
            let max_rank = runner
                .max_positive_ceiling()
                .positive_rank()
                .context("runner ceiling must be positive")?;
            ensure!(
                claimed_rank <= max_rank,
                "anti-laundering violation: protocol requests {} but backend {:?} ceiling is {}",
                request.protocol.epistemic_ceiling,
                runner.backend_id(),
                runner.max_positive_ceiling()
            );
            let sha = runner.verifier_sha256()?;
            ensure!(
                is_sha256_hex(&sha),
                "runner {:?} returned non-SHA-256 verifier digest",
                runner.backend_id()
            );
            distinct_backends.insert(runner.backend_id());
            distinct_digests.insert(sha.clone());
            runner_digests.push(sha);
            for debt in runner.default_debts() {
                debt_set.insert((*debt).to_owned());
            }
        }
        if request.protocol.orthogonal_policy.require_distinct_backends && request.runners.len() > 1
        {
            ensure!(
                distinct_backends.len() == request.runners.len()
                    && distinct_digests.len() == request.runners.len(),
                "orthogonal protocol requires distinct verifier backends and binary digests"
            );
        }

        // 2. Compile untrusted candidate submission against frozen slots & templates.
        let compiled_candidate = request.protocol.compile_submission(request.submission)?;

        // 3. Validate and compile broker-exclusive blinded controls if configured.
        let compiled_controls = if request.protocol.control_policy.requires_controls() {
            let pack = request.blinded_controls.context(
                "protocol requires a broker-exclusive BlindedControlPack matching control_pack_sha256",
            )?;
            Some(pack.compile_for(request.protocol)?)
        } else {
            None
        };

        // 4. Acquire broker lane lease.
        let _lease = match request.lane {
            ProtocolLane::Fast => self
                .resources
                .acquire(Lane::Fast, resources::host_pressure()?)?,
            ProtocolLane::Heavy => self.resources.acquire(Lane::Heavy, Pressure::Red)?,
        };

        let started = Instant::now();
        let combined_verifier_sha256 = if runner_digests.len() == 1 {
            runner_digests[0].clone()
        } else {
            digest(&serde_json::to_vec(&(
                "ai4math:protocol-verifiers:v1",
                &runner_digests,
            ))?)
        };

        // 5. Execute blinded positive & negative controls FIRST inside clean-room.
        let mut failure_reasons = Vec::new();
        let control_summary = if let Some(controls) = &compiled_controls {
            let mut positive_passed = 0u32;
            for pos in &controls.positive {
                let mut pos_reports = Vec::with_capacity(request.runners.len());
                let mut err_seen = false;
                for (runner, sha) in request.runners.iter().zip(&runner_digests) {
                    match run_single_verifier(request.protocol, *runner, sha.clone(), pos) {
                        Ok(report) => pos_reports.push(report),
                        Err(_) => {
                            err_seen = true;
                            break;
                        }
                    }
                }
                let ok = !err_seen
                    && pos_reports.len() == request.runners.len()
                    && pos_reports
                        .iter()
                        .all(|r| r.sanity_passed && r.acceptance_passed)
                    && evaluate_orthogonal_agreement(
                        &request.protocol.orthogonal_policy,
                        &pos_reports,
                    )
                    .is_empty();
                if ok {
                    positive_passed += 1;
                }
            }

            let mut negative_properly_rejected = 0u32;
            for neg in &controls.negative {
                let mut neg_reports = Vec::with_capacity(request.runners.len());
                let mut err_seen = false;
                for (runner, sha) in request.runners.iter().zip(&runner_digests) {
                    match run_single_verifier(request.protocol, *runner, sha.clone(), neg) {
                        Ok(report) => neg_reports.push(report),
                        Err(_) => {
                            err_seen = true;
                            break;
                        }
                    }
                }
                // A negative control must be physically well-behaved (sanity passes)
                // AND cleanly rejected by the protocol acceptance criteria across all verifiers.
                let ok = !err_seen
                    && neg_reports.len() == request.runners.len()
                    && neg_reports
                        .iter()
                        .all(|r| r.sanity_passed && !r.acceptance_passed);
                if ok {
                    negative_properly_rejected += 1;
                }
            }

            let positive_total = u32::try_from(controls.positive.len())?;
            let negative_total = u32::try_from(controls.negative.len())?;
            let all_controls_passed =
                positive_passed == positive_total && negative_properly_rejected == negative_total;
            BlindedControlSummary {
                control_pack_sha256: Some(controls.control_pack_sha256.clone()),
                positive_total,
                positive_passed,
                negative_total,
                negative_properly_rejected,
                all_controls_passed,
            }
        } else {
            BlindedControlSummary::not_configured()
        };

        let (verdict, epistemic_level, verifier_reports, aggregated_diagnostics) =
            if !control_summary.all_controls_passed {
                failure_reasons.push(format!(
                    "blinded control separation failed (positive {}/{}, negative true-reject {}/{})",
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
            } else {
                // 6. Execute candidate across all trusted verifier runners.
                let mut reports = Vec::with_capacity(request.runners.len());
                let mut exec_errors = Vec::new();
                for (runner, sha) in request.runners.iter().zip(&runner_digests) {
                    match run_single_verifier(
                        request.protocol,
                        *runner,
                        sha.clone(),
                        &compiled_candidate,
                    ) {
                        Ok(report) => reports.push(report),
                        Err(err) => exec_errors.push(err),
                    }
                }
                if !exec_errors.is_empty() {
                    failure_reasons.extend(exec_errors);
                    let agg = aggregate_diagnostics(&reports);
                    (
                        ProtocolVerdict::VerifierError,
                        ScientificTrust::E0,
                        reports,
                        agg,
                    )
                } else {
                    let agg = aggregate_diagnostics(&reports);
                    let orthogonal_conflicts = if reports.len() > 1
                        || request.protocol.orthogonal_policy.min_verifiers > 1
                    {
                        evaluate_orthogonal_agreement(&request.protocol.orthogonal_policy, &reports)
                    } else {
                        Vec::new()
                    };

                    if !orthogonal_conflicts.is_empty() {
                        failure_reasons.extend(orthogonal_conflicts);
                        (
                            ProtocolVerdict::Disagreement,
                            ScientificTrust::E0,
                            reports,
                            agg,
                        )
                    } else if reports.iter().any(|r| !r.sanity_passed) {
                        for r in &reports {
                            for fail in &r.sanity_failures {
                                failure_reasons.push(format!(
                                    "[{}] sanity gate {:?} ({}): {}",
                                    r.backend_id, fail.rule_id, fail.metric, fail.reason
                                ));
                            }
                        }
                        (
                            ProtocolVerdict::SanityGateFailed,
                            ScientificTrust::E0,
                            reports,
                            agg,
                        )
                    } else if reports.iter().all(|r| r.acceptance_passed) {
                        (
                            ProtocolVerdict::Passed,
                            request.protocol.epistemic_ceiling,
                            reports,
                            agg,
                        )
                    } else {
                        for r in &reports {
                            for fail in &r.acceptance_failures {
                                failure_reasons.push(format!(
                                    "[{}] criterion {:?} ({}): {}",
                                    r.backend_id, fail.rule_id, fail.metric, fail.reason
                                ));
                            }
                        }
                        let ref_level =
                            refutation_level_for_clean_rejection(request.protocol, request.runners);
                        (ProtocolVerdict::Rejected, ref_level, reports, agg)
                    }
                }
            };

        let elapsed_seconds = started.elapsed().as_secs_f64();
        let issued_unix_seconds = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let epistemic_debts: Vec<String> = debt_set.into_iter().collect();

        #[derive(Serialize)]
        struct ReceiptPreimage<'a> {
            domain: &'static str,
            domain_id: &'a str,
            backend_id: &'a str,
            protocol_id: &'a str,
            protocol_sha256: &'a str,
            base_commit: &'a str,
            candidate_id: &'a str,
            candidate_sha256: &'a str,
            rendered_artifacts_sha256: &'a str,
            pinned_inputs_sha256: &'a str,
            verifier_sha256: &'a str,
            verdict: ProtocolVerdict,
            epistemic_level: ScientificTrust,
            epistemic_debts: &'a [String],
            control_summary: &'a BlindedControlSummary,
            verifier_reports: &'a [VerifierReport],
            failure_reasons: &'a [String],
        }
        let receipt_sha256 = digest(&serde_json::to_vec(&ReceiptPreimage {
            domain: "ai4math:protocol-run-receipt:v1",
            domain_id: &request.protocol.domain_id,
            backend_id: &request.protocol.backend_id,
            protocol_id: &request.protocol.protocol_id,
            protocol_sha256: &protocol_sha256,
            base_commit: request.base_commit,
            candidate_id: compiled_candidate.candidate_id(),
            candidate_sha256: compiled_candidate.candidate_sha256(),
            rendered_artifacts_sha256: compiled_candidate.rendered_artifacts_sha256(),
            pinned_inputs_sha256: &pinned_inputs_sha256,
            verifier_sha256: &combined_verifier_sha256,
            verdict,
            epistemic_level,
            epistemic_debts: &epistemic_debts,
            control_summary: &control_summary,
            verifier_reports: &verifier_reports,
            failure_reasons: &failure_reasons,
        })?);

        Ok(VerifiedProtocolRun {
            domain_id: request.protocol.domain_id.clone(),
            backend_id: request.protocol.backend_id.clone(),
            protocol_id: request.protocol.protocol_id.clone(),
            protocol_sha256,
            base_commit: request.base_commit.to_owned(),
            candidate_id: compiled_candidate.candidate_id().to_owned(),
            candidate_sha256: compiled_candidate.candidate_sha256().to_owned(),
            rendered_artifacts_sha256: compiled_candidate.rendered_artifacts_sha256().to_owned(),
            pinned_inputs_sha256,
            verifier_sha256: combined_verifier_sha256,
            verdict,
            epistemic_level,
            epistemic_debts,
            control_summary,
            verifier_reports,
            aggregated_diagnostics,
            failure_reasons,
            issued_unix_seconds,
            elapsed_seconds,
            receipt_sha256,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ai4math_evidence::{
        ArtifactTemplate, BlindedControlCase, BlindedControlPolicy, MetricRule, NamedMetricRule,
        OrthogonalPolicy, ParamValue, SlotKind, SlotSpec, SlotValue, TextProfile,
    };

    struct MockCompChemRunner {
        backend: &'static str,
        sha: String,
        ceiling: ScientificTrust,
        ref_ceiling: Option<ScientificTrust>,
        debts: &'static [&'static str],
        evaluator: Box<dyn Fn(&CompiledProtocolRun) -> ParsedDiagnostics + Send + Sync>,
    }

    impl ProtocolBackendRunner for MockCompChemRunner {
        fn backend_id(&self) -> &str {
            self.backend
        }

        fn verifier_sha256(&self) -> Result<String> {
            Ok(self.sha.clone())
        }

        fn max_positive_ceiling(&self) -> ScientificTrust {
            self.ceiling
        }

        fn max_refutation_ceiling(&self) -> Option<ScientificTrust> {
            self.ref_ceiling
        }

        fn default_debts(&self) -> &'static [&'static str] {
            self.debts
        }

        fn execute(&self, compiled: &CompiledProtocolRun) -> Result<RawBackendOutput> {
            let diag = (self.evaluator)(compiled);
            Ok(RawBackendOutput {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: Some(0),
                elapsed_seconds: 0.05,
                structured_payload: Some(serde_json::to_value(diag)?),
            })
        }

        fn parse_diagnostics(&self, raw: &RawBackendOutput) -> Result<ParsedDiagnostics> {
            let payload = raw
                .structured_payload
                .clone()
                .context("missing mock payload")?;
            Ok(serde_json::from_value(payload)?)
        }
    }

    fn base_protocol(ceiling: ScientificTrust) -> ProtocolSpec {
        ProtocolSpec {
            version: 1,
            protocol_id: "dft_barrier_v1".into(),
            domain_id: "compchem".into(),
            backend_id: "dft_irc".into(),
            epistemic_ceiling: ceiling,
            default_debts: vec!["implicit_solvent".into()],
            pinned_inputs: BTreeMap::from([("catalyst_xyz".into(), "a".repeat(64))]),
            frozen_parameters: BTreeMap::from([
                ("functional".into(), ParamValue::Text("wB97X-D".into())),
                ("basis".into(), ParamValue::Text("def2-TZVP".into())),
            ]),
            slots: vec![SlotSpec {
                name: "substrate_smiles".into(),
                kind: SlotKind::Text {
                    max_len: 128,
                    profile: TextProfile::Smiles,
                },
            }],
            artifact_templates: vec![ArtifactTemplate {
                path: "job.inp".into(),
                template: "func={{param:functional}}\nbasis={{param:basis}}\nsmiles={{slot:substrate_smiles}}\n".into(),
            }],
            sanity_gates: vec![
                NamedMetricRule {
                    rule_id: "scf_converged".into(),
                    rule: MetricRule::EqualsBool {
                        metric: "scf_converged".into(),
                        expected: true,
                    },
                },
                NamedMetricRule {
                    rule_id: "ts_single_imag_mode".into(),
                    rule: MetricRule::EqualsInt {
                        metric: "imaginary_frequencies".into(),
                        expected: 1,
                    },
                },
                NamedMetricRule {
                    rule_id: "irc_connected".into(),
                    rule: MetricRule::EqualsBool {
                        metric: "irc_endpoints_connected".into(),
                        expected: true,
                    },
                },
            ],
            acceptance_criteria: vec![NamedMetricRule {
                rule_id: "barrier_threshold".into(),
                rule: MetricRule::MaxFloat {
                    metric: "barrier_kcal_mol".into(),
                    max: 18.0,
                },
            }],
            orthogonal_policy: OrthogonalPolicy::default(),
            control_policy: BlindedControlPolicy::default(),
            preregistration_required: true,
        }
    }

    fn diag_values(scf: bool, imag: i64, irc: bool, barrier: f64) -> ParsedDiagnostics {
        ParsedDiagnostics {
            metrics: BTreeMap::from([
                ("scf_converged".into(), DiagnosticValue::Bool(scf)),
                ("imaginary_frequencies".into(), DiagnosticValue::Int(imag)),
                ("irc_endpoints_connected".into(), DiagnosticValue::Bool(irc)),
                ("barrier_kcal_mol".into(), DiagnosticValue::Float(barrier)),
            ]),
        }
    }

    fn commit() -> String {
        "a".repeat(40)
    }

    #[test]
    fn broker_blocks_surrogate_epistemic_laundering_before_execution() -> Result<()> {
        let broker = Broker::new();
        let protocol = base_protocol(ScientificTrust::E4a);
        let sub = CandidateSubmission {
            version: 1,
            protocol_sha256: protocol.protocol_sha256()?,
            candidate_id: "cand_1".into(),
            slot_values: BTreeMap::from([(
                "substrate_smiles".into(),
                SlotValue::Text("C=CC".into()),
            )]),
        };
        let surrogate = MockCompChemRunner {
            backend: "ml_surrogate",
            sha: "1".repeat(64),
            ceiling: ScientificTrust::E1,
            ref_ceiling: Some(ScientificTrust::E2a),
            debts: &["surrogate_only"],
            evaluator: Box::new(|_| diag_values(true, 1, true, 12.0)),
        };
        let err = broker
            .verify_protocol(ProtocolVerificationRequest {
                base_commit: &commit(),
                protocol: &protocol,
                submission: &sub,
                blinded_controls: None,
                runners: &[&surrogate],
                lane: ProtocolLane::Heavy,
            })
            .unwrap_err();
        assert!(err.to_string().contains("anti-laundering violation"));
        Ok(())
    }

    #[test]
    fn broker_rejects_exit_zero_unphysical_ts_via_sanity_gate() -> Result<()> {
        let broker = Broker::new();
        let protocol = base_protocol(ScientificTrust::E4a);
        let sub = CandidateSubmission {
            version: 1,
            protocol_sha256: protocol.protocol_sha256()?,
            candidate_id: "bad_ts_2_imag".into(),
            slot_values: BTreeMap::from([(
                "substrate_smiles".into(),
                SlotValue::Text("C=CC".into()),
            )]),
        };
        // Exit 0 and low barrier (10.0 kcal/mol), but 2 imaginary modes -> MUST FAIL sanity gate!
        let runner = MockCompChemRunner {
            backend: "orca_dft",
            sha: "2".repeat(64),
            ceiling: ScientificTrust::E4a,
            ref_ceiling: Some(ScientificTrust::E2a),
            debts: &["single_functional_dft"],
            evaluator: Box::new(|_| diag_values(true, 2, true, 10.0)),
        };
        let receipt = broker.verify_protocol(ProtocolVerificationRequest {
            base_commit: &commit(),
            protocol: &protocol,
            submission: &sub,
            blinded_controls: None,
            runners: &[&runner],
            lane: ProtocolLane::Heavy,
        })?;
        assert_eq!(receipt.verdict(), ProtocolVerdict::SanityGateFailed);
        assert_eq!(receipt.epistemic_level_typed(), ScientificTrust::E0);
        assert!(
            receipt
                .failure_reasons()
                .iter()
                .any(|r| r.contains("ts_single_imag_mode"))
        );
        Ok(())
    }

    #[test]
    fn broker_withholds_passing_candidate_when_blinded_control_drifts_and_hides_control_data()
    -> Result<()> {
        let broker = Broker::new();
        let mut protocol = base_protocol(ScientificTrust::E4a);
        let controls = BlindedControlPack {
            version: 1,
            protocol_id: protocol.protocol_id.clone(),
            positive_controls: vec![BlindedControlCase {
                control_id: "secret_pos_ctrl".into(),
                slot_values: BTreeMap::from([(
                    "substrate_smiles".into(),
                    SlotValue::Text("CC=CC".into()),
                )]),
            }],
            negative_controls: vec![BlindedControlCase {
                control_id: "secret_neg_decoy".into(),
                slot_values: BTreeMap::from([(
                    "substrate_smiles".into(),
                    SlotValue::Text("CCCCCC".into()),
                )]),
            }],
        };
        protocol.control_policy = BlindedControlPolicy {
            control_pack_sha256: Some(controls.fingerprint()?),
            min_positive_controls: 1,
            min_negative_controls: 1,
        };
        let sub = CandidateSubmission {
            version: 1,
            protocol_sha256: protocol.protocol_sha256()?,
            candidate_id: "cand_good".into(),
            slot_values: BTreeMap::from([(
                "substrate_smiles".into(),
                SlotValue::Text("C=CC".into()),
            )]),
        };

        // Broken assay: falsely gives the negative decoy a passing barrier (11.0 <= 18.0)!
        let drifting_runner = MockCompChemRunner {
            backend: "orca_dft",
            sha: "3".repeat(64),
            ceiling: ScientificTrust::E4a,
            ref_ceiling: Some(ScientificTrust::E2a),
            debts: &["single_functional_dft"],
            evaluator: Box::new(|_| diag_values(true, 1, true, 11.0)),
        };
        let drifted = broker.verify_protocol(ProtocolVerificationRequest {
            base_commit: &commit(),
            protocol: &protocol,
            submission: &sub,
            blinded_controls: Some(&controls),
            runners: &[&drifting_runner],
            lane: ProtocolLane::Heavy,
        })?;
        assert_eq!(drifted.verdict(), ProtocolVerdict::ControlDriftDetected);
        assert_eq!(drifted.epistemic_level_typed(), ScientificTrust::E0);
        assert!(!drifted.control_summary().all_controls_passed);
        assert_eq!(drifted.control_summary().negative_properly_rejected, 0);

        // Receipt JSON MUST NOT leak hidden control IDs or hidden SMILES.
        let json_text = drifted.raw_json()?.to_string();
        assert!(!json_text.contains("secret_pos_ctrl"));
        assert!(!json_text.contains("secret_neg_decoy"));
        assert!(!json_text.contains("CCCCCC"));

        // Healthy assay separates positive (12.0) vs negative decoy (34.0) -> candidate passes!
        let healthy_runner = MockCompChemRunner {
            backend: "orca_dft",
            sha: "4".repeat(64),
            ceiling: ScientificTrust::E4a,
            ref_ceiling: Some(ScientificTrust::E2a),
            debts: &["single_functional_dft"],
            evaluator: Box::new(|run| {
                let job = run.artifacts().get("job.inp").unwrap();
                if job.contains("smiles=CCCCCC") {
                    diag_values(true, 1, true, 34.0)
                } else {
                    diag_values(true, 1, true, 12.5)
                }
            }),
        };
        let passed = broker.verify_protocol(ProtocolVerificationRequest {
            base_commit: &commit(),
            protocol: &protocol,
            submission: &sub,
            blinded_controls: Some(&controls),
            runners: &[&healthy_runner],
            lane: ProtocolLane::Heavy,
        })?;
        assert_eq!(passed.verdict(), ProtocolVerdict::Passed);
        assert_eq!(passed.epistemic_level_typed(), ScientificTrust::E4a);
        assert!(passed.control_summary().all_controls_passed);
        assert_eq!(
            passed.epistemic_debts(),
            &["implicit_solvent", "single_functional_dft"]
        );
        Ok(())
    }

    #[test]
    fn broker_enforces_orthogonal_consensus_and_flags_disagreement_for_e4b() -> Result<()> {
        let broker = Broker::new();
        let mut protocol = base_protocol(ScientificTrust::E4b);
        protocol.orthogonal_policy = OrthogonalPolicy {
            min_verifiers: 2,
            require_distinct_backends: true,
            metric_tolerances: BTreeMap::from([("barrier_kcal_mol".into(), 1.5)]),
        };
        let sub = CandidateSubmission {
            version: 1,
            protocol_sha256: protocol.protocol_sha256()?,
            candidate_id: "cand_ortho".into(),
            slot_values: BTreeMap::from([(
                "substrate_smiles".into(),
                SlotValue::Text("C=CC".into()),
            )]),
        };
        let dft = MockCompChemRunner {
            backend: "orca_wb97xd",
            sha: "5".repeat(64),
            ceiling: ScientificTrust::E4b,
            ref_ceiling: Some(ScientificTrust::E2b),
            debts: &[],
            evaluator: Box::new(|_| diag_values(true, 1, true, 13.0)),
        };
        let cc_disagree = MockCompChemRunner {
            backend: "dlpno_ccsdt",
            sha: "6".repeat(64),
            ceiling: ScientificTrust::E4b,
            ref_ceiling: Some(ScientificTrust::E2b),
            debts: &[],
            evaluator: Box::new(|_| diag_values(true, 1, true, 16.5)),
        };
        let conflict = broker.verify_protocol(ProtocolVerificationRequest {
            base_commit: &commit(),
            protocol: &protocol,
            submission: &sub,
            blinded_controls: None,
            runners: &[&dft, &cc_disagree],
            lane: ProtocolLane::Heavy,
        })?;
        assert_eq!(conflict.verdict(), ProtocolVerdict::Disagreement);
        assert_eq!(conflict.epistemic_level_typed(), ScientificTrust::E0);

        let cc_agree = MockCompChemRunner {
            backend: "dlpno_ccsdt",
            sha: "7".repeat(64),
            ceiling: ScientificTrust::E4b,
            ref_ceiling: Some(ScientificTrust::E2b),
            debts: &[],
            evaluator: Box::new(|_| diag_values(true, 1, true, 13.8)),
        };
        let consensus = broker.verify_protocol(ProtocolVerificationRequest {
            base_commit: &commit(),
            protocol: &protocol,
            submission: &sub,
            blinded_controls: None,
            runners: &[&dft, &cc_agree],
            lane: ProtocolLane::Heavy,
        })?;
        assert_eq!(consensus.verdict(), ProtocolVerdict::Passed);
        assert_eq!(consensus.epistemic_level_typed(), ScientificTrust::E4b);
        assert_eq!(consensus.verifier_reports().len(), 2);
        Ok(())
    }
}
