//! Dual-channel deterministic Belief Propagation over AND-OR-Conflict factor
//! graphs using Jøsang Subjective Logic `(belief, disbelief, uncertainty)`.
//!
//! Architecture invariants (ADR-0007 D32):
//! - **Channel 1 (Hard Epistemic Floor)**: discrete `effective_rank`, accumulated
//!   `EpistemicDebt` union, and hard refutation / retraction veto are strictly
//!   non-compensable and never overridden by continuous belief scores.
//! - **Channel 2 (Subjective Logic BP)**: deterministic, auditable `(b, d, u)`
//!   propagation with condition-tag attenuation, shared-ancestor provenance
//!   deduplication (blocking double-counting on diamond DAGs), explicit conflict
//!   mass tracking, blame attribution, and exact counterfactual Value of
//!   Information (VoI) rankings for idle-time replication/formalization.

use crate::{EpistemicDebt, EpistemicTag, ReproducibilityClass, ScientificTrust, Trust, wire_enum};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
};

const EPS: f64 = 1e-9;

/// Subjective Logic opinion `(b, d, u)` with `b, d, u >= 0` and `b + d + u = 1`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Opinion {
    pub belief: f64,
    pub disbelief: f64,
    pub uncertainty: f64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BeliefError {
    InvalidOpinion(&'static str),
    InvalidGraph(&'static str),
    UnknownNode(String),
}

impl fmt::Display for BeliefError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidOpinion(msg) => write!(f, "invalid Subjective Logic opinion: {msg}"),
            Self::InvalidGraph(msg) => write!(f, "invalid belief factor graph: {msg}"),
            Self::UnknownNode(id) => write!(f, "unknown node in belief factor graph: {id:?}"),
        }
    }
}

impl std::error::Error for BeliefError {}

fn clean_zero(v: f64) -> f64 {
    if v.abs() < 1e-12 { 0.0 } else { v }
}

impl Opinion {
    pub fn new(belief: f64, disbelief: f64, uncertainty: f64) -> Result<Self, BeliefError> {
        if !belief.is_finite() || !disbelief.is_finite() || !uncertainty.is_finite() {
            return Err(BeliefError::InvalidOpinion(
                "opinion components must be finite",
            ));
        }
        if belief < -EPS || disbelief < -EPS || uncertainty < -EPS {
            return Err(BeliefError::InvalidOpinion(
                "opinion components must be non-negative",
            ));
        }
        let b = belief.clamp(0.0, 1.0);
        let d = disbelief.clamp(0.0, 1.0);
        let u = uncertainty.clamp(0.0, 1.0);
        let sum = b + d + u;
        if (sum - 1.0).abs() > 1e-6 {
            return Err(BeliefError::InvalidOpinion(
                "belief + disbelief + uncertainty must sum to 1.0",
            ));
        }
        Ok(Self {
            belief: clean_zero(b / sum),
            disbelief: clean_zero(d / sum),
            uncertainty: clean_zero(u / sum),
        })
    }

    pub const fn vacuous() -> Self {
        Self {
            belief: 0.0,
            disbelief: 0.0,
            uncertainty: 1.0,
        }
    }

    pub const fn dogmatic_true() -> Self {
        Self {
            belief: 1.0,
            disbelief: 0.0,
            uncertainty: 0.0,
        }
    }

    pub const fn dogmatic_false() -> Self {
        Self {
            belief: 0.0,
            disbelief: 1.0,
            uncertainty: 0.0,
        }
    }

    /// Auditable baseline prior for an ITP `Trust` level.
    pub const fn from_itp_trust(trust: Trust) -> Self {
        match trust {
            Trust::L0 => Self::vacuous(),
            Trust::L1 => Self {
                belief: 0.20,
                disbelief: 0.0,
                uncertainty: 0.80,
            },
            Trust::L2a => Self {
                belief: 0.0,
                disbelief: 0.55,
                uncertainty: 0.45,
            },
            Trust::L2b => Self::dogmatic_false(),
            Trust::L3 => Self {
                belief: 0.10,
                disbelief: 0.0,
                uncertainty: 0.90,
            },
            Trust::L4a => Self {
                belief: 0.70,
                disbelief: 0.0,
                uncertainty: 0.30,
            },
            Trust::L4b => Self {
                belief: 0.88,
                disbelief: 0.0,
                uncertainty: 0.12,
            },
            Trust::L5 => Self::dogmatic_true(),
        }
    }

    /// Auditable baseline prior for a computational/empirical `ScientificTrust` level.
    pub const fn from_scientific_trust(trust: ScientificTrust) -> Self {
        match trust {
            ScientificTrust::E0 => Self::vacuous(),
            ScientificTrust::E1 => Self {
                belief: 0.20,
                disbelief: 0.0,
                uncertainty: 0.80,
            },
            ScientificTrust::E2a => Self {
                belief: 0.0,
                disbelief: 0.55,
                uncertainty: 0.45,
            },
            ScientificTrust::E2b => Self::dogmatic_false(),
            ScientificTrust::E3 => Self {
                belief: 0.10,
                disbelief: 0.0,
                uncertainty: 0.90,
            },
            ScientificTrust::E4a => Self {
                belief: 0.70,
                disbelief: 0.0,
                uncertainty: 0.30,
            },
            ScientificTrust::E4b => Self {
                belief: 0.88,
                disbelief: 0.0,
                uncertainty: 0.12,
            },
            ScientificTrust::E5 => Self {
                belief: 0.96,
                disbelief: 0.0,
                uncertainty: 0.04,
            },
        }
    }

