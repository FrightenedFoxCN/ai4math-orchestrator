use super::{ReplayedClaim, ReplayedPlan, StateStore, claim_replay, ledger_replay, plan_replay};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

pub(super) const MAX_NODES: usize = 1024;
pub(super) const MAX_DEPENDENCY_EDGES: usize = 8192;

/// Ledger-only historical Claim and ProofPlan graph. Not a source of proof authority.
#[derive(Debug, Serialize)]
pub struct GraphReplay {
    pub through_event_seq: i64,
    pub ledger_head_event_seq: i64,
    pub applied_claim_events: u32,
    pub applied_plan_events: u32,
    /// Number of plan dependency records, including parallel dependencies on different plans.
    pub dependency_edge_count: usize,
    /// Sorted by Claim ID.
    pub claims: Vec<ReplayedClaim>,
    /// Sorted by ProofPlan ID.
    pub plans: Vec<ReplayedPlan>,
}

impl StateStore {
    /// Replay both projections through an inclusive event sequence, defaulting to the head
    /// of the same ledger snapshot. Do not read live projections or recompute trust.
    ///
    /// A cursor may separate events in the same write transaction: route blocks can precede
    /// Claim exhaustion, and effective trust can lag a local trust or route change. Cause
    /// references must name an already created Claim distinct from the affected Claim or
    /// plan target, but this is not a check of refutation or derived route/trust semantics.
    ///
    /// Reject invalid histories rather than returning a partial graph. A plan's target and
    /// dependencies must already exist at creation, and the DAG includes all plans regardless
    /// of status. In addition to the ledger reader's limits, at most 1024 Claims and ProofPlans
    /// combined and 8192 plan dependencies are allowed.
    pub fn replay_graph(&self, through_event_seq: Option<i64>) -> Result<GraphReplay> {
        let snapshot = self.db.unchecked_transaction()?;
        let graph = replay_graph_in_snapshot(&snapshot, through_event_seq)?;
        snapshot.commit()?;
        Ok(graph)
    }
}

/// Replay the ledger in the caller's transaction without ending the snapshot.
pub(super) fn replay_graph_in_snapshot(
    snapshot: &rusqlite::Transaction<'_>,
    through: Option<i64>,
) -> Result<GraphReplay> {
    let mut graph = Projection::default();
    let window = ledger_replay::replay_graph_rows_in_snapshot(
        snapshot,
        through,
        |seq, entity_type, id, kind, commit, payload| match entity_type {
            "claim" => graph.apply_claim(id, seq, kind, commit, payload),
            "proof_plan" => graph.apply_plan(id, seq, kind, payload),
            _ => bail!("unsupported graph replay entity type {entity_type}"),
        },
    )?;
    graph.ensure_dag()?;
    Ok(GraphReplay {
        through_event_seq: window.through_event_seq,
        ledger_head_event_seq: window.ledger_head_event_seq,
        applied_claim_events: graph.applied_claim_events,
        applied_plan_events: graph.applied_plan_events,
        dependency_edge_count: graph.dependency_edge_count,
        claims: graph.claims.into_values().collect(),
        plans: graph.plans.into_values().collect(),
    })
}

#[derive(Default)]
struct Projection {
    claims: BTreeMap<String, ReplayedClaim>,
    plans: BTreeMap<String, ReplayedPlan>,
    applied_claim_events: u32,
    applied_plan_events: u32,
    dependency_edge_count: usize,
}

#[derive(Deserialize)]
struct RouteExhaustionCause {
    refuted_claim: String,
}

impl Projection {
    fn ensure_node_budget(&self) -> Result<()> {
        ensure!(
            self.claims.len() + self.plans.len() < MAX_NODES,
            "graph replay exceeds the {MAX_NODES}-node cap on Claims and ProofPlans"
        );
        Ok(())
    }

    fn require_claim_reference(&self, id: &str, role: &str) -> Result<()> {
        ensure!(
            self.claims.contains_key(id),
            "missing preceding CLAIM_CREATED for {role} Claim {id:?}"
        );
        Ok(())
    }

    fn require_cause(&self, id: &str, target: &str, role: &str) -> Result<()> {
        ensure!(
            id != target,
            "invalid {role} Claim {id:?}: it is the affected Claim"
        );
        self.require_claim_reference(id, role)
    }

    fn apply_claim(
        &mut self,
        id: &str,
        seq: i64,
        kind: &str,
        commit: Option<&str>,
        payload: &str,
    ) -> Result<()> {
        if kind == "CLAIM_CREATED" && !self.claims.contains_key(id) {
            self.ensure_node_budget()?;
        }
        let (key, mut state) = take_state(&mut self.claims, id);
        claim_replay::apply(&mut state, id, seq, kind, commit, payload)?;
        if kind == "CLAIM_ROUTES_EXHAUSTED" {
            // The exact reducer validates the payload; it does not retain the cause ID.
            let cause: RouteExhaustionCause = serde_json::from_str(payload)?;
            self.require_cause(&cause.refuted_claim, id, "route exhaustion cause")?;
        }
        self.claims
            .insert(key, state.context("Claim reducer returned no state")?);
        self.applied_claim_events += 1;
        Ok(())
    }

