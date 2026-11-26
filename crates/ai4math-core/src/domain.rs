//! Domain-agnostic epistemic ordering, debt propagation, and Domain Pack traits.
//!
//! This module factors out the orchestration algebra that is independent of
//! interactive theorem provers (Lean 4 / Agda / Rocq) so non-ITP domains such
//! as computational chemistry and computational biology can plug into the same
//! AND-OR claim graph, debt propagation, and backend epistemic ceilings.

use crate::wire_enum;
use serde::{Deserialize, Serialize};
use std::{cmp::Ordering, collections::BTreeSet};

/// Partial-order evidence lattice separating positive corroboration from
/// falsification/refutation branches.
pub trait EpistemicLevel: Copy + Eq + PartialOrd + Sized {
    /// Rank on the positive verification/corroboration ladder (`None` for refutations).
    fn positive_rank(self) -> Option<u8>;

    /// Rank on the refutation/falsification ladder (`None` for non-refutations).
    fn refutation_rank(self) -> Option<u8>;

    /// Whether this level represents a hard, archive-grade refutation (`L2b` / `E2b`).
    fn is_hard_refutation(self) -> bool;

    /// Whether positive reliance on this level implies unresolved assumption/oracle debt.
    fn implies_debt(self) -> bool;

    /// Canonical wire spelling (e.g. `"L5"`, `"E4a"`).
    fn wire_name(self) -> &'static str;
}

/// Canonical partial order for any two-branch epistemic lattice:
/// - Positive ranks compare numerically.
/// - Refutation ranks compare numerically.
/// - Any refutation dominates unformalized preliminary states (positive rank <= 1).
/// - Formalized/verified states (positive rank >= 2) are incomparable with refutations.
pub fn compare_epistemic<E: EpistemicLevel>(lhs: E, rhs: E) -> Option<Ordering> {
    match (lhs.positive_rank(), rhs.positive_rank()) {
        (Some(a), Some(b)) => Some(a.cmp(&b)),
        (None, None) => match (lhs.refutation_rank(), rhs.refutation_rank()) {
            (Some(a), Some(b)) => Some(a.cmp(&b)),
            _ if lhs == rhs => Some(Ordering::Equal),
            _ => None,
        },
        (Some(rank), None) if rank <= 1 => Some(Ordering::Less),
        (None, Some(rank)) if rank <= 1 => Some(Ordering::Greater),
        _ => None,
    }
}

/// AND aggregation: a plan's effective level is capped by the weakest positive
/// rank among `local` and all `dependencies`. Any refutation disqualifies the
/// plan from counting as positive support and returns `None`.
pub fn aggregate_epistemic_plan<E: EpistemicLevel>(
    local: E,
    dependencies: impl IntoIterator<Item = E>,
) -> Option<E> {
    let mut best = local;
    let mut rank = best.positive_rank()?;
    for dependency in dependencies {
        let next = dependency.positive_rank()?;
        if next < rank {
            best = dependency;
            rank = next;
        }
    }
    Some(best)
}

/// OR aggregation: select the highest positive epistemic level among surviving
/// plans, ignoring refutations.
pub fn best_epistemic_plan<E: EpistemicLevel>(plans: impl IntoIterator<Item = E>) -> Option<E> {
    plans
        .into_iter()
        .filter_map(|level| level.positive_rank().map(|rank| (rank, level)))
        .max_by_key(|(rank, _)| *rank)
        .map(|(_, level)| level)
}

/// Explicit, structured epistemic/model assumption debt carried by a claim or receipt.
///
/// Examples:
/// - ITP: `code = "oracle_unreconstructed"`, `code = "cross_itp_axiom"`
/// - CompChem: `code = "classical_ff_only"`, `code = "implicit_solvent"`, `code = "fixed_protonation"`
/// - CompBio: `code = "surrogate_only"`, `code = "no_off_target_selectivity"`
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EpistemicDebt {
    pub code: String,
    pub scope: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,
}

impl EpistemicDebt {
    pub fn new(
        code: impl Into<String>,
        scope: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            code: code.into(),
            scope: scope.into(),
            detail: detail.into(),
        }
    }
}