    pub fn from_epistemic_tag(tag: &EpistemicTag) -> Result<Self, BeliefError> {
        tag.validate()
            .map_err(|_| BeliefError::InvalidOpinion("invalid domain epistemic tag"))?;
        if let Some(t) = tag.as_itp() {
            Ok(Self::from_itp_trust(t))
        } else if let Some(s) = tag.as_scientific() {
            Ok(Self::from_scientific_trust(s))
        } else {
            Err(BeliefError::InvalidOpinion("unrecognized epistemic tag"))
        }
    }

    /// Projected probability `b + base_rate * u`.
    pub fn projected_probability(self, base_rate: f64) -> f64 {
        (self.belief + base_rate.clamp(0.0, 1.0) * self.uncertainty).clamp(0.0, 1.0)
    }

    /// Credal support interval `[lower = b, upper = 1 - d]` (width equals `uncertainty`).
    pub fn credal_interval(self) -> (f64, f64) {
        (self.belief, (1.0 - self.disbelief).max(self.belief))
    }

    /// Subjective Logic conjunction (AND factor): belief multiplies and disbelief
    /// accumulates disjunctively (`d1 + d2 - d1*d2`), so any disbelieved premise
    /// strongly suppresses the plan.
    pub fn conjoin(self, other: Self) -> Self {
        let b = (self.belief * other.belief).clamp(0.0, 1.0);
        let d =
            (self.disbelief + other.disbelief - self.disbelief * other.disbelief).clamp(0.0, 1.0);
        let u = (1.0 - b - d).clamp(0.0, 1.0);
        let sum = b + d + u;
        Self {
            belief: clean_zero(b / sum),
            disbelief: clean_zero(d / sum),
            uncertainty: clean_zero(u / sum),
        }
    }

    /// Pairwise conflict mass `K = b1*d2 + d1*b2 in [0, 1]`.
    pub fn conflict_with(self, other: Self) -> f64 {
        (self.belief * other.disbelief + self.disbelief * other.belief).clamp(0.0, 1.0)
    }

    /// Jøsang cumulative fusion for genuinely independent evidence sources.
    /// Returns `(fused_opinion, pairwise_conflict_mass)`.
    pub fn fuse_independent(self, other: Self) -> (Self, f64) {
        let conflict = self.conflict_with(other);
        let kappa = self.uncertainty + other.uncertainty - self.uncertainty * other.uncertainty;
        if kappa <= EPS {
            let b = 0.5 * (self.belief + other.belief);
            let d = 0.5 * (self.disbelief + other.disbelief);
            let sum = (b + d).max(EPS);
            return (
                Self {
                    belief: clean_zero(b / sum),
                    disbelief: clean_zero(d / sum),
                    uncertainty: 0.0,
                },
                conflict,
            );
        }
        let b = ((self.belief * other.uncertainty + other.belief * self.uncertainty) / kappa)
            .clamp(0.0, 1.0);
        let d = ((self.disbelief * other.uncertainty + other.disbelief * self.uncertainty) / kappa)
            .clamp(0.0, 1.0);
        let u = ((self.uncertainty * other.uncertainty) / kappa).clamp(0.0, 1.0);
        let sum = b + d + u;
        (
            Self {
                belief: clean_zero(b / sum),
                disbelief: clean_zero(d / sum),
                uncertainty: clean_zero(u / sum),
            },
            conflict,
        )
    }

    /// Jøsang averaging fusion (idempotent when fusing an opinion with itself:
    /// `o.fuse_averaging(o) == o`), used when two routes share identical evidence.
    pub fn fuse_averaging(self, other: Self) -> (Self, f64) {
        let conflict = self.conflict_with(other);
        let denom = self.uncertainty + other.uncertainty;
        if denom <= EPS {
            let b = 0.5 * (self.belief + other.belief);
            let d = 0.5 * (self.disbelief + other.disbelief);
            let sum = (b + d).max(EPS);
            return (
                Self {
                    belief: clean_zero(b / sum),
                    disbelief: clean_zero(d / sum),
                    uncertainty: 0.0,
                },
                conflict,
            );
        }
        let b = ((self.belief * other.uncertainty + other.belief * self.uncertainty) / denom)
            .clamp(0.0, 1.0);
        let d = ((self.disbelief * other.uncertainty + other.disbelief * self.uncertainty) / denom)
            .clamp(0.0, 1.0);
        let u = ((2.0 * self.uncertainty * other.uncertainty) / denom).clamp(0.0, 1.0);
        let sum = b + d + u;
        (
            Self {
                belief: clean_zero(b / sum),
                disbelief: clean_zero(d / sum),
                uncertainty: clean_zero(u / sum),
            },
            conflict,
        )
    }

    /// Provenance-overlap-aware fusion preventing double-counting on diamond DAGs:
    /// - `overlap_ratio == 0.0` (disjoint evidence) -> full cumulative fusion (reduces `u`).
    /// - `overlap_ratio == 1.0` (identical upstream evidence) -> idempotent averaging (zero double-count).
    pub fn fuse_with_overlap(self, other: Self, overlap_ratio: f64) -> (Self, f64) {
        let rho = overlap_ratio.clamp(0.0, 1.0);
        let (cum, conflict) = self.fuse_independent(other);
        if rho <= EPS {
            return (cum, conflict);
        }
        let (avg, _) = self.fuse_averaging(other);
        if (1.0 - rho) <= EPS {
            return (avg, conflict);
        }
        let b = (1.0 - rho) * cum.belief + rho * avg.belief;
        let d = (1.0 - rho) * cum.disbelief + rho * avg.disbelief;
        let u = (1.0 - rho) * cum.uncertainty + rho * avg.uncertainty;
        let sum = (b + d + u).max(EPS);
        (
            Self {
                belief: clean_zero(b / sum),
                disbelief: clean_zero(d / sum),
                uncertainty: clean_zero(u / sum),
            },
            conflict,
        )
    }

