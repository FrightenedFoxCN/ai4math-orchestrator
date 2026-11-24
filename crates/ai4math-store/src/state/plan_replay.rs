use super::{StateStore, Trust, ledger_replay::replay_entity};
use ai4math_core::{DependencyKind, PlanStatus, PlanTrack};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

const BLOCKED: &str = "BLOCKED_BY_REFUTATION";
const INVALIDATED: &str = PlanStatus::InvalidatedByRefutation.as_str();
const ARCHIVED: &str = PlanStatus::ArchivedByAdr.as_str();

/// Historical ProofPlan projection from one ledger snapshot, not proof authority.
#[derive(Debug, Serialize)]
pub struct PlanReplay {
    pub plan_id: String,
    pub through_event_seq: i64,
    pub ledger_head_event_seq: i64,
    /// Number of ProofPlan state events applied, excluding unrelated events.
    pub applied_events: u32,
    pub plan: Option<ReplayedPlan>,
}

#[derive(Debug, Serialize)]
pub struct ReplayedPlan {
    pub plan_id: String,
    pub target_claim_id: String,
    pub name: String,
    pub track: PlanTrack,
    /// Creation order as recorded in the event, not order in the graph.
    pub dependencies: Vec<ReplayedPlanDependency>,
    pub durable_status: &'static str,
    /// The route-block overlay takes precedence until its explicit unblock event.
    pub status: &'static str,
    pub effective_trust: Trust,
    pub blocking_claim_id: Option<String>,
    /// Direct dependency recorded by invalidation, distinct from a route blocker.
    pub refuted_claim_id: Option<String>,
    /// ADR ID recorded by PLAN_ARCHIVED; not independently authenticated by replay.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archived_by_adr: Option<String>,
    pub last_event_seq: i64,
}

#[derive(Debug, Serialize)]
pub struct ReplayedPlanDependency {
    pub claim_id: String,
    pub kind: DependencyKind,
}

