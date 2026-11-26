//! IO-free evidence contracts shared by the native broker and durable store.
//!
//! The sealed concrete evidence types remain in `ai4math-broker`. This crate
//! exposes only read-only ports. Implementing a port is unsafe because a false
//! implementation can fabricate proof, refutation, or admission evidence.

mod oracle;
pub mod protocol;
mod sealed;

use ai4math_core::Engine;
use anyhow::Result;
pub use oracle::{
    ExactCheck, MAX_ORACLE_QUERY_BYTES, OracleOutcome, OracleQuery, PreparedOracleQuery,
};
pub use protocol::{
    ArtifactTemplate, BlindedControlCase, BlindedControlPack, BlindedControlPolicy,
    BlindedControlSummary, CandidateSubmission, CompiledControlSet, CompiledProtocolRun,
    CorrectionMethod, DiagnosticValue, GateFailure, MAX_CONTROL_PACK_BYTES, MAX_PREREG_BYTES,
    MAX_PROTOCOL_SPEC_BYTES, MAX_RENDERED_PROTOCOL_BYTES, MAX_SUBMISSION_BYTES, MetricRule,
    NamedMetricRule, OrthogonalPolicy, ParamValue, ParsedDiagnostics, PreRegistrationSpec,
    ProtocolSmellPolicy, ProtocolSmellSignal, ProtocolSpec, ProtocolVerdict, SlotKind, SlotSpec,
    SlotValue, TextProfile, TrialAuditSummary, TrialSample, VerifierReport, compute_trial_audit,
    evaluate_acceptance_criteria, evaluate_orthogonal_agreement, evaluate_sanity_gates,
};
pub use sealed::{ProtocolRunEvidence, SealedReceipt};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

/// Verification tier stored in the durable receipt wire format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ReceiptTier {
    T1Promotion,
    T3TaintRebuild,
}

impl ReceiptTier {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::T1Promotion => "T1_PROMOTION",
            Self::T3TaintRebuild => "T3_TAINT_REBUILD",
        }
    }
}

/// Read-only view of a broker-sealed verification receipt.
///
/// # Safety
///
/// Implement only for a receipt constructed by the native broker after all
/// required verification checks succeed. A false implementation can fabricate
/// admission evidence and must never be derived from JSON or caller input.
#[allow(clippy::len_without_is_empty)]
pub unsafe trait ReceiptEvidence {
    fn tier(&self) -> ReceiptTier;
    fn issued_unix_seconds(&self) -> u64;
    fn compile_seconds(&self) -> f64;
    fn engine(&self) -> Engine;
    fn target_id(&self) -> &str;
    fn file_path(&self) -> &str;
    fn base_commit(&self) -> &str;
    fn patch_sha256(&self) -> &str;
    fn dependencies_sha256(&self) -> &str;
    fn compiler_sha256(&self) -> &str;
    fn observed_axioms(&self) -> &[String];
    fn receipt_sha256(&self) -> &str;
    fn matches(&self, base_commit: &str, file_path: &str, candidate_bytes: &[u8]) -> bool;
    fn raw_json(&self) -> Result<Value>;
}

/// Read-only view of a natively verified candidate and its private T3 mode.
///
/// # Safety
///
/// Implement only for candidates returned by the native broker. The source,
/// receipt binding, and assessed-T3 flag must reflect the actual verification
/// path; caller-supplied text or serialized receipts are not valid evidence.
pub unsafe trait CandidateEvidence {
    fn source(&self) -> &str;
    fn receipt(&self) -> &dyn ReceiptEvidence;
    fn is_assessed_t3(&self) -> bool;
}

/// Confirms that a fresh native regression checked the exact candidate.
///
/// # Safety
///
/// Implement only for regression artifacts produced by the native broker for
/// the exact candidate. Returning true for unrelated or absent checks can
/// authorize an unverified promotion.
pub unsafe trait RegressionEvidence {
    fn matches_candidate(&self, candidate: &dyn CandidateEvidence) -> bool;
}

/// Read-only view of a sealed Lean specification comparison.
///
/// # Safety
///
/// Implement only for comparisons emitted by the native Lean comparator after
/// kernel replay and definitional-equality checks. Every candidate and spec
/// binding must describe the exact verified declarations.
pub unsafe trait LeanComparisonEvidence {
    fn spec_file(&self) -> &str;
    fn spec_decl(&self) -> &str;
    fn spec_prefix_sha256(&self) -> &str;
    fn base_commit(&self) -> &str;
    fn candidate_file(&self) -> &str;
    fn candidate_decl(&self) -> &str;
    fn candidate_source_sha256(&self) -> &str;
    fn candidate_receipt_sha256(&self) -> &str;
    fn receipt_sha256(&self) -> &str;
}

/// Read-only view of a sealed native refutation witness.
///
/// # Safety
///
/// Implement only for witnesses constructed by the native broker after the
/// sealed refutation theorem and axiom checks pass. Fabricated witnesses can
/// create false hard-refutation events and P0 alerts.
pub unsafe trait WitnessEvidence {
    fn base_commit(&self) -> &str;
    fn file_path(&self) -> &str;
    fn proposition_id(&self) -> &str;
    fn proposition_name(&self) -> &str {
        self.proposition_id()
    }
    fn source_sha256(&self) -> &str;
    fn proof_body_sha256(&self) -> &str;
    fn witness_sha256(&self) -> &str;
    fn source(&self) -> &str;
    fn matches(
        &self,
        base_commit: &str,
        file_path: &str,
        proposition_id: &str,
        committed_source: &[u8],
        proof_body: &[u8],
    ) -> bool;
    fn receipt_json(&self) -> Result<String>;
}