    /// Linear damping step used by bounded loopy belief propagation.
    pub fn damp_toward(self, target: Self, damping: f64) -> Self {
        let alpha = damping.clamp(0.0, 1.0);
        let b = (1.0 - alpha) * self.belief + alpha * target.belief;
        let d = (1.0 - alpha) * self.disbelief + alpha * target.disbelief;
        let u = (1.0 - alpha) * self.uncertainty + alpha * target.uncertainty;
        let sum = (b + d + u).max(EPS);
        Self {
            belief: clean_zero(b / sum),
            disbelief: clean_zero(d / sum),
            uncertainty: clean_zero(u / sum),
        }
    }

    pub fn l1_distance(self, other: Self) -> f64 {
        (self.belief - other.belief).abs()
            + (self.disbelief - other.disbelief).abs()
            + (self.uncertainty - other.uncertainty).abs()
    }
}

/// Deterministic quality/condition attenuation rule applied to tagged nodes
/// (e.g. `incomplete_conditions`, `unreproduced_literature`, `retracted`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttenuationRule {
    pub belief_multiplier: f64,
    pub max_belief: f64,
    pub min_uncertainty: f64,
    pub min_disbelief: f64,
    pub hard_veto: bool,
}

/// Auditable, order-independent policy mapping admission/condition tags to
/// deterministic Subjective Logic attenuation rules.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TagAttenuationPolicy {
    pub rules: BTreeMap<String, AttenuationRule>,
}

impl Default for TagAttenuationPolicy {
    fn default() -> Self {
        let rules = BTreeMap::from([
            (
                "unreproduced_literature".into(),
                AttenuationRule {
                    belief_multiplier: 0.75,
                    max_belief: 0.55,
                    min_uncertainty: 0.45,
                    min_disbelief: 0.0,
                    hard_veto: false,
                },
            ),
            (
                "incomplete_conditions".into(),
                AttenuationRule {
                    belief_multiplier: 0.70,
                    max_belief: 0.45,
                    min_uncertainty: 0.55,
                    min_disbelief: 0.0,
                    hard_veto: false,
                },
            ),
            (
                "missing_standing_assumptions".into(),
                AttenuationRule {
                    belief_multiplier: 0.65,
                    max_belief: 0.40,
                    min_uncertainty: 0.60,
                    min_disbelief: 0.0,
                    hard_veto: false,
                },
            ),
            (
                "secondary_citation".into(),
                AttenuationRule {
                    belief_multiplier: 0.50,
                    max_belief: 0.30,
                    min_uncertainty: 0.70,
                    min_disbelief: 0.0,
                    hard_veto: false,
                },
            ),
            (
                "author_speculation".into(),
                AttenuationRule {
                    belief_multiplier: 0.25,
                    max_belief: 0.15,
                    min_uncertainty: 0.85,
                    min_disbelief: 0.0,
                    hard_veto: false,
                },
            ),
            (
                "unattested_remote_hpc".into(),
                AttenuationRule {
                    belief_multiplier: 0.95,
                    max_belief: 0.90,
                    min_uncertainty: 0.10,
                    min_disbelief: 0.0,
                    hard_veto: false,
                },
            ),
            (
                "surrogate_only".into(),
                AttenuationRule {
                    belief_multiplier: 0.50,
                    max_belief: 0.25,
                    min_uncertainty: 0.75,
                    min_disbelief: 0.0,
                    hard_veto: false,
                },
            ),
            (
                "retracted".into(),
                AttenuationRule {
                    belief_multiplier: 0.0,
                    max_belief: 0.0,
                    min_uncertainty: 0.15,
                    min_disbelief: 0.85,
                    hard_veto: true,
                },
            ),
        ]);
        Self { rules }
    }
}

impl TagAttenuationPolicy {
    /// Apply all matching tag attenuation rules commutatively to `base`.
    /// Returns `(attenuated_opinion, hard_vetoed)`.
    pub fn apply(&self, base: Opinion, tags: &[String]) -> (Opinion, bool) {
        let mut mult = 1.0f64;
        let mut max_b = 1.0f64;
        let mut min_u = 0.0f64;
        let mut min_d = 0.0f64;
        let mut veto = false;
        let unique: BTreeSet<&str> = tags.iter().map(String::as_str).collect();
        for tag in unique {
            if let Some(rule) = self.rules.get(tag) {
                mult *= rule.belief_multiplier.clamp(0.0, 1.0);
                max_b = max_b.min(rule.max_belief.clamp(0.0, 1.0));
                min_u = min_u.max(rule.min_uncertainty.clamp(0.0, 1.0));
                min_d = min_d.max(rule.min_disbelief.clamp(0.0, 1.0));
                veto |= rule.hard_veto;
            }
        }
        if veto {
            let d = base.disbelief.max(min_d).clamp(0.0, 1.0);
            let u = (1.0 - d).clamp(0.0, 1.0);
            return (
                Opinion {
                    belief: 0.0,
                    disbelief: clean_zero(d),
                    uncertainty: clean_zero(u),
                },
                true,
            );
        }
        let d = base.disbelief.max(min_d).clamp(0.0, 1.0);
        let mut b = (base.belief * mult).min(max_b).clamp(0.0, 1.0 - d);
        let mut u = (1.0 - b - d).max(min_u).clamp(0.0, 1.0);
        if b + d + u > 1.0 {
            // Preserve disbelief and uncertainty floor by shifting excess out of belief.
            b = (1.0 - d - u).max(0.0);
            u = (1.0 - b - d).clamp(0.0, 1.0);
        }
        let sum = (b + d + u).max(EPS);
        (
            Opinion {
                belief: clean_zero(b / sum),
                disbelief: clean_zero(d / sum),
                uncertainty: clean_zero(u / sum),
            },
            false,
        )
    }
}

