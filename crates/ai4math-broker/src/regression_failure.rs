//! Sealed evidence of a narrowly attributable native downstream regression.
use crate::{ReceiptTier, VerifiedCandidate, sandbox::ChildExit, t3};
use ai4math_core::Engine;

/// This can only come from a native Lean T1 candidate's tracked regression,
/// after the candidate itself verified. It is not a receipt or trust evidence.
#[derive(Debug)]
pub struct RegressionFailure {
    base: String,
    target_module: String,
    target_decl: String,
    failed_module: String,
    candidate_sha256: String,
    compiler_sha256: String,
    original: anyhow::Error,
}

impl RegressionFailure {
    pub fn base_commit(&self) -> &str {
        &self.base
    }
    pub fn engine(&self) -> Engine {
        Engine::Lean4
    }
    pub fn target_module(&self) -> &str {
        &self.target_module
    }
    pub fn target_decl(&self) -> &str {
        &self.target_decl
    }
    pub fn failed_module(&self) -> &str {
        &self.failed_module
    }
    pub fn candidate_sha256(&self) -> &str {
        &self.candidate_sha256
    }
    pub fn compiler_sha256(&self) -> &str {
        &self.compiler_sha256
    }
}

impl std::fmt::Display for RegressionFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "native Lean downstream regression rejected {}",
            self.failed_module
        )
    }
}
impl std::error::Error for RegressionFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.original.as_ref())
    }
}

pub(super) fn classify(
    original: anyhow::Error,
    candidate: &VerifiedCandidate,
    failed_module: &str,
    source: &str,
) -> anyhow::Error {
    let receipt = candidate.receipt();
    let known = receipt.engine == Engine::Lean4
        && receipt.tier == ReceiptTier::T1Promotion
        && original
            .downcast_ref::<ChildExit>()
            .is_some_and(|exit| t3::is_downstream_regression(exit, failed_module, source));
    if !known {
        return original;
    }
    RegressionFailure {
        base: receipt.base_commit.clone(),
        target_module: receipt.file_path.clone(),
        target_decl: receipt.target_id.clone(),
        failed_module: failed_module.into(),
        candidate_sha256: receipt.patch_sha256.clone(),
        compiler_sha256: receipt.compiler_sha256.clone(),
        original,
    }
    .into()
}
