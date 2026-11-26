//! Built-in Interactive Theorem Prover (ITP) Domain Pack wrapping the
//! existing Lean 4, Agda, Rocq, SMT, ATP, CAS, and Python admission logic.
//!
//! No existing ITP semantics, wire spellings, or admission rules are modified.

use crate::{
    BackendDescriptor, DispatchEngine, DomainPack, Engine, FrozenTarget, PreFlightScanResult,
    PreFlightViolation, ProofBodyError, ProofBodySubmission, Trust, render_proof_body, scan_source,
    validate_source_path,
};

const ITP_BACKENDS: &[BackendDescriptor<Trust>] = &[
    BackendDescriptor {
        backend_id: "lean4",
        max_positive_ceiling: Trust::L5,
        max_refutation_ceiling: Some(Trust::L2b),
        default_debts: &[],
        requires_orthogonal_pair: false,
    },
    BackendDescriptor {
        backend_id: "agda",
        max_positive_ceiling: Trust::L5,
        max_refutation_ceiling: Some(Trust::L2b),
        default_debts: &[],
        requires_orthogonal_pair: false,
    },
    BackendDescriptor {
        backend_id: "rocq",
        max_positive_ceiling: Trust::L5,
        max_refutation_ceiling: Some(Trust::L2b),
        default_debts: &[],
        requires_orthogonal_pair: false,
    },
    BackendDescriptor {
        backend_id: "smt",
        max_positive_ceiling: Trust::L4a,
        max_refutation_ceiling: Some(Trust::L2a),
        default_debts: &["oracle_unreconstructed"],
        requires_orthogonal_pair: true,
    },
    BackendDescriptor {
        backend_id: "atp",
        max_positive_ceiling: Trust::L4a,
        max_refutation_ceiling: Some(Trust::L2a),
        default_debts: &["oracle_unreconstructed"],
        requires_orthogonal_pair: true,
    },
    BackendDescriptor {
        backend_id: "cas",
        max_positive_ceiling: Trust::L4a,
        max_refutation_ceiling: Some(Trust::L2a),
        default_debts: &["oracle_unreconstructed", "cas_symbolic_assumptions"],
        requires_orthogonal_pair: true,
    },
    BackendDescriptor {
        backend_id: "python",
        max_positive_ceiling: Trust::L1,
        max_refutation_ceiling: Some(Trust::L2a),
        default_debts: &["finite_probe_only"],
        requires_orthogonal_pair: false,
    },
];

/// First-party Domain Pack for formal mathematics / proof assistants.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ItpDomainPack;

impl ItpDomainPack {
    pub const DOMAIN_ID: &'static str = "itp";

    pub const fn new() -> Self {
        Self
    }

    pub const fn knowledge_profile(&self) -> &'static crate::DomainKnowledgeProfile {
        &crate::ITP_KNOWLEDGE_PROFILE
    }

    pub fn validate_source_path(
        &self,
        engine: Engine,
        file_path: &str,
    ) -> Result<(), PreFlightViolation> {
        validate_source_path(engine, file_path)
    }

    pub fn scan_source(
        &self,
        engine: Engine,
        file_path: &str,
        source: &str,
    ) -> PreFlightScanResult {
        scan_source(engine, file_path, source)
    }

    pub fn render_proof_body(
        &self,
        target: &FrozenTarget,
        submission: &ProofBodySubmission,
    ) -> Result<String, ProofBodyError> {
        render_proof_body(target, submission)
    }
}

impl DomainPack for ItpDomainPack {
    type Level = Trust;
    type Backend = DispatchEngine;

    fn domain_id(&self) -> &'static str {
        Self::DOMAIN_ID
    }

    fn display_name(&self) -> &'static str {
        "Interactive Theorem Provers (Lean 4 / Agda / Rocq)"
    }

    fn backends(&self) -> &'static [BackendDescriptor<Self::Level>] {
        ITP_BACKENDS
    }

    fn descriptor(&self, backend: Self::Backend) -> &'static BackendDescriptor<Self::Level> {
        match backend {
            DispatchEngine::Lean4 => &ITP_BACKENDS[0],
            DispatchEngine::Agda => &ITP_BACKENDS[1],
            DispatchEngine::Rocq => &ITP_BACKENDS[2],
            DispatchEngine::Smt => &ITP_BACKENDS[3],
            DispatchEngine::Atp => &ITP_BACKENDS[4],
            DispatchEngine::Cas => &ITP_BACKENDS[5],
            DispatchEngine::Python => &ITP_BACKENDS[6],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn itp_pack_enforces_kernel_vs_oracle_ceilings() {
        let pack = ItpDomainPack::new();
        assert_eq!(pack.domain_id(), "itp");
        assert_eq!(pack.backend_ceiling(DispatchEngine::Lean4), Trust::L5);
        assert_eq!(pack.backend_ceiling(DispatchEngine::Smt), Trust::L4a);
        assert_eq!(pack.backend_ceiling(DispatchEngine::Python), Trust::L1);

        assert!(
            pack.validate_backend_ceiling(DispatchEngine::Lean4, Trust::L5)
                .is_ok()
        );
        assert!(
            pack.validate_backend_ceiling(DispatchEngine::Lean4, Trust::L2b)
                .is_ok()
        );
        assert!(
            pack.validate_backend_ceiling(DispatchEngine::Smt, Trust::L4a)
                .is_ok()
        );
        assert!(
            pack.validate_backend_ceiling(DispatchEngine::Smt, Trust::L2a)
                .is_ok()
        );
        // SMT/CAS/Python cannot mint L5 or L2b kernel refutation.
        assert!(
            pack.validate_backend_ceiling(DispatchEngine::Smt, Trust::L5)
                .is_err()
        );
        assert!(
            pack.validate_backend_ceiling(DispatchEngine::Smt, Trust::L2b)
                .is_err()
        );
        assert!(
            pack.validate_backend_ceiling(DispatchEngine::Cas, Trust::L5)
                .is_err()
        );
        assert!(
            pack.validate_backend_ceiling(DispatchEngine::Python, Trust::L3)
                .is_err()
        );
        assert!(
            pack.validate_backend_ceiling(DispatchEngine::Python, Trust::L2b)
                .is_err()
        );
    }

    #[test]
    fn itp_pack_delegates_admission_unchanged() {
        let pack = ItpDomainPack::new();
        assert!(
            pack.validate_source_path(Engine::Lean4, "Demo.lean")
                .is_ok()
        );
        assert!(
            pack.validate_source_path(Engine::Lean4, "lakefile.lean")
                .is_err()
        );
        let scan = pack.scan_source(
            Engine::Lean4,
            "Demo.lean",
            "theorem t : True := True.intro\n",
        );
        assert!(scan.passed);
    }
}