wire_enum!(
    /// Explicit corroboration or conflict factor edge between two graph nodes.
    ConflictEdgeKind {
        Contradicts => "contradicts",
        Replicates => "replicates",
    }
);

/// Node input to the AND-OR-Conflict belief factor graph.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BeliefNodeInput {
    pub node_id: String,
    pub epistemic: EpistemicTag,
    pub is_critical_path: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_opinion: Option<Opinion>,
    #[serde(default)]
    pub condition_tags: Vec<String>,
    #[serde(default)]
    pub local_debts: BTreeSet<EpistemicDebt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reproducibility: Option<ReproducibilityClass>,
}

/// AND factor connecting a target Claim node to its required premise nodes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BeliefPlanInput {
    pub plan_id: String,
    pub target_node_id: String,
    pub dependencies: Vec<String>,
    pub active: bool,
}

/// Directed corroboration (`Replicates`) or contradiction (`Contradicts`) factor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BeliefConflictEdge {
    pub edge_id: String,
    pub source_node_id: String,
    pub target_node_id: String,
    pub kind: ConflictEdgeKind,
    /// Coupling weight in `(0.0, 1.0]`.
    pub weight: f64,
}

/// Complete factor-graph snapshot submitted to `BeliefPropagator`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BeliefGraph {
    pub nodes: Vec<BeliefNodeInput>,
    pub plans: Vec<BeliefPlanInput>,
    #[serde(default)]
    pub conflict_edges: Vec<BeliefConflictEdge>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BeliefPropagationConfig {
    pub max_iterations: u32,
    pub damping: f64,
    pub convergence_epsilon: f64,
}

impl Default for BeliefPropagationConfig {
    fn default() -> Self {
        Self {
            max_iterations: 32,
            damping: 0.5,
            convergence_epsilon: 1e-6,
        }
    }
}

/// One node's combined Channel 1 (hard floor/debts) + Channel 2 (BP opinion) state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeBeliefState {
    pub node_id: String,
    /// Channel 1: hardest surviving positive rank (None if vetoed or all plans dead).
    pub effective_positive_rank: Option<u8>,
    /// Channel 1: whether this node or its sole surviving support carries a hard veto.
    pub hard_vetoed: bool,
    /// Channel 1: accumulated non-compensable assumption/condition debts.
    pub accumulated_debts: BTreeSet<EpistemicDebt>,
    /// Channel 2: local attenuated opinion before upstream graph propagation.
    pub local_opinion: Opinion,
    /// Channel 2: propagated Subjective Logic opinion `(b, d, u)`.
    pub propagated_opinion: Opinion,
    /// Channel 2: maximum observed pairwise conflict mass incident to this node.
    pub conflict_mass: f64,
    /// Provenance leaf IDs supporting this node's belief (used for overlap dedup & blame).
    pub supporting_leaves: BTreeSet<String>,
}

/// Backward sensitivity (Value of Information) ranking of an uncertain upstream
/// node relative to critical-path root conclusions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VoIRanking {
    pub node_id: String,
    pub current_uncertainty: f64,
    /// Total marginal belief gain across critical-path roots if `node_id` is verified.
    pub belief_gain_on_critical_paths: f64,
    /// Total uncertainty reduction across critical-path roots if `node_id` is verified.
    pub uncertainty_reduction_on_critical_paths: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reproducibility: Option<ReproducibilityClass>,
    pub condition_tags: Vec<String>,
}

/// Deterministic output of one full dual-channel belief propagation pass.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BeliefPropagationReport {
    pub converged: bool,
    pub iterations: u32,
    pub nodes: BTreeMap<String, NodeBeliefState>,
    /// Ordered descending by `belief_gain_on_critical_paths`, deterministic tie-break by `node_id`.
    pub voi_rankings: Vec<VoIRanking>,
}

/// Pluggable belief propagation strategy trait.
pub trait BeliefPropagator {
    fn propagate(
        &self,
        graph: &BeliefGraph,
        attenuation: &TagAttenuationPolicy,
        config: &BeliefPropagationConfig,
    ) -> Result<BeliefPropagationReport, BeliefError>;
}

/// Default deterministic damped loopy Subjective Logic BP engine with shared-
/// ancestor provenance deduplication and exact counterfactual VoI sensitivity.
#[derive(Debug, Default, Clone, Copy)]
pub struct SubjectiveLogicPropagator;

fn provenance_overlap(a: &BTreeSet<String>, b: &BTreeSet<String>) -> f64 {
    let min_len = a.len().min(b.len());
    if min_len == 0 {
        return 0.0;
    }
    let shared = a.intersection(b).count();
    (shared as f64) / (min_len as f64)
}