    fn apply_plan(&mut self, id: &str, seq: i64, kind: &str, payload: &str) -> Result<()> {
        if kind == "PLAN_CREATED" && !self.plans.contains_key(id) {
            self.ensure_node_budget()?;
        }
        let (key, mut state) = take_state(&mut self.plans, id);
        plan_replay::apply(&mut state, id, seq, kind, payload)?;
        let plan = state.context("ProofPlan reducer returned no state")?;
        match kind {
            "PLAN_CREATED" => {
                let edge_count = self.dependency_edge_count + plan.dependencies.len();
                ensure!(
                    edge_count <= MAX_DEPENDENCY_EDGES,
                    "graph replay exceeds the {MAX_DEPENDENCY_EDGES}-edge cap on plan dependencies"
                );
                self.require_claim_reference(&plan.target_claim_id, "target")?;
                for dep in &plan.dependencies {
                    self.require_claim_reference(&dep.claim_id, "dependency")?;
                }
                self.dependency_edge_count = edge_count;
            }
            "PLAN_ROUTE_BLOCKED" => self.require_cause(
                plan.blocking_claim_id
                    .as_deref()
                    .context("missing ProofPlan blocking cause")?,
                &plan.target_claim_id,
                "blocking cause",
            )?,
            // The exact reducer already requires a direct dependency, whose existence was
            // checked at PLAN_CREATED. Claim IDs cannot disappear during replay.
            _ => {}
        }
        self.plans.insert(key, plan);
        self.applied_plan_events += 1;
        Ok(())
    }

    fn ensure_dag(&self) -> Result<()> {
        // Use the Claim-only graph: each plan dependency gives dep -> target. Parallel
        // edges are counted and removed separately; neither plan nor Claim status matters.
        let indices: HashMap<&str, usize> = self
            .claims
            .keys()
            .enumerate()
            .map(|(index, id)| (id.as_str(), index))
            .collect();
        let mut incoming = vec![0; indices.len()];
        let mut outgoing = vec![Vec::new(); indices.len()];
        for plan in self.plans.values() {
            let target = indices[plan.target_claim_id.as_str()];
            incoming[target] += plan.dependencies.len();
            for dep in &plan.dependencies {
                outgoing[indices[dep.claim_id.as_str()]].push(target);
            }
        }
        let mut ready: Vec<_> = incoming
            .iter()
            .enumerate()
            .filter_map(|(index, &degree)| (degree == 0).then_some(index))
            .collect();
        let mut visited = 0;
        while let Some(node) = ready.pop() {
            visited += 1;
            for &next in &outgoing[node] {
                incoming[next] -= 1;
                if incoming[next] == 0 {
                    ready.push(next);
                }
            }
        }
        ensure!(
            visited == indices.len(),
            "dependency graph contains a cycle"
        );
        Ok(())
    }
}

// Reducers take an Option to handle creation and transitions uniformly. Temporarily move
// an existing entry out without cloning the projection or its key; on failure the whole
// reconstruction is discarded.
fn take_state<T>(states: &mut BTreeMap<String, T>, id: &str) -> (String, Option<T>) {
    match states.remove_entry(id) {
        Some((key, state)) => (key, Some(state)),
        None => (id.to_owned(), None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Change, NewClaim};

    fn create_claim(store: &mut StateStore, id: &str) -> Result<()> {
        store.create_claim(
            NewClaim {
                id,
                title: id,
                informal_statement: "Snapshot test claim",
                engine: "lean4",
                formal_target: None,
                module_path: None,
                critical: false,
            },
            Change {
                actor: "snapshot-test",
                git_commit: None,
            },
        )
    }

    #[test]
    fn borrowed_graph_replay_keeps_the_snapshot_for_live_reads() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("state.sqlite");
        let mut writer = StateStore::open(&path)?;
        create_claim(&mut writer, "first")?;
        let reader = StateStore::open_readonly(&path)?;
        let changes_before = reader.db.total_changes();
        let snapshot = reader.db.unchecked_transaction()?;
        let before = replay_graph_in_snapshot(&snapshot, None)?;
        assert_eq!(before.claims.len(), 1);
        assert_eq!(before.claims[0].claim_id, "first");
        assert!(!snapshot.is_autocommit());

        create_claim(&mut writer, "second")?;
        writer.db.execute(
            "INSERT INTO events(occurred_at,event_type,entity_type,entity_id,actor)
             VALUES ('timestamp','TASK_CREATED','task','task','test')",
            [],
        )?;
        let newer_head = writer.db.last_insert_rowid();

        let still_pinned = replay_graph_in_snapshot(&snapshot, None)?;
        assert_eq!(
            still_pinned.ledger_head_event_seq,
            before.ledger_head_event_seq
        );
        assert_eq!(still_pinned.through_event_seq, before.ledger_head_event_seq);
        assert_eq!(still_pinned.claims.len(), 1);
        let live_claims: i64 =
            snapshot.query_row("SELECT COUNT(*) FROM claims", [], |r| r.get(0))?;
        assert_eq!(live_claims, 1);
        snapshot.commit()?;

        let fresh = reader.replay_graph(None)?;
        assert_eq!(fresh.ledger_head_event_seq, newer_head);
        assert_eq!(fresh.through_event_seq, newer_head);
        assert_eq!(fresh.applied_claim_events, 2);
        assert_eq!(fresh.claims.len(), 2);
        assert!(reader.db.is_autocommit());
        assert_eq!(reader.db.total_changes(), changes_before);
        Ok(())
    }
}