/// Read-only view of a sealed bounded Oracle observation.
///
/// # Safety
///
/// Implement only for observations constructed by the broker after the exact
/// finite-domain check and both solver runs. Oracle evidence is advisory, but a
/// false implementation can still corrupt review and retention records.
pub unsafe trait OracleObservationEvidence {
    fn id(&self) -> &str;
    fn query_sha256(&self) -> &str;
    fn solver_sha256s(&self) -> [&str; 2];
    fn outcome(&self) -> OracleOutcome;
    fn raw_json(&self) -> Result<Value>;
}

/// Read-only view of sealed breaker probe evidence.
///
/// # Safety
///
/// Implement only for probe evidence constructed by the native broker for the
/// exact approved commit, definition, cases, and ordered source set. A false
/// implementation can close a breaker without successful kernel probes.
pub unsafe trait BreakerProbeEvidence {
    fn base_commit(&self) -> &str;
    fn file_path(&self) -> &str;
    fn definition(&self) -> &str;
    fn cases(&self) -> &[String];
    fn sources_sha256(&self) -> &str;
    fn receipt_sha256(&self) -> &str;
}

/// Read-only view of sealed infrastructure smoke evidence.
///
/// # Safety
///
/// Implement only for smoke evidence constructed by the native broker after
/// the exact infrastructure module, definitions, and source slice pass. False
/// evidence can freeze an InfraRFC that was never successfully checked.
pub unsafe trait InfraSmokeEvidence {
    fn commit(&self) -> &str;
    fn module_path(&self) -> &str;
    fn definitions(&self) -> &[String];
    fn receipt_sha256(&self) -> &str;
}

/// Read-only view of sealed infrastructure completion evidence.
///
/// # Safety
///
/// Implement only for completion evidence constructed by the native broker
/// after the exact module, definitions, API lemmas, and source slice pass.
/// False evidence can mark an InfraRFC complete without native verification.
pub unsafe trait InfraCompletionEvidence {
    fn commit(&self) -> &str;
    fn module_path(&self) -> &str;
    fn definitions(&self) -> &[String];
    fn api_lemmas(&self) -> &[String];
    fn receipt_sha256(&self) -> &str;
}

/// Read-only view of a narrowly classified native downstream regression.
///
/// # Safety
///
/// Implement only for failures classified by the broker after the candidate
/// itself verified and a tracked downstream module failed. False evidence can
/// manufacture regression-meltdown alerts or attribute failure to the wrong
/// candidate and compiler.
pub unsafe trait RegressionFailureEvidence {
    fn base_commit(&self) -> &str;
    fn engine(&self) -> Engine;
    fn target_module(&self) -> &str;
    fn target_decl(&self) -> &str;
    fn failed_module(&self) -> &str;
    fn candidate_sha256(&self) -> &str;
    fn compiler_sha256(&self) -> &str;
}

/// Exact ordered source-set digest used by receipts, comparisons and probes.
pub fn ordered_source_set_sha256(modules: &[(&str, &str)]) -> String {
    let mut hash = Sha256::new();
    hash.update(b"ai4math:ordered-source-dependencies:v1");
    hash.update((modules.len() as u64).to_be_bytes());
    for &(path, source) in modules {
        hash.update((path.len() as u64).to_be_bytes());
        hash.update(path.as_bytes());
        hash.update((source.len() as u64).to_be_bytes());
        hash.update(source.as_bytes());
    }
    format!("{:x}", hash.finalize())
}

/// Serialize a sealed evidence view without changing its concrete wire shape.
pub fn evidence_json<T: Serialize + ?Sized>(value: &T) -> Result<Value> {
    Ok(serde_json::to_value(value)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordered_source_hash_keeps_length_delimiters_and_order() {
        let empty = ordered_source_set_sha256(&[]);
        assert_eq!(
            empty,
            "53d6c8644575b13a8814f4cd42c27f935ea28d030d4da4cebbb9eb80d4b060c0"
        );
        assert_ne!(
            ordered_source_set_sha256(&[("A.lean", "x"), ("B.lean", "y")]),
            ordered_source_set_sha256(&[("B.lean", "y"), ("A.lean", "x")])
        );
        assert_ne!(
            ordered_source_set_sha256(&[("A.lean", "xy")]),
            ordered_source_set_sha256(&[("A.lean", "x"), ("B.lean", "y")])
        );
    }

    #[test]
    fn oracle_query_wire_shape_is_stable() {
        let bytes = br#"{"version":1,"variables":[{"name":"x","lower":-2,"upper":2}],"premises":[{"terms":[{"var":"x","coeff":1}],"relation":"ge","constant":0}],"conclusion":{"terms":[{"var":"x","coeff":-1}],"relation":"lt","constant":0}}"#;
        let query = OracleQuery::parse(bytes).unwrap();
        assert_eq!(
            serde_json::to_string(&query).unwrap(),
            std::str::from_utf8(bytes).unwrap()
        );
        assert_eq!(query.fingerprint().unwrap().len(), 64);
    }
}
