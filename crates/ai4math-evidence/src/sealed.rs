//! Domain-agnostic sealed evidence ports.
//!
//! Implementing these traits is `unsafe` for the same reason as the ITP-specific
//! ports in the crate root: only the trusted native broker may construct sealed
//! evidence after executing all required checks. Caller-supplied JSON must never
//! implement these traits.

use crate::protocol::{BlindedControlSummary, ParsedDiagnostics, ProtocolVerdict, VerifierReport};
use anyhow::Result;
use serde_json::Value;

/// Universal read-only view of any broker-sealed verification receipt across
/// all domains (ITP proof assistants, computational chemistry, computational
/// biology, or other scientific verification backends).
///
/// # Safety
///
/// Implement only for receipts minted by the trusted broker after executing its
/// clean-room verification pipeline. Fabricated implementations can bypass
/// admission gates and corrupt the durable ledger.
pub unsafe trait SealedReceipt {
    /// Domain identifier (e.g. `"itp"`, `"compchem"`, `"compbio"`).
    fn domain_id(&self) -> &str;
    /// Verifier backend identifier (e.g. `"lean4"`, `"xtb_gfn2"`, `"openmm_fep"`).
    fn backend_id(&self) -> &str;
    /// Logical target or candidate identifier bound to this receipt.
    fn target_id(&self) -> &str;
    /// Reviewed Git commit or immutable workspace base bound to this receipt.
    fn base_commit(&self) -> &str;
    /// Cryptographic digest of the frozen specification / protocol / template.
    fn spec_sha256(&self) -> &str;
    /// Cryptographic digest of the untrusted candidate artifact / slot submission.
    fn candidate_sha256(&self) -> &str;
    /// Cryptographic digest of pinned dependency / input artifacts.
    fn inputs_sha256(&self) -> &str;
    /// Cryptographic digest of the operator-trusted verifier toolchain / evaluator.
    fn verifier_sha256(&self) -> &str;
    /// Epistemic level wire spelling enforced by the broker (e.g. `"L5"`, `"E4a"`).
    fn epistemic_level(&self) -> &str;
    /// Mandatory unresolved epistemic/model assumption debts attached by the broker.
    fn epistemic_debts(&self) -> &[String];
    fn issued_unix_seconds(&self) -> u64;
    fn elapsed_seconds(&self) -> f64;
    /// Canonical receipt digest uniquely identifying this sealed evidence record.
    fn receipt_sha256(&self) -> &str;
    fn raw_json(&self) -> Result<Value>;
}

/// Read-only view of a broker-sealed protocol verification run carrying sanity
/// gate outcomes, blinded control separation summaries, orthogonal verifier
/// agreement reports, and anti-laundering epistemic ceilings.
///
/// # Safety
///
/// Implement only for values constructed by the native broker after compiling
/// the candidate against the frozen `ProtocolSpec`, running any required
/// `BlindedControlPack`, evaluating deterministic sanity gates and orthogonal
/// verifier agreement, and capping the achieved epistemic level.
pub unsafe trait ProtocolRunEvidence: SealedReceipt {
    fn protocol_id(&self) -> &str;
    fn protocol_sha256(&self) -> &str;
    fn candidate_id(&self) -> &str;
    fn rendered_artifacts_sha256(&self) -> &str;
    fn verdict(&self) -> ProtocolVerdict;
    fn control_summary(&self) -> &BlindedControlSummary;
    fn verifier_reports(&self) -> &[VerifierReport];
    fn aggregated_diagnostics(&self) -> &ParsedDiagnostics;
    fn failure_reasons(&self) -> &[String];
}