fn validate_graph(graph: &BeliefGraph) -> Result<BTreeMap<&str, &BeliefNodeInput>, BeliefError> {
    let mut index = BTreeMap::new();
    for node in &graph.nodes {
        if node.node_id.is_empty() {
            return Err(BeliefError::InvalidGraph("empty node_id"));
        }
        node.epistemic
            .validate()
            .map_err(|_| BeliefError::InvalidGraph("node has invalid epistemic tag"))?;
        if let Some(op) = node.base_opinion {
            Opinion::new(op.belief, op.disbelief, op.uncertainty)?;
        }
        if index.insert(node.node_id.as_str(), node).is_some() {
            return Err(BeliefError::InvalidGraph("duplicate node_id"));
        }
    }
    for plan in &graph.plans {
        if !index.contains_key(plan.target_node_id.as_str()) {
            return Err(BeliefError::UnknownNode(plan.target_node_id.clone()));
        }
        for dep in &plan.dependencies {
            if !index.contains_key(dep.as_str()) {
                return Err(BeliefError::UnknownNode(dep.clone()));
            }
        }
    }
    for edge in &graph.conflict_edges {
        if !index.contains_key(edge.source_node_id.as_str()) {
            return Err(BeliefError::UnknownNode(edge.source_node_id.clone()));
        }
        if !index.contains_key(edge.target_node_id.as_str()) {
            return Err(BeliefError::UnknownNode(edge.target_node_id.clone()));
        }
        if !edge.weight.is_finite() || edge.weight <= 0.0 || edge.weight > 1.0 {
            return Err(BeliefError::InvalidGraph(
                "conflict edge weight must be finite and in (0.0, 1.0]",
            ));
        }
    }
    Ok(index)
}

