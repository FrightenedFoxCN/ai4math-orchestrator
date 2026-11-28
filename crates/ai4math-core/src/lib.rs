//! Typed orchestration data and static admission for phase one.

pub mod belief;
pub mod compute;
pub mod contracts;
pub mod domain;
pub mod knowledge;
pub mod pack_itp;
pub mod preflight;
pub mod proof;
pub mod trust;

pub use belief::{
    AttenuationRule, BeliefConflictEdge, BeliefError, BeliefGraph, BeliefNodeInput,
    BeliefPlanInput, BeliefPropagationConfig, BeliefPropagationReport, BeliefPropagator,
    ConflictEdgeKind, NodeBeliefState, Opinion, SubjectiveLogicPropagator, TagAttenuationPolicy,
    VoIRanking,
};
pub use compute::{
    ComputeJobStatus, ComputePoolSnapshot, ExecutionTargetKind, GpuDeviceSnapshot, GpuKind,
    GpuRequest, ResourcePolicyError, ResourceRequest, SlurmClusterPolicy,
};
pub use contracts::*;
pub use domain::{
    BackendDescriptor, DomainPack, EpistemicAssessment, EpistemicCeilingViolation, EpistemicDebt,
    EpistemicLevel, ScientificTrust, aggregate_assessment_plan, aggregate_epistemic_plan,
    best_assessment_plan, best_epistemic_plan, compare_epistemic,
    validate_backend_descriptor_ceiling,
};
pub use knowledge::{
    CitationStatus, DomainKnowledgeProfile, EpistemicTag, ExtractionStance, FacetDescriptor,
    ITP_KNOWLEDGE_FACETS, ITP_KNOWLEDGE_PROFILE, InvalidEpistemicTag, KnowledgeEdgeKind,
    KnowledgeSourceKind, KnowledgeStratum, LITERATURE_COMMON_FACETS, ReproducibilityClass,
};
pub use pack_itp::ItpDomainPack;
pub use preflight::{
    PreFlightScanResult, PreFlightSeverity, PreFlightViolation, scan_source, validate_source_path,
};
pub use proof::{
    FrozenTarget, ProofBodyError, ProofBodySubmission, render_proof_body,
    render_proof_body_allow_oracle, render_proof_body_allow_sorry,
};
pub use trust::{Trust, aggregate_plan, best_proof_plan};

/// Error returned when a string is not an enum's wire spelling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseEnumError {
    pub kind: &'static str,
    pub value: String,
}

impl std::fmt::Display for ParseEnumError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "unknown {}: {:?}", self.kind, self.value)
    }
}
impl std::error::Error for ParseEnumError {}

macro_rules! wire_enum {
    ($(#[$attr:meta])* $name:ident { $($variant:ident => $wire:literal),+ $(,)? }) => {
        $(#[$attr])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
        pub enum $name { $(#[serde(rename = $wire)] $variant),+ }
        impl $name {
            pub const fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $wire),+ }
            }
        }
        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }
        impl std::str::FromStr for $name {
            type Err = $crate::ParseEnumError;
            fn from_str(value: &str) -> Result<Self, Self::Err> {
                match value {
                    $($wire => Ok(Self::$variant),)+
                    _ => Err($crate::ParseEnumError { kind: stringify!($name), value: value.to_owned() }),
                }
            }
        }
    };
}
pub(crate) use wire_enum;