/// Combined epistemic level and accumulated unresolved assumption/fidelity debts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EpistemicAssessment<E> {
    pub level: E,
    #[serde(default)]
    pub debts: BTreeSet<EpistemicDebt>,
}

impl<E> EpistemicAssessment<E> {
    pub fn new(level: E) -> Self {
        Self {
            level,
            debts: BTreeSet::new(),
        }
    }

    pub fn with_debts(level: E, debts: impl IntoIterator<Item = EpistemicDebt>) -> Self {
        Self {
            level,
            debts: debts.into_iter().collect(),
        }
    }
}

/// AND aggregation with debt propagation: takes the minimum positive level and
/// the deterministic union of all unresolved debts from `local` and every dependency.
pub fn aggregate_assessment_plan<E: EpistemicLevel>(
    local: &EpistemicAssessment<E>,
    dependencies: impl IntoIterator<Item = EpistemicAssessment<E>>,
) -> Option<EpistemicAssessment<E>> {
    let mut best = local.level;
    let mut rank = best.positive_rank()?;
    let mut debts = local.debts.clone();
    for dep in dependencies {
        let next = dep.level.positive_rank()?;
        if next < rank {
            best = dep.level;
            rank = next;
        }
        debts.extend(dep.debts);
    }
    Some(EpistemicAssessment { level: best, debts })
}

/// OR aggregation with debt awareness: picks the highest positive rank, breaking
/// ties in favor of fewer unresolved debts (and deterministically by debt set).
pub fn best_assessment_plan<E: EpistemicLevel>(
    plans: impl IntoIterator<Item = EpistemicAssessment<E>>,
) -> Option<EpistemicAssessment<E>> {
    let mut best: Option<(u8, EpistemicAssessment<E>)> = None;
    for plan in plans {
        let Some(rank) = plan.level.positive_rank() else {
            continue;
        };
        let replace = match &best {
            None => true,
            Some((best_rank, best_plan)) => {
                rank > *best_rank
                    || (rank == *best_rank
                        && (plan.debts.len() < best_plan.debts.len()
                            || (plan.debts.len() == best_plan.debts.len()
                                && plan.debts < best_plan.debts)))
            }
        };
        if replace {
            best = Some((rank, plan));
        }
    }
    best.map(|(_, plan)| plan)
}

wire_enum!(
    /// Generalized epistemic ladder for empirical / computational science domains
    /// (e.g. computational chemistry and computational biology).
    ///
    /// - `E0`: untested hypothesis
    /// - `E1`: heuristic / fast surrogate screened (never authoritative)
    /// - `E2a`: single-model computational disconfirmation (soft prune only)
    /// - `E2b`: exact invariant violation, orthogonal multi-fidelity disconfirmation,
    ///          or signed experimental falsification (hard falsification)
    /// - `E3`: protocol formalized, blind-reviewed, and control smoke-tested
    /// - `E4a`: single-fidelity physical/numerical simulation converged (carries fidelity debt)
    /// - `E4b`: multi-fidelity / orthogonal cross-verification agreed within error bounds
    /// - `E5`: externally grounded wet-lab or authoritative benchmark validation
    ScientificTrust {
        E0 => "E0",
        E1 => "E1",
        E2a => "E2a",
        E2b => "E2b",
        E3 => "E3",
        E4a => "E4a",
        E4b => "E4b",
        E5 => "E5",
    }
);

#[allow(clippy::derivable_impls)]
impl Default for ScientificTrust {
    fn default() -> Self {
        Self::E0
    }
}

impl EpistemicLevel for ScientificTrust {
    fn positive_rank(self) -> Option<u8> {
        match self {
            Self::E0 => Some(0),
            Self::E1 => Some(1),
            Self::E3 => Some(2),
            Self::E4a => Some(3),
            Self::E4b => Some(4),
            Self::E5 => Some(5),
            Self::E2a | Self::E2b => None,
        }
    }

    fn refutation_rank(self) -> Option<u8> {
        match self {
            Self::E2a => Some(1),
            Self::E2b => Some(2),
            _ => None,
        }
    }