impl StateStore {
    /// Replay one ProofPlan through an inclusive sequence or the snapshot head if omitted.
    /// A cursor can separate events from the same write transaction. A missing plan or
    /// cursor before creation yields no plan, not a live projection lookup.
    ///
    /// Rejects malformed or inconsistent ProofPlan histories within the shared bounded
    /// ledger reader. Does not read live projections, authenticate ADRs, or recompute
    /// authority from peer plans or claims.
    pub fn replay_plan(&self, plan_id: &str, through_event_seq: Option<i64>) -> Result<PlanReplay> {
        let mut plan = None;
        let window = replay_entity(
            &self.db,
            "proof_plan",
            plan_id,
            through_event_seq,
            "PLAN_",
            None,
            |seq, kind, _git_commit, payload| apply(&mut plan, plan_id, seq, kind, payload),
        )?;
        Ok(PlanReplay {
            plan_id: plan_id.to_owned(),
            through_event_seq: window.through_event_seq,
            ledger_head_event_seq: window.ledger_head_event_seq,
            applied_events: window.applied_events,
            plan,
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Created {
    target: String,
    name: String,
    track: PlanTrack,
    status: PlanStatus,
    dependencies: Vec<(String, DependencyKind)>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TrustChanged {
    previous: Trust,
    effective: Trust,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Invalidated {
    refuted_claim: String,
    target: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RouteBlocked {
    refuted_claim: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RouteUnblocked {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Archived {
    target: String,
    adr_id: String,
}

pub(super) fn apply(
    state: &mut Option<ReplayedPlan>,
    id: &str,
    seq: i64,
    kind: &str,
    payload: &str,
) -> Result<()> {
    ensure!(seq > 0, "ProofPlan event sequence must be positive");
    // Serde structs also accept positional arrays; the event envelope is a JSON object.
    ensure!(
        payload.trim_start().starts_with('{'),
        "expected ProofPlan event payload object"
    );
    if kind == "PLAN_CREATED" {
        ensure!(state.is_none(), "duplicate PLAN_CREATED");
        let p: Created = serde_json::from_str(payload)?;
        ensure!(
            matches!(p.status, PlanStatus::Active | PlanStatus::Proved),
            "new ProofPlan must be active or proved"
        );
        let mut dependencies = HashSet::with_capacity(p.dependencies.len());
        for (claim_id, _) in &p.dependencies {
            ensure!(
                claim_id != &p.target,
                "ProofPlan cannot depend on its target"
            );
            ensure!(
                dependencies.insert(claim_id),
                "duplicate ProofPlan dependency"
            );
        }
        let status = p.status.as_str();
        *state = Some(ReplayedPlan {
            plan_id: id.to_owned(),
            target_claim_id: p.target,
            name: p.name,
            track: p.track,
            dependencies: p
                .dependencies
                .into_iter()
                .map(|(claim_id, kind)| ReplayedPlanDependency { claim_id, kind })
                .collect(),
            durable_status: status,
            status,
            effective_trust: Trust::L0,
            blocking_claim_id: None,
            refuted_claim_id: None,
            archived_by_adr: None,
            last_event_seq: seq,
        });
        return Ok(());
    }
    let plan = state.as_mut().context("missing preceding PLAN_CREATED")?;
    ensure!(
        plan.durable_status != ARCHIVED,
        "archived ProofPlan cannot transition"
    );
    let live = plan.durable_status != INVALIDATED;
    match kind {
        "PLAN_TRUST_CHANGED" => {
            let p: TrustChanged = serde_json::from_str(payload)?;
            ensure!(live, "only a live ProofPlan can change trust");
            ensure!(
                p.previous == plan.effective_trust && p.effective != p.previous,
                "illegal ProofPlan trust predecessor"
            );
            ensure!(
                p.effective.proof_rank().is_some(),
                "invalid ProofPlan trust"
            );
            ensure!(
                plan.blocking_claim_id.is_none() || p.effective == Trust::L0,
                "blocked ProofPlan trust can only reset to L0"
            );
            plan.effective_trust = p.effective;
        }
        "PLAN_INVALIDATED" => {
            let p: Invalidated = serde_json::from_str(payload)?;
            ensure!(live, "only a live ProofPlan can be invalidated");
            ensure!(p.target == plan.target_claim_id, "wrong ProofPlan target");
            ensure!(
                plan.dependencies
                    .iter()
                    .any(|d| d.claim_id == p.refuted_claim),
                "invalidation cause is not a direct ProofPlan dependency"
            );
            plan.durable_status = INVALIDATED;
            // Invalidation's writer resets trust itself; there is no companion trust event.
            plan.effective_trust = Trust::L0;
            plan.refuted_claim_id = Some(p.refuted_claim);
        }
        "PLAN_ARCHIVED" => {
            let p: Archived = serde_json::from_str(payload)?;
            ensure!(
                plan.durable_status == PlanStatus::Active.as_str()
                    && plan.blocking_claim_id.is_none(),
                "only an active, unblocked ProofPlan can be archived"
            );
            ensure!(p.target == plan.target_claim_id, "wrong ProofPlan target");
            ensure!(
                p.adr_id.strip_prefix("ADR-PIVOT-").is_some_and(|hash| {
                    hash.len() == 64
                        && hash
                            .bytes()
                            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                }),
                "invalid ProofPlan archival ADR ID"
            );
            plan.durable_status = ARCHIVED;
            plan.archived_by_adr = Some(p.adr_id);
            // Archive resets effective trust without a companion trust event.
            plan.effective_trust = Trust::L0;
        }
        "PLAN_ROUTE_BLOCKED" => {
            let p: RouteBlocked = serde_json::from_str(payload)?;
            ensure!(live, "only a live ProofPlan can be route blocked");
            ensure!(
                plan.blocking_claim_id.as_deref() != Some(p.refuted_claim.as_str()),
                "ProofPlan route block cause did not change"
            );
            plan.blocking_claim_id = Some(p.refuted_claim);
        }
        "PLAN_ROUTE_UNBLOCKED" => {
            let _: RouteUnblocked = serde_json::from_str(payload)?;
            ensure!(
                plan.blocking_claim_id.is_some(),
                "ProofPlan route was not blocked"
            );
            // Route refresh can emit this echo after the same route was directly invalidated.
            plan.blocking_claim_id = None;
        }
        _ => bail!("unsupported ProofPlan state event {kind}"),
    }
    plan.status = if plan.blocking_claim_id.is_some() {
        BLOCKED
    } else {
        plan.durable_status
    };
    plan.last_event_seq = seq;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const CREATE: &str = r#"{"target":"target","name":"route","track":"exploration","status":"PROVED","dependencies":[["z-dep","lemma"],["a-dep","definition"]]}"#;
    const BLOCK: &str = r#"{"refuted_claim":"root"}"#;
    const INVALIDATE: &str = r#"{"refuted_claim":"z-dep","target":"target"}"#;

    #[test]
    fn creation_requires_exact_writer_shape() -> Result<()> {
        for malformed in [
            CREATE.replace("PROVED", "SUSPENDED_SOFT_PRUNE"),
            CREATE.replace("exploration", "unknown"),
            CREATE.replace("definition", "unknown"),
            CREATE.replace("a-dep", "z-dep"),
            CREATE.replace("a-dep", "target"),
            CREATE.replace("\"definition\"", "\"definition\", \"extra\""),
            CREATE.replace("\"name\":\"route\",", ""),
            CREATE.replace(
                "\"name\":\"route\",",
                "\"name\":\"route\",\"name\":\"duplicate\",",
            ),
            r#"["target","route","exploration","PROVED",[]]"#.to_owned(),
        ] {
            assert!(
                apply(&mut None, "plan", 1, "PLAN_CREATED", &malformed).is_err(),
                "accepted {malformed}"
            );
        }
        let mut state = None;
        apply(&mut state, "plan", 1, "PLAN_CREATED", CREATE)?;
        assert!(apply(&mut state, "plan", 2, "PLAN_CREATED", CREATE).is_err());
        let plan = state.unwrap();
        assert_eq!(plan.dependencies[0].claim_id, "z-dep");
        assert_eq!(plan.dependencies[1].claim_id, "a-dep");
        assert_eq!(plan.effective_trust, Trust::L0);
        Ok(())
    }

    #[test]
    fn invalidation_and_overlay_are_independent_at_event_boundaries() -> Result<()> {
        let mut state = None;
        apply(&mut state, "plan", 1, "PLAN_CREATED", CREATE)?;
        apply(
            &mut state,
            "plan",
            2,
            "PLAN_TRUST_CHANGED",
            r#"{"previous":"L0","effective":"L3"}"#,
        )?;
        apply(&mut state, "plan", 3, "PLAN_ROUTE_BLOCKED", BLOCK)?;
        let plan = state.as_ref().unwrap();
        assert_eq!(plan.status, BLOCKED);
        assert_eq!(plan.effective_trust, Trust::L3);
        apply(
            &mut state,
            "plan",
            4,
            "PLAN_ROUTE_BLOCKED",
            r#"{"refuted_claim":"earlier-root"}"#,
        )?;
        apply(&mut state, "plan", 5, "PLAN_INVALIDATED", INVALIDATE)?;
        let plan = state.as_ref().unwrap();
        assert_eq!(plan.status, BLOCKED);
        assert_eq!(plan.durable_status, INVALIDATED);
        assert_eq!(plan.effective_trust, Trust::L0);
        assert_eq!(plan.blocking_claim_id.as_deref(), Some("earlier-root"));
        assert_eq!(plan.refuted_claim_id.as_deref(), Some("z-dep"));
        apply(&mut state, "plan", 6, "PLAN_ROUTE_UNBLOCKED", "{}")?;
        let plan = state.unwrap();
        assert_eq!(plan.status, INVALIDATED);
        assert_eq!(plan.blocking_claim_id, None);
        assert_eq!(plan.last_event_seq, 6);
        Ok(())
    }

    #[test]
    fn archive_is_exact_active_only_and_terminal() -> Result<()> {
        let adr_id = format!("ADR-PIVOT-{}", "a".repeat(64));
        let archive = format!(r#"{{"target":"target","adr_id":"{adr_id}"}}"#);
        let active = CREATE.replace("PROVED", "ACTIVE");
        let mut state = None;
        assert!(apply(&mut state, "plan", 1, "PLAN_ARCHIVED", &archive).is_err());
        apply(&mut state, "plan", 1, "PLAN_CREATED", &active)?;
        apply(
            &mut state,
            "plan",
            2,
            "PLAN_TRUST_CHANGED",
            r#"{"previous":"L0","effective":"L3"}"#,
        )?;
        for malformed in [
            archive.replace(r#""target":"target""#, r#""target":"other""#),
            archive.replace(r#""target":"target","#, ""),
            archive.replace(
                r#""target":"target","#,
                r#""target":"target","target":"target","#,
            ),
            archive.replace(&adr_id, "ADR-PIVOT-123"),
            archive.replace(&adr_id, &"f".repeat(64)),
            archive.replace(&adr_id, &format!("ADR-PIVOT-{}", "A".repeat(64))),
            archive.replace(&adr_id, &format!("ADR-PIVOT-{}", "é".repeat(32))),
            format!(r#"{{"target":"target","adr_id":"{adr_id}","extra":null}}"#),
            format!(r#"["target","{adr_id}"]"#),
        ] {
            assert!(
                apply(&mut state, "plan", 3, "PLAN_ARCHIVED", &malformed).is_err(),
                "accepted {malformed}"
            );
        }
        let live = state.as_ref().unwrap();
        assert_eq!(live.last_event_seq, 2);
        assert_eq!(live.effective_trust, Trust::L3);
        assert_eq!(live.durable_status, "ACTIVE");
        assert!(serde_json::to_value(live)?.get("archived_by_adr").is_none());

        apply(&mut state, "plan", 3, "PLAN_ROUTE_BLOCKED", BLOCK)?;
        assert!(apply(&mut state, "plan", 4, "PLAN_ARCHIVED", &archive).is_err());
        apply(&mut state, "plan", 4, "PLAN_ROUTE_UNBLOCKED", "{}")?;
        apply(&mut state, "plan", 5, "PLAN_ARCHIVED", &archive)?;
        for (kind, payload) in [
            ("PLAN_ARCHIVED", archive.as_str()),
            ("PLAN_ROUTE_BLOCKED", BLOCK),
            ("PLAN_ROUTE_UNBLOCKED", "{}"),
            ("PLAN_INVALIDATED", INVALIDATE),
            (
                "PLAN_TRUST_CHANGED",
                r#"{"previous":"L0","effective":"L1"}"#,
            ),
        ] {
            assert!(
                apply(&mut state, "plan", 6, kind, payload).is_err(),
                "accepted {kind}"
            );
        }
        let archived = state.unwrap();
        assert_eq!(archived.last_event_seq, 5);
        assert_eq!(archived.status, ARCHIVED);
        assert_eq!(archived.durable_status, ARCHIVED);
        assert_eq!(archived.archived_by_adr.as_deref(), Some(adr_id.as_str()));
        assert_eq!(archived.effective_trust, Trust::L0);
        assert_eq!(archived.dependencies.len(), 2);
        assert_eq!(archived.blocking_claim_id, None);
        assert_eq!(archived.refuted_claim_id, None);

        let mut proved = None;
        apply(&mut proved, "plan", 1, "PLAN_CREATED", CREATE)?;
        assert!(apply(&mut proved, "plan", 2, "PLAN_ARCHIVED", &archive).is_err());
        let mut invalidated = None;
        apply(&mut invalidated, "plan", 1, "PLAN_CREATED", &active)?;
        apply(&mut invalidated, "plan", 2, "PLAN_INVALIDATED", INVALIDATE)?;
        assert!(apply(&mut invalidated, "plan", 3, "PLAN_ARCHIVED", &archive).is_err());
        Ok(())
    }

    #[test]
    fn malformed_transitions_do_not_mutate_state() -> Result<()> {
        let mut state = None;
        assert!(apply(&mut state, "plan", 1, "PLAN_ROUTE_BLOCKED", BLOCK).is_err());
        apply(&mut state, "plan", 1, "PLAN_CREATED", CREATE)?;
        let assert_err = |state: &mut Option<ReplayedPlan>, kind, payload| {
            let previous = state.as_ref().unwrap().last_event_seq;
            assert!(apply(state, "plan", previous + 1, kind, payload).is_err());
            assert_eq!(state.as_ref().unwrap().last_event_seq, previous);
        };
        for payload in [
            r#"{"previous":"L1","effective":"L3"}"#,
            r#"{"previous":"L0","effective":"L0"}"#,
            r#"{"previous":"L0","effective":"L2b"}"#,
        ] {
            assert_err(&mut state, "PLAN_TRUST_CHANGED", payload);
        }
        assert_err(&mut state, "PLAN_FUTURE_EVENT", "{}");
        assert_err(&mut state, "PLAN_ROUTE_UNBLOCKED", "{}");
        assert_err(&mut state, "PLAN_INVALIDATED", BLOCK);
        assert_err(
            &mut state,
            "PLAN_INVALIDATED",
            r#"{"refuted_claim":"root","target":"target"}"#,
        );
        assert_err(
            &mut state,
            "PLAN_INVALIDATED",
            r#"{"refuted_claim":"z-dep","target":"wrong"}"#,
        );
        apply(&mut state, "plan", 2, "PLAN_ROUTE_BLOCKED", BLOCK)?;
        assert_err(&mut state, "PLAN_ROUTE_BLOCKED", BLOCK);
        assert_err(&mut state, "PLAN_ROUTE_UNBLOCKED", r#"{"unexpected":true}"#);
        apply(&mut state, "plan", 3, "PLAN_INVALIDATED", INVALIDATE)?;
        assert_err(&mut state, "PLAN_ROUTE_BLOCKED", BLOCK);
        assert_err(&mut state, "PLAN_INVALIDATED", INVALIDATE);
        assert_err(
            &mut state,
            "PLAN_TRUST_CHANGED",
            r#"{"previous":"L0","effective":"L1"}"#,
        );
        Ok(())
    }
}
