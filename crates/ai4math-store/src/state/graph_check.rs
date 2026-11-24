use super::{Dag, GraphReplay, ReplayedPlan, StateStore, Trust};
use ai4math_core::{PlanStatus, aggregate_plan, best_proof_plan};
use anyhow::Result;
use serde::Serialize;
use std::collections::BTreeMap;

const MAX_MISMATCHES: usize = 128;
const ACTIVE: &str = PlanStatus::Active.as_str();
const PROVED: &str = PlanStatus::Proved.as_str();
const INVALIDATED: &str = PlanStatus::InvalidatedByRefutation.as_str();
const ARCHIVED: &str = PlanStatus::ArchivedByAdr.as_str();

/// Consistency of graph-derived fields in the ledger at one committed head.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct GraphSemanticCheck {
    pub ledger_event_seq: i64,
    pub consistent: bool,
    /// Exact count, including mismatches omitted from the report.
    pub mismatch_count: usize,
    pub truncated: bool,
    /// First 128 mismatches, ordered by entity type, ID, then field.
    pub mismatches: Vec<GraphSemanticMismatch>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct GraphSemanticMismatch {
    pub entity_type: &'static str,
    pub entity_id: String,
    pub field: &'static str,
}

impl StateStore {
    /// Independently recompute AND–OR graph semantics at the head of bounded ledger replay.
    /// Malformed or out-of-budget histories fail rather than returning a partial report.
    /// This does not consult live graph projections, authenticate evidence, or change state.
    pub fn check_graph(&self) -> Result<GraphSemanticCheck> {
        check_replay(&self.replay_graph(None)?)
    }
}

struct DerivedClaim<'a> {
    trust: Trust,
    stale: bool,
    exhausted_by: Option<&'a str>,
}

fn check_replay(replay: &GraphReplay) -> Result<GraphSemanticCheck> {
    let claims: BTreeMap<_, _> = replay
        .claims
        .iter()
        .map(|claim| (claim.claim_id.as_str(), claim))
        .collect();
    let mut plans: BTreeMap<&str, Vec<&ReplayedPlan>> = BTreeMap::new();
    let mut dag = Dag::default();
    for id in claims.keys() {
        dag.add_node(id);
    }
    for plan in &replay.plans {
        plans.entry(&plan.target_claim_id).or_default().push(plan);
        for dep in &plan.dependencies {
            dag.add_edge(&dep.claim_id, &plan.target_claim_id);
        }
    }

    let mut derived: BTreeMap<&str, DerivedClaim<'_>> = BTreeMap::new();
    // Replay bounds the graph, hence also the number of possible field mismatches.
    // Keep identifiers borrowed until the lexicographic output prefix is known.
    let mut mismatches = Vec::new();
    for id in dag.ordered()? {
        let claim = claims[id.as_str()];
        let id = claim.claim_id.as_str();
        let mut stale = claim.stale;
        let mut scores = Vec::new();
        let mut open_route = false;
        let mut exhaustion_root = None;
        let routes = plans.get(id).map_or(&[][..], Vec::as_slice);
        for plan in routes {
            let plan_id = plan.plan_id.as_str();
            let status = plan.durable_status;
            let mut root = None;
            let mut direct_refutation = false;
            let mut valid_invalidation = false;
            let mut dependency_stale = false;
            for dep in &plan.dependencies {
                let dep_id = dep.claim_id.as_str();
                let dependency = &derived[dep_id];
                dependency_stale |= dependency.stale;
                if claims[dep_id].local_trust.is_hard_refutation() {
                    direct_refutation = true;
                    // Historical invalidation need not name today's lexical minimum.
                    valid_invalidation |= plan.refuted_claim_id.as_deref() == Some(dep_id);
                    take_min(&mut root, Some(dep_id));
                } else {
                    take_min(&mut root, dependency.exhausted_by);
                }
            }
            if (status == INVALIDATED && !valid_invalidation)
                || (status != INVALIDATED && status != ARCHIVED && direct_refutation)
            {
                mismatches.push(("proof_plan", plan_id, "durable_status"));
            }

            // A direct dependency refutation requires durable invalidation, not a reversible
            // overlay, even if the ledger omitted the invalidation event. For a recorded but
            // invalidly justified invalidation, report the durable field without inventing a
            // replacement durable status.
            let blocker = if status == ARCHIVED || status == INVALIDATED || direct_refutation {
                None
            } else {
                root
            };
            if blocker != plan.blocking_claim_id.as_deref() {
                mismatches.push(("proof_plan", plan_id, "blocking_claim_id"));
            }
            // Archived routes count as plans for trust fallback, but not for exhaustion.
            if status != ARCHIVED {
                open_route |= root.is_none();
                take_min(&mut exhaustion_root, root);
            }

            let score = if matches!(status, ACTIVE | PROVED)
                && root.is_none()
                && claim.local_trust.proof_rank().is_some()
            {
                // Stale is independent of whether this eligible route yields proof credit.
                stale |= dependency_stale;
                aggregate_plan(
                    claim.local_trust,
                    plan.dependencies
                        .iter()
                        .map(|dep| derived[dep.claim_id.as_str()].trust),
                )
            } else {
                None
            };
            if score.unwrap_or(Trust::L0) != plan.effective_trust {
                mismatches.push(("proof_plan", plan_id, "effective_trust"));
            }
            scores.extend(score);
        }

        let trust = if routes.is_empty() || claim.local_trust.proof_rank().is_none() {
            claim.local_trust
        } else {
            best_proof_plan(scores).unwrap_or(Trust::L0)
        };
        let exhausted_by =
            exhaustion_root.filter(|_| !open_route && !claim.local_trust.is_hard_refutation());
        if exhausted_by.is_some() != claim.routes_exhausted {
            mismatches.push(("claim", id, "routes_exhausted"));
        }
        if trust != claim.effective_trust {
            mismatches.push(("claim", id, "effective_trust"));
        }
        if stale != claim.effective_stale {
            mismatches.push(("claim", id, "effective_stale"));
        }
        derived.insert(
            id,
            DerivedClaim {
                trust,
                stale,
                exhausted_by,
            },
        );
    }

    mismatches.sort_unstable();
    let mismatch_count = mismatches.len();
    Ok(GraphSemanticCheck {
        ledger_event_seq: replay.ledger_head_event_seq,
        consistent: mismatch_count == 0,
        mismatch_count,
        truncated: mismatch_count > MAX_MISMATCHES,
        mismatches: mismatches
            .into_iter()
            .take(MAX_MISMATCHES)
            .map(|(entity_type, id, field)| GraphSemanticMismatch {
                entity_type,
                entity_id: id.to_owned(),
                field,
            })
            .collect(),
    })
}

fn take_min<'a>(current: &mut Option<&'a str>, candidate: Option<&'a str>) {
    if let Some(candidate) = candidate
        && current.is_none_or(|previous| candidate < previous)
    {
        *current = Some(candidate);
    }
}