    fn is_hard_refutation(self) -> bool {
        matches!(self, Self::E2b)
    }

    fn implies_debt(self) -> bool {
        matches!(self, Self::E1 | Self::E4a)
    }

    fn wire_name(self) -> &'static str {
        self.as_str()
    }
}

impl PartialOrd for ScientificTrust {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        compare_epistemic(*self, *other)
    }
}

/// Static capability and epistemic ceiling declaration for one verifier backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackendDescriptor<E: EpistemicLevel> {
    pub backend_id: &'static str,
    /// Highest positive epistemic level this backend is ever allowed to mint.
    pub max_positive_ceiling: E,
    /// Highest refutation level this backend is allowed to mint (`None` = cannot refute).
    pub max_refutation_ceiling: Option<E>,
    /// Mandatory assumption/fidelity debt codes attached to positive receipts.
    pub default_debts: &'static [&'static str],
    /// Whether promotion to its highest level requires orthogonal corroboration.
    pub requires_orthogonal_pair: bool,
}

/// Returned when a subagent or protocol attempts to claim an epistemic level
/// above what the backend is permitted to mint (anti-laundering guard).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EpistemicCeilingViolation {
    pub backend_id: String,
    pub claimed_level: &'static str,
    pub allowed_ceiling: Option<&'static str>,
}

impl std::fmt::Display for EpistemicCeilingViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.allowed_ceiling {
            Some(ceiling) => write!(
                f,
                "backend {:?} cannot mint epistemic level {} (ceiling is {})",
                self.backend_id, self.claimed_level, ceiling
            ),
            None => write!(
                f,
                "backend {:?} is not authorized to mint epistemic level {}",
                self.backend_id, self.claimed_level
            ),
        }
    }
}

impl std::error::Error for EpistemicCeilingViolation {}

/// Enforce that `claimed` does not exceed a backend's positive or refutation ceiling.
pub fn validate_backend_descriptor_ceiling<E: EpistemicLevel>(
    descriptor: &BackendDescriptor<E>,
    claimed: E,
) -> Result<(), EpistemicCeilingViolation> {
    if let Some(claimed_pos) = claimed.positive_rank() {
        let allowed = descriptor.max_positive_ceiling.positive_rank();
        if allowed.is_some_and(|max_pos| claimed_pos <= max_pos) {
            return Ok(());
        }
        return Err(EpistemicCeilingViolation {
            backend_id: descriptor.backend_id.to_owned(),
            claimed_level: claimed.wire_name(),
            allowed_ceiling: allowed.map(|_| descriptor.max_positive_ceiling.wire_name()),
        });
    }
    if let Some(claimed_ref) = claimed.refutation_rank() {
        let allowed = descriptor
            .max_refutation_ceiling
            .and_then(EpistemicLevel::refutation_rank);
        if allowed.is_some_and(|max_ref| claimed_ref <= max_ref) {
            return Ok(());
        }
        return Err(EpistemicCeilingViolation {
            backend_id: descriptor.backend_id.to_owned(),
            claimed_level: claimed.wire_name(),
            allowed_ceiling: descriptor
                .max_refutation_ceiling
                .map(EpistemicLevel::wire_name),
        });
    }
    Err(EpistemicCeilingViolation {
        backend_id: descriptor.backend_id.to_owned(),
        claimed_level: claimed.wire_name(),
        allowed_ceiling: None,
    })
}

/// Pluggable domain pack connecting a scientific/formal discipline to the
/// orchestration kernel without hardcoding ITP semantics into core workflows.
pub trait DomainPack {
    type Level: EpistemicLevel + 'static;
    type Backend: Copy + Eq;

    fn domain_id(&self) -> &'static str;
    fn display_name(&self) -> &'static str;
    fn backends(&self) -> &'static [BackendDescriptor<Self::Level>];
    fn descriptor(&self, backend: Self::Backend) -> &'static BackendDescriptor<Self::Level>;

    fn backend_ceiling(&self, backend: Self::Backend) -> Self::Level {
        self.descriptor(backend).max_positive_ceiling
    }