fn run_forward_pass(
    graph: &BeliefGraph,
    index: &BTreeMap<&str, &BeliefNodeInput>,
    attenuation: &TagAttenuationPolicy,
    config: &BeliefPropagationConfig,
    verified_override: Option<&str>,
) -> Result<(bool, u32, BTreeMap<String, NodeBeliefState>), BeliefError> {
    let mut plans_by_target: BTreeMap<&str, Vec<&BeliefPlanInput>> = BTreeMap::new();
    for plan in &graph.plans {
        if plan.active {
            plans_by_target
                .entry(plan.target_node_id.as_str())
                .or_default()
                .push(plan);
        }
    }
    let mut conflicts_by_target: BTreeMap<&str, Vec<&BeliefConflictEdge>> = BTreeMap::new();
    for edge in &graph.conflict_edges {
        conflicts_by_target
            .entry(edge.target_node_id.as_str())
            .or_default()
            .push(edge);
    }

    // Initialize local attenuated opinions and Channel 1 states.
    let mut states: BTreeMap<String, NodeBeliefState> = BTreeMap::new();
    for (&id, node) in index {
        let (local_op, vetoed, local_rank, local_debts) = if verified_override == Some(id) {
            let op = if node.epistemic.domain_id == "itp" {
                Opinion::dogmatic_true()
            } else {
                Opinion::from_scientific_trust(ScientificTrust::E4b)
            };
            let rank = if node.epistemic.domain_id == "itp" {
                Some(5)
            } else {
                Some(4)
            };
            (op, false, rank, BTreeSet::new())
        } else {
            let base = match node.base_opinion {
                Some(op) => op,
                None => Opinion::from_epistemic_tag(&node.epistemic)?,
            };
            let (attenuated, tag_veto) = attenuation.apply(base, &node.condition_tags);
            let hard_veto = tag_veto || node.epistemic.is_hard_refutation();
            let mut debts = node.local_debts.clone();
            for tag in &node.condition_tags {
                if attenuation.rules.contains_key(tag) {
                    debts.insert(EpistemicDebt::new(tag.clone(), id, "condition_tag"));
                }
            }
            let rank = if hard_veto {
                None
            } else {
                node.epistemic.positive_rank()
            };
            (attenuated, hard_veto, rank, debts)
        };

        let mut leaves = BTreeSet::new();
        leaves.insert(id.to_owned());
        states.insert(
            id.to_owned(),
            NodeBeliefState {
                node_id: id.to_owned(),
                effective_positive_rank: local_rank,
                hard_vetoed: vetoed,
                accumulated_debts: local_debts,
                local_opinion: local_op,
                propagated_opinion: local_op,
                conflict_mass: 0.0,
                supporting_leaves: leaves,
            },
        );
    }

    let max_iters = config.max_iterations.max(1);
    let mut converged = false;
    let mut iterations = 0u32;

    for iter in 1..=max_iters {
        iterations = iter;
        let prev = states.clone();
        let mut max_delta = 0.0f64;

        for &id in index.keys() {
            let cur_local = &prev[id];
            let mut target_op = cur_local.local_opinion;
            let mut target_conflict = 0.0f64;
            let mut target_rank = cur_local.effective_positive_rank;
            let mut target_veto = cur_local.hard_vetoed;
            let mut target_debts = cur_local.accumulated_debts.clone();
            let mut target_leaves = BTreeSet::from([id.to_owned()]);

            // Aggregate active AND plans -> OR node fusion with provenance deduplication.
            if let Some(plans) = plans_by_target.get(id)
                && !plans.is_empty()
            {
                let mut surviving_plan_opinions: Vec<(Opinion, BTreeSet<String>)> = Vec::new();
                let mut best_plan_channel1: Option<(u8, BTreeSet<EpistemicDebt>)> = None;
                let mut any_plan_non_vetoed = false;

                for plan in plans {
                    // AND conjunction over dependencies.
                    let mut plan_op = Opinion::dogmatic_true();
                    let mut plan_leaves = BTreeSet::new();
                    let mut plan_rank = cur_local.effective_positive_rank;
                    let mut plan_debts = cur_local.accumulated_debts.clone();
                    let mut plan_vetoed = cur_local.hard_vetoed;

                    for dep_id in &plan.dependencies {
                        let dep = &prev[dep_id];
                        plan_op = plan_op.conjoin(dep.propagated_opinion);
                        plan_leaves.extend(dep.supporting_leaves.iter().cloned());
                        plan_debts.extend(dep.accumulated_debts.iter().cloned());
                        if dep.hard_vetoed {
                            plan_vetoed = true;
                            plan_rank = None;
                        } else if let (Some(pr), Some(dr)) =
                            (plan_rank, dep.effective_positive_rank)
                        {
                            plan_rank = Some(pr.min(dr));
                        } else {
                            plan_rank = None;
                        }
                    }

                    // Combine local proof/evidence step with its dependencies:
                    // If the Claim itself is not yet locally verified (L0/E0), let plan support
                    // drive belief while Channel 1 rank still obeys local/dependency floor.
                    if cur_local.local_opinion != Opinion::vacuous() {
                        plan_op = plan_op.conjoin(cur_local.local_opinion);
                    }
                    if !plan_vetoed {
                        any_plan_non_vetoed = true;
                        if let Some(rank) = plan_rank {
                            let replace = match &best_plan_channel1 {
                                None => true,
                                Some((best_r, best_d)) => {
                                    rank > *best_r
                                        || (rank == *best_r && plan_debts.len() < best_d.len())
                                }
                            };
                            if replace {
                                best_plan_channel1 = Some((rank, plan_debts));
                            }
                        }
                    }
                    surviving_plan_opinions.push((plan_op, plan_leaves));
                }

                // OR fusion across plans with shared-ancestor overlap discounting!
                let mut fused: Option<(Opinion, BTreeSet<String>)> = None;
                for (p_op, p_leaves) in surviving_plan_opinions {
                    fused = Some(match fused {
                        None => (p_op, p_leaves),
                        Some((acc_op, mut acc_leaves)) => {
                            let rho = provenance_overlap(&acc_leaves, &p_leaves);
                            let (next_op, c) = acc_op.fuse_with_overlap(p_op, rho);
                            target_conflict = target_conflict.max(c);
                            acc_leaves.extend(p_leaves);
                            (next_op, acc_leaves)
                        }
                    });
                }
                if let Some((op, leaves)) = fused {
                    target_op = op;
                    target_leaves = leaves;
                    target_leaves.insert(id.to_owned());
                }
                if let Some((rank, debts)) = best_plan_channel1 {
                    target_rank = Some(rank);
                    target_debts = debts;
                    target_veto = false;
                } else if !any_plan_non_vetoed {
                    target_rank = None;
                    target_veto = true;
                }
            }

            // Apply explicit Contradicts / Replicates conflict edges into target node.
            if let Some(edges) = conflicts_by_target.get(id) {
                for edge in edges {
                    let src = &prev[&edge.source_node_id];
                    let strength = (src.propagated_opinion.belief * edge.weight).clamp(0.0, 1.0);
                    if strength <= EPS {
                        continue;
                    }
                    let msg = match edge.kind {
                        ConflictEdgeKind::Contradicts => Opinion {
                            belief: 0.0,
                            disbelief: strength,
                            uncertainty: (1.0 - strength).clamp(0.0, 1.0),
                        },
                        ConflictEdgeKind::Replicates => Opinion {
                            belief: strength,
                            disbelief: 0.0,
                            uncertainty: (1.0 - strength).clamp(0.0, 1.0),
                        },
                    };
                    let (next_op, c) = target_op.fuse_independent(msg);
                    target_op = next_op;
                    target_conflict = target_conflict.max(c);
                    if edge.kind == ConflictEdgeKind::Replicates {
                        target_leaves.extend(src.supporting_leaves.iter().cloned());
                    }
                }
            }

            // On first iteration take direct update; subsequent iterations damp to prevent oscillation.
            let next_op = if iter == 1 {
                target_op
            } else {
                prev[id]
                    .propagated_opinion
                    .damp_toward(target_op, config.damping)
            };
            max_delta = max_delta.max(next_op.l1_distance(prev[id].propagated_opinion));

            let entry = states.get_mut(id).expect("node exists");
            entry.effective_positive_rank = target_rank;
            entry.hard_vetoed = target_veto;
            entry.accumulated_debts = target_debts;
            entry.propagated_opinion = next_op;
            entry.conflict_mass = target_conflict;
            entry.supporting_leaves = target_leaves;
        }

        if max_delta <= config.convergence_epsilon {
            converged = true;
            break;
        }
    }

    Ok((converged, iterations, states))
}

