//! Phase-one data contracts with the wire spellings in schemas/contracts.schema.json.
//! Dispatch engine labels include non-executable oracle labels; only `Engine` selects an ITP.
use crate::{Trust, wire_enum};
use serde::{Deserialize, Serialize};

wire_enum!(Engine { Lean4 => "lean4", Agda => "agda", Rocq => "rocq" });
wire_enum!(DispatchEngine { Lean4 => "lean4", Rocq => "rocq", Agda => "agda", Smt => "smt", Atp => "atp", Cas => "cas", Python => "python" });
wire_enum!(FormalEngine { Lean4 => "lean4", Rocq => "rocq", Agda => "agda", Smt => "smt", Atp => "atp", Cas => "cas" });
wire_enum!(Role { ChiefOrchestrator => "chief_orchestrator", RegionalOrchestrator => "regional_orchestrator", Explorer => "explorer", InfraBuilder => "infra_builder", ProverWorker => "prover_worker", ToolAutomation => "tool_automation", IndependentReviewer => "independent_reviewer" });
wire_enum!(WikiStratum { Core => "core", Sediment => "sediment", Graveyard => "graveyard" });
wire_enum!(ClaimEngine { Lean4 => "lean4", Agda => "agda", Rocq => "rocq", Smt => "smt", Atp => "atp", Cas => "cas", Informal => "informal" });
wire_enum!(WorkspaceMode { Readonly => "readonly", Scratch => "scratch", CowWorktree => "cow_worktree" });
wire_enum!(ObjectiveMode { ProveOnly => "PROVE_ONLY", ProveOrRefute => "PROVE_OR_REFUTE", BuildInfra => "BUILD_INFRA", Explore => "EXPLORE", AuditSpec => "AUDIT_SPEC", AuditBridge => "AUDIT_BRIDGE", GolfProof => "GOLF_PROOF", RehydrateSediment => "REHYDRATE_SEDIMENT", HammerReconstruct => "HAMMER_RECONSTRUCT", OraclePrune => "ORACLE_PRUNE", WitnessVerify => "WITNESS_VERIFY" });
wire_enum!(VerifiedLevel { L4a => "L4a", L4b => "L4b", L5 => "L5" });
wire_enum!(ResultStatus { Proved => "PROVED", Refuted => "REFUTED", PartialProgress => "PARTIAL_PROGRESS", BlockedMissingInfra => "BLOCKED_MISSING_INFRA", BlockedSpecDefect => "BLOCKED_SPEC_DEFECT", BudgetExhausted => "BUDGET_EXHAUSTED", Cancelled => "CANCELLED" });
wire_enum!(DiagnosisCategory { ConceptualGap => "conceptual_gap", MissingMathlibApi => "missing_mathlib_api", TacticTimeout => "tactic_timeout", MissingHypothesis => "missing_hypothesis", DefinitionMismatch => "definition_mismatch", EngineLimitation => "engine_limitation" });
wire_enum!(InfraStatus { Draft => "DRAFT", Approved => "APPROVED", BuildingSmoke => "BUILDING_SMOKE", FrozenProving => "FROZEN_PROVING", Completed => "COMPLETED", Superseded => "SUPERSEDED", Rejected => "REJECTED" });
wire_enum!(DecisionPriority { P0 => "P0", P1 => "P1", P2 => "P2" });
wire_enum!(DecisionCategory { BreakingDefChange => "breaking_def_change", StrategyBranchPruning => "strategy_branch_pruning", HardBottleneckEscalation => "hard_bottleneck_escalation", CrossEngineAdmission => "cross_engine_admission", OracleDebtWaiver => "oracle_debt_waiver" });
wire_enum!(DecisionStatus { Pending => "PENDING", Resolved => "RESOLVED", Dismissed => "DISMISSED" });
wire_enum!(PlanTrack { Exploration => "exploration", Infrastructure => "infrastructure" });
wire_enum!(PlanStatus { Active => "ACTIVE", Proved => "PROVED", SuspendedSoftPrune => "SUSPENDED_SOFT_PRUNE", InvalidatedByRefutation => "INVALIDATED_BY_REFUTATION", ArchivedByAdr => "ARCHIVED_BY_ADR" });
wire_enum!(DependencyKind { Lemma => "lemma", Definition => "definition", OraclePlaceholder => "oracle_placeholder" });

pub type PrimaryITP = Engine;
pub type TrustLevel = Trust;

impl From<Engine> for DispatchEngine {
    fn from(engine: Engine) -> Self {
        match engine {
            Engine::Lean4 => Self::Lean4,
            Engine::Agda => Self::Agda,
            Engine::Rocq => Self::Rocq,
        }
    }
}