    fn default_debts(&self, backend: Self::Backend) -> &'static [&'static str] {
        self.descriptor(backend).default_debts
    }

    fn validate_backend_ceiling(
        &self,
        backend: Self::Backend,
        claimed: Self::Level,
    ) -> Result<(), EpistemicCeilingViolation> {
        validate_backend_descriptor_ceiling(self.descriptor(backend), claimed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ScientificTrust::*;

    #[test]
    fn scientific_trust_preserves_two_branch_partial_order() {
        assert_eq!(E2b.partial_cmp(&E3), None);
        assert_eq!(E5.partial_cmp(&E2a), None);
        assert_eq!(E4a.partial_cmp(&E2b), None);
        assert!(E2b > E2a && E2a > E1 && E1 > E0);
        assert!(E5 > E4b && E4b > E4a && E4a > E3 && E3 > E1 && E1 > E0);
        assert_eq!(E2a.partial_cmp(&E2a), Some(Ordering::Equal));
        assert!(E1.implies_debt());
        assert!(E4a.implies_debt());
        assert!(!E4b.implies_debt());
        assert!(!E5.implies_debt());
        assert!(E2b.is_hard_refutation());
        assert!(!E2a.is_hard_refutation());
    }

    #[test]
    fn assessment_and_or_propagates_debts_and_breaks_ties_deterministically() {
        let local = EpistemicAssessment::with_debts(
            E4b,
            [EpistemicDebt::new("fixed_protonation", "CLM-1", "pH 7.4")],
        );
        let dep_ok = EpistemicAssessment::with_debts(
            E4a,
            [EpistemicDebt::new("classical_ff_only", "CLM-2", "Amber19")],
        );
        let dep_clean = EpistemicAssessment::new(E4b);

        let plan_a = aggregate_assessment_plan(&local, [dep_ok]).unwrap();
        assert_eq!(plan_a.level, E4a);
        assert_eq!(plan_a.debts.len(), 2);

        let plan_b = aggregate_assessment_plan(&local, [dep_clean]).unwrap();
        assert_eq!(plan_b.level, E4b);
        assert_eq!(plan_b.debts.len(), 1);

        // Refuted dependency disqualifies the entire AND plan.
        assert_eq!(
            aggregate_assessment_plan(&local, [EpistemicAssessment::new(E2b)]),
            None
        );

        // OR prefers higher rank; on equal rank, prefers fewer debts.
        let best = best_assessment_plan([plan_a.clone(), plan_b.clone()]).unwrap();
        assert_eq!(best, plan_b);

        let fewer_debts = EpistemicAssessment::with_debts(
            E4a,
            [EpistemicDebt::new("implicit_solvent", "CLM-3", "GBSA")],
        );
        let best_tie = best_assessment_plan([plan_a, fewer_debts.clone()]).unwrap();
        assert_eq!(best_tie, fewer_debts);
    }

    #[test]
    fn backend_descriptor_blocks_surrogate_and_refutation_laundering() {
        let docking = BackendDescriptor {
            backend_id: "vina_surrogate",
            max_positive_ceiling: E1,
            max_refutation_ceiling: Some(E2a),
            default_debts: &["surrogate_only"],
            requires_orthogonal_pair: false,
        };
        assert!(validate_backend_descriptor_ceiling(&docking, E0).is_ok());
        assert!(validate_backend_descriptor_ceiling(&docking, E1).is_ok());
        assert!(validate_backend_descriptor_ceiling(&docking, E2a).is_ok());
        assert!(validate_backend_descriptor_ceiling(&docking, E4a).is_err());
        assert!(validate_backend_descriptor_ceiling(&docking, E5).is_err());
        assert!(validate_backend_descriptor_ceiling(&docking, E2b).is_err());

        let score_only = BackendDescriptor {
            backend_id: "plddt_scorer",
            max_positive_ceiling: E1,
            max_refutation_ceiling: None,
            default_debts: &["surrogate_only"],
            requires_orthogonal_pair: false,
        };
        assert!(validate_backend_descriptor_ceiling(&score_only, E2a).is_err());
    }
}