impl BeliefPropagator for SubjectiveLogicPropagator {
    fn propagate(
        &self,
        graph: &BeliefGraph,
        attenuation: &TagAttenuationPolicy,
        config: &BeliefPropagationConfig,
    ) -> Result<BeliefPropagationReport, BeliefError> {
        let index = validate_graph(graph)?;
        let (converged, iterations, nodes) =
            run_forward_pass(graph, &index, attenuation, config, None)?;

        let critical_roots: Vec<&str> = graph
            .nodes
            .iter()
            .filter(|n| n.is_critical_path)
            .map(|n| n.node_id.as_str())
            .collect();

        let mut voi_rankings = Vec::new();
        if !critical_roots.is_empty() {
            for node in &graph.nodes {
                let state = &nodes[&node.node_id];
                if state.local_opinion.uncertainty <= 0.05 || state.hard_vetoed {
                    continue;
                }
                let (_, _, perturbed) = run_forward_pass(
                    graph,
                    &index,
                    attenuation,
                    config,
                    Some(node.node_id.as_str()),
                )?;
                let mut belief_gain = 0.0f64;
                let mut unc_reduction = 0.0f64;
                for &root_id in &critical_roots {
                    let base = nodes[root_id].propagated_opinion;
                    let next = perturbed[root_id].propagated_opinion;
                    belief_gain += (next.belief - base.belief).max(0.0);
                    unc_reduction += (base.uncertainty - next.uncertainty).max(0.0);
                }
                if belief_gain > 1e-6 || unc_reduction > 1e-6 {
                    voi_rankings.push(VoIRanking {
                        node_id: node.node_id.clone(),
                        current_uncertainty: state.local_opinion.uncertainty,
                        belief_gain_on_critical_paths: belief_gain,
                        uncertainty_reduction_on_critical_paths: unc_reduction,
                        reproducibility: node.reproducibility,
                        condition_tags: node.condition_tags.clone(),
                    });
                }
            }
            voi_rankings.sort_by(|a, b| {
                b.belief_gain_on_critical_paths
                    .total_cmp(&a.belief_gain_on_critical_paths)
                    .then_with(|| a.node_id.cmp(&b.node_id))
            });
        }

        Ok(BeliefPropagationReport {
            converged,
            iterations,
            nodes,
            voi_rankings,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subjective_logic_distinguishes_ignorance_from_high_conflict_and_prevents_double_count() {
        let ign = Opinion::vacuous();
        assert_eq!(ign.credal_interval(), (0.0, 1.0));

        let pos = Opinion::new(0.8, 0.0, 0.2).unwrap();
        let neg = Opinion::new(0.0, 0.8, 0.2).unwrap();
        let (contested, conflict) = pos.fuse_independent(neg);
        assert!(conflict > 0.6);
        assert!(contested.belief > 0.4 && contested.disbelief > 0.4);
        assert!(contested.uncertainty < 0.2);

        // Two genuinely independent positive routes reduce uncertainty.
        let (boosted, c0) = pos.fuse_independent(pos);
        assert_eq!(c0, 0.0);
        assert!(boosted.belief > pos.belief);
        assert!(boosted.uncertainty < pos.uncertainty);

        // Full provenance overlap (rho = 1.0) is idempotent: ZERO double counting!
        let (deduped, _) = pos.fuse_with_overlap(pos, 1.0);
        assert!((deduped.belief - pos.belief).abs() < 1e-9);
        assert!((deduped.uncertainty - pos.uncertainty).abs() < 1e-9);
    }

    #[test]
    fn condition_tags_attenuate_commutatively_and_retracted_vetoes_belief() {
        let policy = TagAttenuationPolicy::default();
        let base = Opinion::new(0.80, 0.0, 0.20).unwrap();

        let (a, veto_a) = policy.apply(
            base,
            &[
                "unreproduced_literature".into(),
                "incomplete_conditions".into(),
            ],
        );
        let (b, veto_b) = policy.apply(
            base,
            &[
                "incomplete_conditions".into(),
                "unreproduced_literature".into(),
            ],
        );
        assert!(!veto_a && !veto_b);
        assert_eq!(a, b);
        assert!(a.belief <= 0.45 + 1e-9);
        assert!(a.uncertainty >= 0.55 - 1e-9);

        let (retracted, veto_r) = policy.apply(base, &["retracted".into()]);
        assert!(veto_r);
        assert_eq!(retracted.belief, 0.0);
        assert!(retracted.disbelief >= 0.85);
    }

    #[test]
    fn factor_graph_bp_computes_shared_ancestor_dedup_conflict_and_voi_rankings() {
        // Diamond DAG:
        //   ROOT (critical) <- Plan A (AND: LIT_A, COMP_OK)
        //                   <- Plan B (AND: LIT_A, LIT_B)   [shares LIT_A!]
        // Plus a contradicting source CONTRADICT -> LIT_B
        let graph = BeliefGraph {
            nodes: vec![
                BeliefNodeInput {
                    node_id: "ROOT".into(),
                    epistemic: EpistemicTag::from_scientific("compchem", ScientificTrust::E0)
                        .unwrap(),
                    is_critical_path: true,
                    base_opinion: None,
                    condition_tags: vec![],
                    local_debts: BTreeSet::new(),
                    reproducibility: None,
                },
                BeliefNodeInput {
                    node_id: "LIT_A".into(),
                    epistemic: EpistemicTag::from_scientific("compchem", ScientificTrust::E4a)
                        .unwrap(),
                    is_critical_path: false,
                    base_opinion: None,
                    condition_tags: vec![
                        "unreproduced_literature".into(),
                        "incomplete_conditions".into(),
                    ],
                    local_debts: BTreeSet::new(),
                    reproducibility: Some(ReproducibilityClass::ReplicableByProtocol),
                },
                BeliefNodeInput {
                    node_id: "COMP_OK".into(),
                    epistemic: EpistemicTag::from_scientific("compchem", ScientificTrust::E4b)
                        .unwrap(),
                    is_critical_path: false,
                    base_opinion: None,
                    condition_tags: vec![],
                    local_debts: BTreeSet::new(),
                    reproducibility: None,
                },
                BeliefNodeInput {
                    node_id: "LIT_B".into(),
                    epistemic: EpistemicTag::from_scientific("compchem", ScientificTrust::E4a)
                        .unwrap(),
                    is_critical_path: false,
                    base_opinion: None,
                    condition_tags: vec!["unreproduced_literature".into()],
                    local_debts: BTreeSet::new(),
                    reproducibility: Some(ReproducibilityClass::ReplicableByProtocol),
                },
                BeliefNodeInput {
                    node_id: "CONTRA".into(),
                    epistemic: EpistemicTag::from_scientific("compchem", ScientificTrust::E4a)
                        .unwrap(),
                    is_critical_path: false,
                    base_opinion: None,
                    condition_tags: vec![],
                    local_debts: BTreeSet::new(),
                    reproducibility: None,
                },
            ],
            plans: vec![
                BeliefPlanInput {
                    plan_id: "PLAN_A".into(),
                    target_node_id: "ROOT".into(),
                    dependencies: vec!["LIT_A".into(), "COMP_OK".into()],
                    active: true,
                },
                BeliefPlanInput {
                    plan_id: "PLAN_B".into(),
                    target_node_id: "ROOT".into(),
                    dependencies: vec!["LIT_A".into(), "LIT_B".into()],
                    active: true,
                },
            ],
            conflict_edges: vec![BeliefConflictEdge {
                edge_id: "C1".into(),
                source_node_id: "CONTRA".into(),
                target_node_id: "LIT_B".into(),
                kind: ConflictEdgeKind::Contradicts,
                weight: 0.8,
            }],
        };

        let report = SubjectiveLogicPropagator
            .propagate(
                &graph,
                &TagAttenuationPolicy::default(),
                &BeliefPropagationConfig::default(),
            )
            .unwrap();
        assert!(report.converged);
        // LIT_B carries explicit conflict from CONTRA.
        assert!(report.nodes["LIT_B"].conflict_mass > 0.1);
        assert!(report.nodes["LIT_B"].propagated_opinion.disbelief > 0.2);

        // ROOT inherits incomplete_conditions + unreproduced_literature debts in Channel 1.
        let root = &report.nodes["ROOT"];
        assert!(
            root.accumulated_debts
                .iter()
                .any(|d| d.code == "incomplete_conditions")
        );

        // VoI ranks LIT_A as the #1 bottleneck because BOTH plans depend on LIT_A!
        let top_voi = &report.voi_rankings[0];
        assert_eq!(top_voi.node_id, "LIT_A");
        assert!(top_voi.belief_gain_on_critical_paths > 0.2);
    }

    #[test]
    fn retracted_premise_hard_vetoes_sole_plan_and_loopy_conflict_converges() {
        let graph = BeliefGraph {
            nodes: vec![
                BeliefNodeInput {
                    node_id: "CLAIM".into(),
                    epistemic: EpistemicTag::from_scientific("compbio", ScientificTrust::E4a)
                        .unwrap(),
                    is_critical_path: true,
                    base_opinion: None,
                    condition_tags: vec![],
                    local_debts: BTreeSet::new(),
                    reproducibility: None,
                },
                BeliefNodeInput {
                    node_id: "RETRACTED_PAPER".into(),
                    epistemic: EpistemicTag::from_scientific("compbio", ScientificTrust::E4b)
                        .unwrap(),
                    is_critical_path: false,
                    base_opinion: None,
                    condition_tags: vec!["retracted".into()],
                    local_debts: BTreeSet::new(),
                    reproducibility: None,
                },
                BeliefNodeInput {
                    node_id: "CYCLE_A".into(),
                    epistemic: EpistemicTag::from_scientific("compbio", ScientificTrust::E4a)
                        .unwrap(),
                    is_critical_path: false,
                    base_opinion: None,
                    condition_tags: vec![],
                    local_debts: BTreeSet::new(),
                    reproducibility: None,
                },
                BeliefNodeInput {
                    node_id: "CYCLE_B".into(),
                    epistemic: EpistemicTag::from_scientific("compbio", ScientificTrust::E4a)
                        .unwrap(),
                    is_critical_path: false,
                    base_opinion: None,
                    condition_tags: vec![],
                    local_debts: BTreeSet::new(),
                    reproducibility: None,
                },
            ],
            plans: vec![BeliefPlanInput {
                plan_id: "P1".into(),
                target_node_id: "CLAIM".into(),
                dependencies: vec!["RETRACTED_PAPER".into()],
                active: true,
            }],
            conflict_edges: vec![
                BeliefConflictEdge {
                    edge_id: "AB".into(),
                    source_node_id: "CYCLE_A".into(),
                    target_node_id: "CYCLE_B".into(),
                    kind: ConflictEdgeKind::Contradicts,
                    weight: 0.6,
                },
                BeliefConflictEdge {
                    edge_id: "BA".into(),
                    source_node_id: "CYCLE_B".into(),
                    target_node_id: "CYCLE_A".into(),
                    kind: ConflictEdgeKind::Contradicts,
                    weight: 0.6,
                },
            ],
        };
        let report = SubjectiveLogicPropagator
            .propagate(
                &graph,
                &TagAttenuationPolicy::default(),
                &BeliefPropagationConfig::default(),
            )
            .unwrap();
        assert!(report.converged);
        let claim = &report.nodes["CLAIM"];
        assert!(claim.hard_vetoed);
        assert_eq!(claim.effective_positive_rank, None);
        assert_eq!(claim.propagated_opinion.belief, 0.0);
        assert!(claim.propagated_opinion.disbelief >= 0.85);
        // Symmetric mutual contradiction converges symmetrically with high conflict mass.
        let a = &report.nodes["CYCLE_A"];
        let b = &report.nodes["CYCLE_B"];
        assert!((a.propagated_opinion.belief - b.propagated_opinion.belief).abs() < 1e-5);
        assert!(a.conflict_mass > 0.2);
    }
}