impl TryFrom<DispatchEngine> for Engine {
    type Error = crate::ParseEnumError;
    fn try_from(engine: DispatchEngine) -> Result<Self, Self::Error> {
        engine.as_str().parse()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Budget {
    #[serde(deserialize_with = "checks::wall_limit")]
    pub max_wall_seconds: u64,
    #[serde(deserialize_with = "checks::positive")]
    pub max_turns: u64,
    #[serde(deserialize_with = "checks::positive")]
    pub max_broker_calls: u64,
    #[serde(deserialize_with = "checks::recursion_limit")]
    pub max_recursion_depth: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DispatchContract {
    #[serde(deserialize_with = "checks::nonempty")]
    pub task_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_task_id: Option<String>,
    pub role: Role,
    pub engine: DispatchEngine,
    #[serde(deserialize_with = "checks::nonempty")]
    pub claim_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rfc_id: Option<String>,
    #[serde(deserialize_with = "checks::commit")]
    pub base_commit: String,
    pub workspace_mode: WorkspaceMode,
    pub objective_mode: ObjectiveMode,
    #[serde(default)]
    pub frozen_targets: Vec<String>,
    #[serde(default)]
    pub relevant_wiki_ids: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    pub budget: Budget,
    #[serde(default)]
    pub early_escalate_if: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HarvestedLemma {
    pub name: String,
    pub engine: FormalEngine,
    pub verified_level: VerifiedLevel,
    pub formal_signature: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub informal_gloss: Option<String>,
    pub patch_ref: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResultContract {
    pub task_id: String,
    pub status: ResultStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub achieved_trust_level: Option<Trust>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub patch_ref: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence: Option<ResultEvidence>,
    #[serde(default)]
    pub harvested_lemmas: Vec<HarvestedLemma>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diagnosis: Option<Diagnosis>,
    pub usage: Usage,
    pub trace_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResultEvidence {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verification_receipt_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub receipt_sha256: Option<String>,
    #[serde(default)]
    pub axioms_used: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub counterexample_witness: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oracle_check_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hammer_probe_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reviewer_ticket_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Diagnosis {
    pub category: DiagnosisCategory,
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggested_next_action: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Usage {
    #[serde(deserialize_with = "checks::nonnegative")]
    pub wall_seconds: f64,
    pub turns: u64,
    pub broker_calls: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InfraRFC {
    pub rfc_id: String,
    pub requested_by_claim: String,
    pub engine: Engine,
    pub motivation: String,
    #[serde(deserialize_with = "checks::nonempty_vec")]
    pub proposed_definitions: Vec<String>,
    pub required_api_lemmas: Vec<String>,
    #[serde(deserialize_with = "checks::nonempty")]
    pub consumer_usage_sketch: String,
    #[serde(deserialize_with = "checks::nonempty_vec")]
    pub smoke_test_criteria: Vec<String>,
    pub status: InfraStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CrossEngineBridgeContract {
    pub bridge_id: String,
    pub source_engine: Engine,
    pub source_commit: String,
    pub source_decl: String,
    pub target_engine: Engine,
    pub target_axiom: String,
    pub target_signature: String,
    pub logic_assumptions: LogicAssumptions,
    pub reviewer_approved: bool,
    #[serde(deserialize_with = "checks::nonempty")]
    pub adr_ref: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogicAssumptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub classical: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uip_or_k: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub funext: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub propext: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaimNode {
    pub claim_id: String,
    pub title: String,
    pub informal_statement: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub formal_signature: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub formal_target: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub module_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub engine: Option<ClaimEngine>,
    #[serde(default)]
    pub is_critical_path: bool,
    pub local_trust_level: Trust,
    pub effective_trust_level: Trust,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prior_trust_level: Option<Trust>,
    pub stale: bool,
    #[serde(default)]
    pub effective_stale: bool,
    #[serde(default)]
    pub oracle_debt: bool,
    #[serde(default)]
    pub active_plan_ids: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bridge_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wiki_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub receipt_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionInboxItem {
    pub inbox_id: String,
    pub created_at: String,
    pub priority: DecisionPriority,
    pub category: DecisionCategory,
    #[serde(deserialize_with = "checks::nonnegative")]
    pub leverage_score: f64,
    #[serde(default)]
    pub related_claims: Vec<String>,
    pub summary: String,
    #[serde(default)]
    pub evidence_refs: Vec<String>,
    #[serde(deserialize_with = "checks::options")]
    pub options: Vec<DecisionOption>,
    pub status: DecisionStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_adr: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionOption {
    pub label: String,
    pub tradeoff: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recommended: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProofPlan {
    pub plan_id: String,
    pub target_claim_id: String,
    pub name: String,
    pub track: PlanTrack,
    pub status: PlanStatus,
    pub effective_trust_level: Trust,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rfc_id: Option<String>,
    pub dependencies: Vec<PlanDependency>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanDependency {
    pub dep_claim_id: String,
    pub dep_kind: DependencyKind,
}

mod checks {
    use super::DecisionOption;
    use serde::{Deserialize, Deserializer, de::Error};

    macro_rules! check {
        ($name:ident, $ty:ty, $pred:expr, $error:literal) => {
            pub fn $name<'de, D: Deserializer<'de>>(d: D) -> Result<$ty, D::Error> {
                let value = <$ty>::deserialize(d)?;
                if ($pred)(&value) {
                    Ok(value)
                } else {
                    Err(D::Error::custom($error))
                }
            }
        };
    }
    check!(
        wall_limit,
        u64,
        |n: &u64| *n >= 10,
        "must be at least 10 seconds"
    );
    check!(positive, u64, |n: &u64| *n > 0, "must be at least 1");
    check!(
        recursion_limit,
        u64,
        |n: &u64| *n <= 2,
        "recursion depth must not exceed 2"
    );
    check!(
        commit,
        String,
        |s: &String| s.chars().take(7).count() == 7,
        "base commit must contain at least 7 characters"
    );
    check!(
        nonempty,
        String,
        |s: &String| !s.is_empty(),
        "must not be empty"
    );
    check!(
        nonempty_vec,
        Vec<String>,
        |s: &Vec<String>| !s.is_empty(),
        "must contain at least one entry"
    );
    check!(
        nonnegative,
        f64,
        |n: &f64| n.is_finite() && *n >= 0.0,
        "must be finite and nonnegative"
    );
    check!(
        options,
        Vec<DecisionOption>,
        |v: &Vec<DecisionOption>| v.len() >= 2,
        "must contain at least two decision options"
    );
}
