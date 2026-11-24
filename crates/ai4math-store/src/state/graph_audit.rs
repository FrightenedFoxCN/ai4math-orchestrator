use super::{
    GraphReplay, StateStore,
    graph_replay::{MAX_DEPENDENCY_EDGES, MAX_NODES, replay_graph_in_snapshot},
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Row, Transaction, types::ValueRef};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

const MAX_MISMATCHES: usize = 128;
const MAX_LIVE_BYTES: usize = 16 * 1024 * 1024;
const MAX_LIVE_ID_BYTES: usize = 4096;

/// Comparison of materialized graph fields against the ledger projection at one snapshot head.
#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct GraphAudit {
    pub ledger_event_seq: i64,
    pub consistent: bool,
    /// Exact count, including mismatches omitted from the report.
    pub mismatch_count: usize,
    pub truncated: bool,
    /// First 128 mismatches, ordered by entity type, ID, then field.
    pub mismatches: Vec<GraphMismatch>,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct GraphMismatch {
    pub entity_type: &'static str,
    pub entity_id: String,
    /// Projected field name, or `presence` for an entire missing or extra node.
    pub field: &'static str,
    pub kind: GraphMismatchKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GraphMismatchKind {
    FieldMismatch,
    /// Present in the ledger projection but absent live.
    MissingProjection,
    /// Present live but absent from the ledger projection.
    UnexpectedProjection,
}

impl StateStore {
    /// Compare event-projected fields and live graph rows in one read transaction.
    /// Malformed replay or out-of-budget reads fail rather than returning a partial audit.
    /// This neither repairs live state nor independently recomputes AND–OR graph semantics.
    /// Only Claim exhaustion presence and the Plan route blocker ID are compared: the Claim
    /// exhaustion cause and the Plan's direct refutation ID are not represented in both views.
    pub fn audit_graph(&self) -> Result<GraphAudit> {
        let tx = self.db.unchecked_transaction()?;
        let replay = replay_graph_in_snapshot(&tx, None)?;
        let report = compare_graph(&tx, &replay)?;
        tx.commit()?;
        Ok(report)
    }
}

struct LiveClaim {
    title: String,
    engine: String,
    informal_statement: String,
    formal_target: Option<String>,
    module_path: Option<String>,
    critical: i64,
    local_trust: String,
    effective_trust: String,
    stale: i64,
    effective_stale: i64,
    prior_trust: Option<String>,
    taint_root_commit: Option<String>,
}

struct LivePlan {
    target_claim_id: String,
    name: String,
    track: String,
    durable_status: String,
    effective_trust: String,
}

fn compare_graph(tx: &Transaction<'_>, replay: &GraphReplay) -> Result<GraphAudit> {
    use GraphMismatchKind::{FieldMismatch, MissingProjection, UnexpectedProjection};

    let mut budget = LiveReadBudget {
        stored: MAX_LIVE_BYTES,
        decoded: MAX_LIVE_BYTES,
    };
    let mut claims = BTreeMap::new();
    let claim_rows = budget.read(
        tx,
        "claims",
        &[
            "claim_id",
            "title",
            "engine",
            "informal_statement",
            "formal_target",
            "module_path",
            "is_critical_path",
            "local_trust_level",
            "effective_trust_level",
            "stale",
            "effective_stale",
            "prior_trust_level",
            "taint_root_commit",
        ],
        MAX_NODES,
        "node",
        |r| {
            claims.insert(
                r.get::<_, String>(0)?,
                LiveClaim {
                    title: r.get(1)?,
                    engine: r.get(2)?,
                    informal_statement: r.get(3)?,
                    formal_target: r.get(4)?,
                    module_path: r.get(5)?,
                    critical: r.get(6)?,
                    local_trust: r.get(7)?,
                    effective_trust: r.get(8)?,
                    stale: r.get(9)?,
                    effective_stale: r.get(10)?,
                    prior_trust: r.get(11)?,
                    taint_root_commit: r.get(12)?,
                },
            );
            Ok(())
        },
    )?;
    let mut plans = BTreeMap::new();
    budget.read(
        tx,
        "proof_plans",
        &[
            "plan_id",
            "target_claim_id",
            "name",
            "track",
            "status",
            "effective_trust_level",
        ],
        MAX_NODES - claim_rows,
        "node",
        |r| {
            plans.insert(
                r.get::<_, String>(0)?,
                LivePlan {
                    target_claim_id: r.get(1)?,
                    name: r.get(2)?,
                    track: r.get(3)?,
                    durable_status: r.get(4)?,
                    effective_trust: r.get(5)?,
                },
            );
            Ok(())
        },
    )?;
    let mut dependencies: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    budget.read(
        tx,
        "plan_dependencies",
        &["plan_id", "dep_claim_id", "dep_kind"],
        MAX_DEPENDENCY_EDGES,
        "edge",
        |r| {
            dependencies
                .entry(r.get(0)?)
                .or_default()
                .insert(r.get(1)?, r.get(2)?);
            Ok(())
        },
    )?;
    // Scan overlays separately so orphan owners (e.g. after a write with foreign keys
    // disabled) are not hidden behind joins from the base tables.
    let mut blockers = BTreeMap::<String, String>::new();
    budget.read(
        tx,
        "proof_plan_route_blocks",
        &["plan_id", "refuted_claim_id"],
        MAX_NODES,
        "overlay",
        |r| {
            blockers.insert(r.get(0)?, r.get(1)?);
            Ok(())
        },
    )?;
    let mut exhausted = BTreeSet::<String>::new();
    budget.read(
        tx,
        "claim_route_exhaustion",
        &["claim_id"],
        MAX_NODES,
        "overlay",
        |r| {
            exhausted.insert(r.get(0)?);
            Ok(())
        },
    )?;

    let mut report = GraphAudit {
        ledger_event_seq: replay.ledger_head_event_seq,
        consistent: true,
        mismatch_count: 0,
        truncated: false,
        mismatches: Vec::new(),
    };
    let projected_claims: BTreeMap<_, _> = replay
        .claims
        .iter()
        .map(|claim| (&claim.claim_id, claim))
        .collect();
    let claim_ids: BTreeSet<_> = claims
        .keys()
        .chain(projected_claims.keys().copied())
        .chain(exhausted.iter())
        .collect();
    for id in claim_ids {
        let mut record = |field, kind| report.record("claim", id, field, kind);
        let routes_exhausted = exhausted.contains(id);
        match (projected_claims.get(id), claims.get(id)) {
            (Some(expected), Some(live)) => {
                for (field, different) in [
                    ("critical", i64::from(expected.critical) != live.critical),
                    (
                        "effective_stale",
                        i64::from(expected.effective_stale) != live.effective_stale,
                    ),
                    (
                        "effective_trust",
                        expected.effective_trust.as_str() != live.effective_trust,
                    ),
                    ("engine", expected.engine != live.engine),
                    (
                        "formal_target",
                        expected.formal_target != live.formal_target,
                    ),
                    (
                        "informal_statement",
                        expected.informal_statement != live.informal_statement,
                    ),
                    (
                        "local_trust",
                        expected.local_trust.as_str() != live.local_trust,
                    ),
                    ("module_path", expected.module_path != live.module_path),
                    (
                        "prior_trust",
                        expected.prior_trust.map(|t| t.as_str()) != live.prior_trust.as_deref(),
                    ),
                    (
                        "routes_exhausted",
                        expected.routes_exhausted != routes_exhausted,
                    ),
                    ("stale", i64::from(expected.stale) != live.stale),
                    (
                        "taint_root_commit",
                        expected.taint_root_commit != live.taint_root_commit,
                    ),
                    ("title", expected.title != live.title),
                ] {
                    if different {
                        record(field, FieldMismatch);
                    }
                }
            }
            (Some(_), None) => record("presence", MissingProjection),
            (None, Some(_)) => record("presence", UnexpectedProjection),
            (None, None) if routes_exhausted => record("routes_exhausted", UnexpectedProjection),
            (None, None) => unreachable!("Claim IDs come from a row"),
        }
    }

    let projected_plans: BTreeMap<_, _> = replay
        .plans
        .iter()
        .map(|plan| (&plan.plan_id, plan))
        .collect();
    let plan_ids: BTreeSet<_> = plans
        .keys()
        .chain(projected_plans.keys().copied())
        .chain(blockers.keys())
        .chain(dependencies.keys())
        .collect();
    for id in plan_ids {
        let mut record = |field, kind| report.record("proof_plan", id, field, kind);
        let blocker = blockers.get(id);
        let live_deps = dependencies.get(id);
        match (projected_plans.get(id), plans.get(id)) {
            (Some(expected), Some(live)) => {
                // Compare Claim ID and canonical DependencyKind for each edge. The map
                // ignores insertion order; equal cardinality also rules out extra edges.
                let deps_differ = live_deps.map_or(0, BTreeMap::len) != expected.dependencies.len()
                    || expected.dependencies.iter().any(|dep| {
                        live_deps
                            .and_then(|deps| deps.get(&dep.claim_id))
                            .map(String::as_str)
                            != Some(dep.kind.as_str())
                    });
                for (field, different) in [
                    (
                        "blocking_claim_id",
                        expected.blocking_claim_id.as_ref() != blocker,
                    ),
                    ("dependencies", deps_differ),
                    (
                        "durable_status",
                        expected.durable_status != live.durable_status,
                    ),
                    (
                        "effective_trust",
                        expected.effective_trust.as_str() != live.effective_trust,
                    ),
                    ("name", expected.name != live.name),
                    (
                        "target_claim_id",
                        expected.target_claim_id != live.target_claim_id,
                    ),
                    ("track", expected.track.as_str() != live.track),
                ] {
                    if different {
                        record(field, FieldMismatch);
                    }
                }
            }
            (Some(_), None) => record("presence", MissingProjection),
            (None, Some(_)) => record("presence", UnexpectedProjection),
            (None, None) => {
                if blocker.is_some() {
                    record("blocking_claim_id", UnexpectedProjection);
                }
                if live_deps.is_some() {
                    record("dependencies", UnexpectedProjection);
                }
            }
        }
    }
    Ok(report)
}

impl GraphAudit {
    fn record(
        &mut self,
        entity_type: &'static str,
        entity_id: &str,
        field: &'static str,
        kind: GraphMismatchKind,
    ) {
        self.consistent = false;
        self.mismatch_count += 1;
        if self.mismatches.len() < MAX_MISMATCHES {
            self.mismatches.push(GraphMismatch {
                entity_type,
                entity_id: entity_id.to_owned(),
                field,
                kind,
            });
        } else {
            self.truncated = true;
        }
    }
}

struct LiveReadBudget {
    stored: usize,
    decoded: usize,
}

impl LiveReadBudget {
    fn read(
        &mut self,
        tx: &Transaction<'_>,
        table: &'static str,
        columns: &[&'static str],
        row_limit: usize,
        row_kind: &'static str,
        mut read: impl FnMut(&Row<'_>) -> Result<()>,
    ) -> Result<usize> {
        // Table and column names are internal schema constants, with owner ID first.
        // Preflight lengths before projecting text, so even a single oversized value
        // fails without copying it. SQLite's octet_length reads TEXT/BLOB lengths from
        // record metadata; omit ORDER BY to avoid unbounded SQL sort work before LIMIT.
        let lengths = columns
            .iter()
            .map(|col| format!("coalesce(octet_length({col}),0)"))
            .collect::<Vec<_>>()
            .join(",");
        let mut preflight = tx.prepare(&format!("SELECT {lengths} FROM {table} LIMIT ?1"))?;
        let mut probe = preflight.query([row_limit + 1])?;
        let mut count = 0;
        while let Some(row) = probe.next()? {
            count += 1;
            ensure!(
                count <= row_limit,
                "graph audit live projection exceeds the {row_kind} cap while reading {table}"
            );
            for idx in 0..columns.len() {
                let len: usize = row.get(idx)?;
                ensure!(
                    idx != 0 || len <= MAX_LIVE_ID_BYTES,
                    "graph audit live entity ID exceeds {MAX_LIVE_ID_BYTES} bytes in {table}"
                );
                self.stored = subtract_bytes(self.stored, len)?;
            }
        }
        if count == 0 {
            return Ok(0);
        }
        let mut stmt = tx.prepare(&format!(
            "SELECT {} FROM {table} LIMIT ?1",
            columns.join(",")
        ))?;
        let mut rows = stmt.query([count])?;
        while let Some(row) = rows.next()? {
            // Also check UTF-8 bytes before cloning; stored length can differ in a
            // non-UTF-8 database. Fixed-size integer/NULL fields are row-bounded.
            for idx in 0..columns.len() {
                if let ValueRef::Text(bytes) | ValueRef::Blob(bytes) = row.get_ref(idx)? {
                    ensure!(
                        idx != 0 || bytes.len() <= MAX_LIVE_ID_BYTES,
                        "graph audit live entity ID exceeds {MAX_LIVE_ID_BYTES} bytes in {table}"
                    );
                    self.decoded = subtract_bytes(self.decoded, bytes.len())?;
                }
            }
            read(row).with_context(|| format!("invalid live graph projection row in {table}"))?;
        }
        Ok(count)
    }
}

fn subtract_bytes(remaining: usize, len: usize) -> Result<usize> {
    remaining.checked_sub(len).with_context(|| {
        format!("graph audit live projection exceeds the {MAX_LIVE_BYTES}-byte cap")
    })
}
