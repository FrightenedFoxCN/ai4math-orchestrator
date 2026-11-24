//! Bounded, consistent projections of the source-of-truth state database.
use crate::{Event, StateStore, Trust, state::regression};
use anyhow::{Result, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

const TRUST: [&str; 8] = ["L0", "L1", "L2a", "L2b", "L3", "L4a", "L4b", "L5"];
const PRIORITIES: [&str; 3] = ["P0", "P1", "P2"];
const MAX_LIST: usize = 64;
const MAX_EVENT_WINDOW: usize = 80;
const MAX_PAYLOAD: usize = 4096;
const MAX_TRANSITIONS: usize = 8;
const MAX_RISK_EVENTS: usize = 24;
const ALERT_BATCH_SIZE: usize = 32;
const P0_TYPES: &str = "'CRITICAL_CLAIM_REFUTED','ALL_TRACKS_BLOCKED','CRITICAL_BREAKER_TRIPPED','BROKER_REGRESSION_MELTDOWN'";
const BREAKER_TYPES: &str = "'BREAKER_TRIPPED','BREAKER_STATE_CHANGED','BREAKER_CLOSED','BREAKER_ARCHIVED','BREAKER_PROBES_ARMED','BREAKER_PROBES_STARTED','BREAKER_PROBES_FAILED','BREAKER_PROBES_PASSED'";
const RISK_TYPES: &str = "'T3_TAINT_APPLIED','CLAIM_TAINTED','CLAIM_REPROOF_REQUESTED','PLAN_INVALIDATED','PLAN_ROUTE_BLOCKED','CLAIM_ROUTES_EXHAUSTED'";
// SQLite may fail on malformed legacy payloads. CASE (unlike AND) guarantees
// short-circuit evaluation before any JSON accessor sees the input.
const EVENT_LOCAL_TRUST: &str =
    "CASE WHEN json_valid(e.payload_json) THEN json_extract(e.payload_json,'$.local') END";
// Lengths are in UTF-8 bytes. Oversized identity fields are omitted rather than
// making shortened text look like a genuine ID, ref, or source actor.
const EVENT_COLUMNS: &str = "e.event_seq,
    CASE WHEN length(CAST(e.occurred_at AS BLOB)) <= 64 THEN e.occurred_at END,
    CASE WHEN length(CAST(e.event_type AS BLOB)) <= 64 THEN e.event_type END,
    CASE WHEN length(CAST(e.entity_type AS BLOB)) <= 64 THEN e.entity_type END,
    CASE WHEN length(CAST(e.entity_id AS BLOB)) <= 256 THEN e.entity_id END,
    CASE WHEN length(CAST(e.git_commit AS BLOB)) <= 128 THEN e.git_commit END,
    CASE WHEN length(CAST(e.actor AS BLOB)) <= 128 THEN e.actor END,
    CASE WHEN length(CAST(e.payload_json AS BLOB)) <= ?1 THEN e.payload_json END";
const COOLDOWN_SECONDS: i64 = 300;

#[derive(Clone, Debug, Serialize)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub truncated: bool,
}
impl<T> Page<T> {
    fn limit(mut items: Vec<T>, limit: usize) -> Self {
        let truncated = items.len() > limit;
        items.truncate(limit);
        Self { items, truncated }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct FrontierClaim {
    pub claim_id: String,
    pub title: String,
    pub informal_statement: String,
    pub formal_signature: Option<String>,
    pub formal_target: Option<String>,
    pub engine: String,
    pub local_trust: Trust,
    pub effective_trust: Trust,
    pub stale: bool,
    pub critical: bool,
    pub oracle_debt: bool,
    pub wiki_id: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct FrontierEdge {
    pub target_claim_id: String,
    pub plan_id: String,
    pub plan_status: String,
    pub dep_claim_id: Option<String>,
    pub dep_kind: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct FrontierPage {
    #[serde(flatten)]
    pub claims: Page<FrontierClaim>,
    pub plans: Page<FrontierEdge>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct InboxProjection {
    pub inbox_id: String,
    pub priority: String,
    pub category: String,
    pub summary: String,
    pub leverage_score: f64,
    pub created_at: String,
    pub payload: Option<Value>,
    pub payload_truncated: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct BreakerProjection {
    pub breaker_id: String,
    pub target_definition_id: String,
    pub module_path: String,
    pub engine: String,
    pub state: String,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe_status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe_passed: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe_required: Option<i64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Checkpoint {
    pub cursor_id: String,
    pub event_seq: i64,
    pub git_commit: String,
    pub last_adr_id: Option<String>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct PushAlert {
    pub alert_id: String,
    pub event_seq: i64,
    pub created_at: String,
    pub severity: &'static str,
    pub reason_code: String,
    pub dedupe_key: String,
    pub headline: String,
    pub related_entities: Vec<String>,
    pub briefing_id: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct TelemetrySnapshot {
    pub event_seq: i64,
    pub observed_at: String,
    pub trust: BTreeMap<String, i64>,
    pub stale_claims: i64,
    pub pending_inbox: BTreeMap<String, i64>,
    pub tripped_breakers: i64,
    pub half_open_breakers: i64,
    pub archived_breakers: i64,
    pub task_scheduler: crate::TaskSchedulerStatus,
    pub last_alert: Option<PushAlert>,
}

#[derive(Debug, Serialize)]
pub struct EvidenceClaim {
    pub claim_id: String,
    pub title: String,
    pub informal_statement: String,
    pub formal_signature: Option<String>,
    pub formal_target: Option<String>,
    pub engine: String,
    pub critical: bool,
}

#[derive(Debug, Serialize)]
pub struct TrustTransition {
    pub event_seq: i64,
    pub git_commit: Option<String>,
    pub claim: Option<EvidenceClaim>,
    pub claim_id: String,
    pub new_local_trust: Trust,
    pub graveyard_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct RiskRadar {
    pub stale_claims: i64,
    pub oracle_debts: i64,
    pub cross_engine_debts: i64,
    pub tripped_breakers: i64,
    pub events: Page<Event>,
}

#[derive(Debug, Serialize)]
pub struct FactBody {
    pub event_from_exclusive: i64,
    pub event_to_inclusive: i64,
    pub to_git_commit: Option<String>,
    pub checkpoint: Option<Checkpoint>,
    pub trust: BTreeMap<String, i64>,
    pub stale_claims: i64,
    pub events: Page<Event>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub alert_sources: Vec<Event>,
    pub frontier: FrontierPage,
    pub milestones: Page<TrustTransition>,
    pub refutations: Page<TrustTransition>,
    pub risk_radar: RiskRadar,
    pub breakers: Page<BreakerProjection>,
    pub inbox: Page<InboxProjection>,
}

fn max_seq(db: &Connection) -> Result<i64> {
    Ok(
        db.query_row("SELECT coalesce(max(event_seq),0) FROM events", [], |r| {
            r.get(0)
        })?,
    )
}

fn trust_counts(db: &Connection) -> Result<BTreeMap<String, i64>> {
    grouped(
        db,
        &TRUST,
        "SELECT effective_trust_level,count(*) FROM claims GROUP BY effective_trust_level",
    )
}

fn grouped(db: &Connection, keys: &[&str], sql: &str) -> Result<BTreeMap<String, i64>> {
    let mut counts = keys
        .iter()
        .map(|&k| (k.to_owned(), 0))
        .collect::<BTreeMap<_, _>>();
    let mut stmt = db.prepare(sql)?;
    for row in stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))? {
        let (key, value) = row?;
        counts.insert(key, value);
    }
    Ok(counts)
}

fn stale_claims(db: &Connection) -> Result<i64> {
    Ok(db.query_row(
        "SELECT count(*) FROM claims WHERE effective_stale=1",
        [],
        |r| r.get(0),
    )?)
}

fn trust(row: &rusqlite::Row<'_>, col: usize) -> rusqlite::Result<Trust> {
    row.get::<_, String>(col)?
        .parse()
        .map_err(|_| rusqlite::Error::InvalidQuery)
}

fn frontier(db: &Connection, limit: usize) -> Result<FrontierPage> {
    // Priority is decision relevance, then stable claim ID rather than wall-clock time.
    let where_rank = "WHERE effective_stale=1 OR effective_trust_level NOT IN ('L5','L2b') ORDER BY is_critical_path DESC,effective_stale DESC,claim_id";
    let mut stmt = db.prepare(&format!("SELECT claim_id, substr(title,1,256), substr(informal_statement,1,2048),
        CASE WHEN length(CAST(formal_signature AS BLOB)) <= 4096 THEN formal_signature END,
        CASE WHEN length(CAST(formal_target AS BLOB)) <= 512 THEN formal_target END, engine, local_trust_level,
        effective_trust_level,effective_stale,is_critical_path,oracle_debt,wiki_id FROM claims {where_rank} LIMIT ?1"))?;
    let claims = Page::limit(
        stmt.query_map([limit + 1], |r| {
            Ok(FrontierClaim {
                claim_id: r.get(0)?,
                title: r.get(1)?,
                informal_statement: r.get(2)?,
                formal_signature: r.get(3)?,
                formal_target: r.get(4)?,
                engine: r.get(5)?,
                local_trust: trust(r, 6)?,
                effective_trust: trust(r, 7)?,
                stale: r.get(8)?,
                critical: r.get(9)?,
                oracle_debt: r.get(10)?,
                wiki_id: r.get(11)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?,
        limit,
    );
    let edge_limit = limit.saturating_mul(3);
    let mut stmt = db.prepare(&format!("WITH selected AS (SELECT claim_id FROM claims {where_rank} LIMIT ?1)
        SELECT p.target_claim_id,p.plan_id,CASE WHEN b.plan_id IS NULL THEN p.status ELSE 'BLOCKED_BY_REFUTATION' END,d.dep_claim_id,d.dep_kind FROM selected s
        JOIN proof_plans p ON p.target_claim_id=s.claim_id LEFT JOIN proof_plan_route_blocks b USING(plan_id) LEFT JOIN plan_dependencies d USING(plan_id)
        ORDER BY p.target_claim_id,p.plan_id,d.dep_claim_id LIMIT ?2"))?;
    let plans = Page::limit(
        stmt.query_map(params![limit, edge_limit + 1], |r| {
            Ok(FrontierEdge {
                target_claim_id: r.get(0)?,
                plan_id: r.get(1)?,
                plan_status: r.get(2)?,
                dep_claim_id: r.get(3)?,
                dep_kind: r.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?,
        edge_limit,
    );
    Ok(FrontierPage { claims, plans })
}

pub(crate) fn current_checkpoint(db: &Connection) -> Result<Option<Checkpoint>> {
    Ok(db.query_row("SELECT cursor_id,event_seq,git_commit,last_adr_id FROM human_checkpoints WHERE is_current=1", [], |r| {
        Ok(Checkpoint {cursor_id:r.get(0)?,event_seq:r.get(1)?,git_commit:r.get(2)?,last_adr_id:r.get(3)?})
    }).optional()?)
}

fn inbox(db: &Connection, limit: usize) -> Result<Page<InboxProjection>> {
    let mut stmt=db.prepare("SELECT inbox_id,priority,category,substr(summary,1,512),leverage_score,created_at,
        CASE WHEN length(CAST(payload_json AS BLOB)) <= ?1 THEN payload_json END
        FROM decision_inbox WHERE status='PENDING' ORDER BY priority,leverage_score DESC,created_at,inbox_id LIMIT ?2")?;
    let items = stmt
        .query_map(params![MAX_PAYLOAD, limit + 1], |r| {
            let payload = r
                .get::<_, Option<String>>(6)?
                .and_then(|s| serde_json::from_str(&s).ok());
            Ok(InboxProjection {
                inbox_id: r.get(0)?,
                priority: r.get(1)?,
                category: r.get(2)?,
                summary: r.get(3)?,
                leverage_score: r.get(4)?,
                created_at: r.get(5)?,
                payload_truncated: payload.is_none(),
                payload,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(Page::limit(items, limit))
}

fn breakers(db: &Connection, limit: usize) -> Result<Page<BreakerProjection>> {
    let mut stmt=db.prepare("SELECT b.breaker_id,b.target_definition_id,b.module_path,b.engine,b.state,b.updated_at,
            CASE WHEN b.state='HALF_OPEN' THEN COALESCE(p.status,'AWAITING_REVIEW')
                 WHEN b.state='CLOSED' AND p.status='PASSED' THEN p.status END,
            CASE WHEN p.status='PASSED' AND b.state='CLOSED' THEN p.required
                 WHEN p.required IS NOT NULL AND b.state='HALF_OPEN' THEN 0 END,
            CASE WHEN b.state IN ('HALF_OPEN','CLOSED') THEN p.required END
        FROM circuit_breakers b LEFT JOIN breaker_probe_batches p ON p.approval_id=(
            SELECT approval_id FROM breaker_probe_batches WHERE breaker_id=b.breaker_id ORDER BY arm_event_seq DESC LIMIT 1)
        ORDER BY CASE b.state WHEN 'TRIPPED' THEN 0 WHEN 'HALF_OPEN' THEN 1 ELSE 2 END,b.breaker_id LIMIT ?1")?;
    Ok(Page::limit(
        stmt.query_map([limit + 1], |r| {
            Ok(BreakerProjection {
                breaker_id: r.get(0)?,
                target_definition_id: r.get(1)?,
                module_path: r.get(2)?,
                engine: r.get(3)?,
                state: r.get(4)?,
                updated_at: r.get(5)?,
                probe_status: r.get(6)?,
                probe_passed: r.get(7)?,
                probe_required: r.get(8)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?,
        limit,
    ))
}

fn event_window(db: &Connection, from: i64, to: i64) -> Result<Page<Event>> {
    let mut stmt = db.prepare(&format!(
        "SELECT {EVENT_COLUMNS} FROM events e
        WHERE e.event_seq > ?2 AND e.event_seq <= ?3 ORDER BY e.event_seq DESC LIMIT ?4"
    ))?;
    let mut result = Page::limit(
        stmt.query_map(
            params![MAX_PAYLOAD, from, to, MAX_EVENT_WINDOW + 1],
            read_event,
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?,
        MAX_EVENT_WINDOW,
    );
    result.items.reverse();
    Ok(result)
}

pub(crate) fn safe_text(value: &str, max_bytes: usize) -> bool {
    !value.is_empty() && value.len() <= max_bytes && !value.chars().any(char::is_control)
}

fn read_event(r: &rusqlite::Row<'_>) -> rusqlite::Result<Event> {
    let text = |col| -> rusqlite::Result<Option<String>> {
        Ok(r.get::<_, Option<String>>(col)?
            .filter(|s| safe_text(s, 256)))
    };
    let occurred_at = text(1)?;
    let event_type = text(2)?;
    let entity_type = text(3)?;
    let entity_id = text(4)?;
    let actor = text(6)?;
    let raw = r
        .get::<_, Option<String>>(7)?
        .and_then(|p| serde_json::from_str(&p).ok());
    let mut payload = event_payload(event_type.as_deref().unwrap_or_default(), raw);
    if occurred_at.is_none()
        || event_type.is_none()
        || entity_type.is_none()
        || entity_id.is_none()
        || actor.is_none()
    {
        payload.insert("metadata_omitted".into(), Value::Bool(true));
    }
    Ok(Event {
        event_seq: r.get(0)?,
        occurred_at: occurred_at.unwrap_or_default(),
        event_type: event_type.unwrap_or_default(),
        entity_type: entity_type.unwrap_or_default(),
        entity_id: entity_id.unwrap_or_default(),
        git_commit: text(5)?,
        actor: actor.unwrap_or_default(),
        payload: Value::Object(payload),
    })
}

fn event_payload(kind: &str, raw: Option<Value>) -> Map<String, Value> {
    let mut out = Map::new();
    // Event payloads are not a public blob interface. Copy only small, typed
    // fields with decision value, never proof bodies, diagnostics or log text.
    let keys: &[&str] = match kind {
        "CLAIM_TRUST_CHANGED" => &["previous_local", "local", "was_stale"],
        "CLAIM_EFFECTIVE_CHANGED" => &["previous", "effective", "effective_stale"],
        "PLAN_TRUST_CHANGED" => &["previous", "effective"],
        "CLAIM_TAINTED" => &["prior_trust"],
        "CRITICAL_CLAIM_REFUTED" => &["critical_claim", "refuted_claim", "claim_itself_refuted"],
        "ALL_TRACKS_BLOCKED" => &["exploration_blocked", "infrastructure_blocked"],
        regression::MELTDOWN => &["failed_module", "distinct_candidates", "window_seconds"],
        "PLAN_INVALIDATED" => &["refuted_claim", "target"],
        "PLAN_ARCHIVED" => &["target", "adr_id"],
        "PLAN_ROUTE_BLOCKED" | "CLAIM_ROUTES_EXHAUSTED" => &["refuted_claim"],
        "WITNESS_VERIFIED" => &["wiki_id", "witness_sha256", "file_path", "target_decl"],
        "BREAKER_TRIPPED" | "BREAKER_STATE_CHANGED" | "BREAKER_CLOSED" | "BREAKER_ARCHIVED" => &[
            "state",
            "previous_state",
            "inbox_id",
            "adr_id",
            "trip_event_seq",
            "probe_status",
        ],
        "CRITICAL_BREAKER_TRIPPED" => &[
            "state",
            "inbox_id",
            "trip_event_seq",
            "critical_claim_id",
            "critical_claim_count",
        ],
        "BREAKER_PROBES_ARMED"
        | "BREAKER_PROBES_STARTED"
        | "BREAKER_PROBES_FAILED"
        | "BREAKER_PROBES_PASSED" => &[
            "state",
            "adr_id",
            "approval_id",
            "probe_status",
            "probe_passed",
            "probe_required",
            "receipt_sha256",
        ],
        "PIVOT_APPLIED" => &[
            "action",
            "fact_sha256",
            "reviewed_through",
            "breaker_id",
            "inbox_id",
            "probes_started",
        ],
        "PROMOTION_COMMITTED" => &[
            "base_commit",
            "receipt_id",
            "patch_sha256",
            "file_path",
            "target_decl",
        ],
        _ => &[],
    };
    if let Some(Value::Object(fields)) = raw.as_ref() {
        for &key in keys {
            let Some(value) = fields.get(key) else {
                continue;
            };
            let usable = match key {
                "was_stale" | "effective_stale" | "claim_itself_refuted" | "probes_started" => {
                    value.is_boolean()
                }
                "trip_event_seq" | "reviewed_through" | "critical_claim_count" => {
                    value.as_i64().is_some_and(|n| n > 0)
                }
                "exploration_blocked" | "infrastructure_blocked" => {
                    value.as_i64().is_some_and(|n| n >= 0)
                }
                "failed_module" => value.as_str().is_some_and(regression::public_failed_module),
                "distinct_candidates" => value.as_i64() == Some(regression::DISTINCT_CANDIDATES),
                "window_seconds" => value.as_i64() == Some(regression::WINDOW_SECONDS),
                "probe_status" => matches!(
                    (kind, value.as_str()),
                    ("BREAKER_STATE_CHANGED", Some("AWAITING_REVIEW"))
                        | ("BREAKER_PROBES_ARMED", Some("READY"))
                        | ("BREAKER_PROBES_STARTED", Some("RUNNING"))
                        | ("BREAKER_PROBES_FAILED", Some("NEEDS_PROBE"))
                        | ("BREAKER_PROBES_PASSED", Some("PASSED"))
                ),
                "probe_passed" => value.as_i64().is_some_and(|n| (0..=2).contains(&n)),
                "probe_required" => value.as_i64().is_some_and(|n| (1..=2).contains(&n)),
                "previous_local" | "local" | "prior_trust" | "previous" | "effective" => {
                    value.as_str().is_some_and(|s| s.parse::<Trust>().is_ok())
                }
                "state" | "previous_state" => value
                    .as_str()
                    .is_some_and(|s| matches!(s, "TRIPPED" | "CLOSED" | "HALF_OPEN" | "ARCHIVED")),
                _ => value.as_str().is_some_and(|s| safe_text(s, 256)),
            };
            if usable {
                out.insert(key.to_owned(), value.clone());
            }
        }
    }
    if raw.as_ref().and_then(Value::as_object) != Some(&out) {
        out.insert("payload_omitted".into(), Value::Bool(true));
    }
    out
}

fn risk_events(db: &Connection, from: i64, to: i64) -> Result<Page<Event>> {
    // Explicit P0 signals take precedence over background risks; within each
    // class, newest first. The generic event tail is not an input to selection.
    let mut stmt = db.prepare(&format!(
        "SELECT {EVENT_COLUMNS} FROM events e
        WHERE e.event_seq > ?2 AND e.event_seq <= ?3 AND (
            e.event_type IN ({P0_TYPES},{BREAKER_TYPES},{RISK_TYPES})
            OR (e.event_type='CLAIM_TRUST_CHANGED' AND e.entity_type='claim'
                AND ({EVENT_LOCAL_TRUST}) IN ('L2a','L4a','L4b'))
        ) ORDER BY (e.event_type IN ({P0_TYPES})) DESC,e.event_seq DESC LIMIT ?4"
    ))?;
    Ok(Page::limit(
        stmt.query_map(
            params![MAX_PAYLOAD, from, to, MAX_RISK_EVENTS + 1],
            read_event,
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?,
        MAX_RISK_EVENTS,
    ))
}

fn alert_sources(db: &Connection, through: i64) -> Result<Vec<Event>> {
    let mut stmt = db.prepare(&format!("SELECT {EVENT_COLUMNS} FROM events e
        WHERE e.event_seq>(SELECT event_seq FROM daemon_alert_cursor WHERE singleton=1) AND e.event_seq<=?2
        AND e.event_type IN ({P0_TYPES}) ORDER BY e.event_seq LIMIT ?3"))?;
    Ok(stmt
        .query_map(params![MAX_PAYLOAD, through, ALERT_BATCH_SIZE], read_event)?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

fn trust_transitions(
    db: &Connection,
    from: i64,
    to: i64,
    refutations: bool,
) -> Result<Page<TrustTransition>> {
    let levels = if refutations {
        "'L2b'"
    } else {
        "'L4a','L4b','L5'"
    };
    // Read the historical transition level and graveyard reference, not the
    // current claim level. The bilingual claim card is a current-snapshot
    // projection: older producers don't record statement/signature versions.
    let mut stmt = db.prepare(&format!("SELECT e.event_seq,
        CASE WHEN length(CAST(e.git_commit AS BLOB)) <= 128 THEN e.git_commit END,
        CASE WHEN length(CAST(e.entity_id AS BLOB)) <= 256 THEN e.entity_id END,
        {EVENT_LOCAL_TRUST},
        CASE WHEN json_valid(e.payload_json) THEN
             CASE WHEN json_type(e.payload_json,'$.detail.wiki_id')='text' AND
                 length(CAST(json_extract(e.payload_json,'$.detail.wiki_id') AS BLOB)) <= 256
             THEN json_extract(e.payload_json,'$.detail.wiki_id') END END,
        c.claim_id,substr(c.title,1,256),substr(c.informal_statement,1,1200),
        CASE WHEN length(CAST(c.formal_signature AS BLOB)) <= 4096 THEN c.formal_signature END,
        CASE WHEN length(CAST(c.formal_target AS BLOB)) <= 512 THEN c.formal_target END,
        c.engine,c.is_critical_path
        FROM events e LEFT JOIN claims c ON c.claim_id=e.entity_id AND length(CAST(e.entity_id AS BLOB)) <= 256
        WHERE e.event_type='CLAIM_TRUST_CHANGED' AND e.entity_type='claim'
            AND e.event_seq>?1 AND e.event_seq<=?2 AND ({EVENT_LOCAL_TRUST}) IN ({levels})
        ORDER BY coalesce(c.is_critical_path,0) DESC,e.event_seq DESC LIMIT ?3"))?;
    let items = stmt
        .query_map(params![from, to, MAX_TRANSITIONS + 1], |r| {
            let id = r.get::<_, Option<String>>(5)?.filter(|s| safe_text(s, 256));
            let claim = if let Some(claim_id) = id {
                Some(EvidenceClaim {
                    claim_id,
                    title: r.get(6)?,
                    informal_statement: r.get(7)?,
                    formal_signature: r.get(8)?,
                    formal_target: r.get(9)?,
                    engine: r.get(10)?,
                    critical: r.get(11)?,
                })
            } else {
                None
            };
            Ok(TrustTransition {
                event_seq: r.get(0)?,
                git_commit: r.get::<_, Option<String>>(1)?.filter(|s| safe_text(s, 128)),
                claim_id: r
                    .get::<_, Option<String>>(2)?
                    .filter(|s| safe_text(s, 256))
                    .unwrap_or_default(),
                new_local_trust: trust(r, 3)?,
                graveyard_id: r.get::<_, Option<String>>(4)?.filter(|s| safe_text(s, 256)),
                claim,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(Page::limit(items, MAX_TRANSITIONS))
}

impl StateStore {
    pub fn live_telemetry(&mut self) -> Result<TelemetrySnapshot> {
        let tx = self.db.transaction()?;
        let value=TelemetrySnapshot { event_seq:max_seq(&tx)?,
            observed_at:tx.query_row("SELECT strftime('%Y-%m-%dT%H:%M:%fZ','now')",[],|r|r.get(0))?,
            trust:trust_counts(&tx)?,stale_claims:stale_claims(&tx)?,
            pending_inbox:grouped(&tx,&PRIORITIES,"SELECT priority,count(*) FROM decision_inbox WHERE status='PENDING' GROUP BY priority")?,
            tripped_breakers:tx.query_row("SELECT count(*) FROM circuit_breakers WHERE state='TRIPPED'",[],|r|r.get(0))?,
            half_open_breakers:tx.query_row("SELECT count(*) FROM circuit_breakers WHERE state='HALF_OPEN'",[],|r|r.get(0))?,
            archived_breakers:tx.query_row("SELECT count(*) FROM circuit_breakers WHERE state='ARCHIVED'",[],|r|r.get(0))?,
            task_scheduler:crate::state::scheduler_snapshot(&tx)?,
            last_alert:tx.query_row("SELECT alert_id,event_seq,created_at,reason_code,dedupe_key,headline,entity_id,briefing_id
                FROM daemon_alerts ORDER BY event_seq DESC LIMIT 1",[],read_alert).optional()?,
        };
        tx.commit()?;
        Ok(value)
    }

    pub fn frontier_page(&mut self, limit: usize) -> Result<FrontierPage> {
        ensure!((1..=MAX_LIST).contains(&limit), "invalid frontier limit");
        let tx = self.db.transaction()?;
        let page = frontier(&tx, limit)?;
        tx.commit()?;
        Ok(page)
    }

    pub fn inbox_page(&self, limit: usize) -> Result<Page<InboxProjection>> {
        ensure!((1..=MAX_LIST).contains(&limit), "invalid inbox limit");
        inbox(&self.db, limit)
    }

    /// Return only breaker notifications, preserving sequence order within the
    /// caller's snapshot bound. Advance to the last item if truncated, or to
    /// `through` when the entire interval was examined.
    pub fn breaker_events_through(
        &self,
        after: i64,
        through: i64,
        limit: usize,
    ) -> Result<Page<Event>> {
        ensure!(
            after >= 0 && through >= after && (1..=128).contains(&limit),
            "invalid breaker event bounds"
        );
        let mut stmt = self.db.prepare(&format!(
            "SELECT {EVENT_COLUMNS} FROM events e
            WHERE e.event_seq>?2 AND e.event_seq<=?3 AND e.entity_type='breaker'
                AND e.event_type IN ({BREAKER_TYPES}) ORDER BY e.event_seq LIMIT ?4"
        ))?;
        Ok(Page::limit(
            stmt.query_map(params![MAX_PAYLOAD, after, through, limit + 1], read_event)?
                .collect::<rusqlite::Result<Vec<_>>>()?,
            limit,
        ))
    }

    pub fn briefing_facts(&mut self) -> Result<FactBody> {
        self.compile_facts(false)
    }

    pub fn briefing_for_alerts(&mut self) -> Result<FactBody> {
        self.compile_facts(true)
    }

    fn compile_facts(&mut self, include_alert_sources: bool) -> Result<FactBody> {
        let tx = self.db.transaction()?;
        let event_to_inclusive = max_seq(&tx)?;
        let checkpoint = tx
            .query_row(
                "SELECT
            CASE WHEN length(CAST(cursor_id AS BLOB))<=256 THEN cursor_id END,event_seq,
            CASE WHEN length(CAST(git_commit AS BLOB))<=128 THEN git_commit END,
            CASE WHEN length(CAST(last_adr_id AS BLOB))<=256 THEN last_adr_id END
            FROM human_checkpoints WHERE is_current=1",
                [],
                |r| {
                    Ok(Checkpoint {
                        cursor_id: r.get::<_, Option<String>>(0)?.unwrap_or_default(),
                        event_seq: r.get(1)?,
                        git_commit: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                        last_adr_id: r.get(3)?,
                    })
                },
            )
            .optional()?;
        let event_from_exclusive = checkpoint.as_ref().map_or(0, |c| c.event_seq);
        let to_git_commit=tx.query_row("SELECT CASE WHEN length(CAST(git_commit AS BLOB))<=128 THEN git_commit END
            FROM events WHERE event_seq<=?1 AND git_commit IS NOT NULL ORDER BY event_seq DESC LIMIT 1",
            [event_to_inclusive],|r|r.get::<_,Option<String>>(0)).optional()?.flatten();
        let events = event_window(&tx, event_from_exclusive, event_to_inclusive)?;
        let milestones = trust_transitions(&tx, event_from_exclusive, event_to_inclusive, false)?;
        let refutations = trust_transitions(&tx, event_from_exclusive, event_to_inclusive, true)?;
        let risks = risk_events(&tx, event_from_exclusive, event_to_inclusive)?;
        let risk_radar=tx.query_row("SELECT (SELECT count(*) FROM claims WHERE effective_stale=1),
            (SELECT count(*) FROM claims WHERE oracle_debt=1), (SELECT count(*) FROM claims WHERE local_trust_level='L4b'),
            (SELECT count(*) FROM circuit_breakers WHERE state='TRIPPED')",[],|r|Ok(RiskRadar {
              stale_claims:r.get(0)?,oracle_debts:r.get(1)?,cross_engine_debts:r.get(2)?,tripped_breakers:r.get(3)?,events:risks
        }))?;
        let value = FactBody {
            event_from_exclusive,
            event_to_inclusive,
            to_git_commit,
            checkpoint,
            trust: trust_counts(&tx)?,
            stale_claims: stale_claims(&tx)?,
            events,
            milestones,
            refutations,
            risk_radar,
            alert_sources: if include_alert_sources {
                alert_sources(&tx, event_to_inclusive)?
            } else {
                vec![]
            },
            frontier: frontier(&tx, 24)?,
            breakers: breakers(&tx, 24)?,
            inbox: inbox(&tx, 3)?,
        };
        tx.commit()?;
        Ok(value)
    }

    /// Persist P0 deduplication and scan position together. The latest alert remains
    /// queryable after a daemon restart or a missed live push.
    pub fn scan_alerts(
        &mut self,
        through: i64,
        briefing_id: &str,
        retain: impl FnOnce() -> Result<()>,
    ) -> Result<Vec<PushAlert>> {
        ensure!(
            through >= 0
                && briefing_id.len() == 64
                && briefing_id.bytes().all(|c| c.is_ascii_hexdigit()),
            "invalid P0 scan bounds"
        );
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let cursor: i64 = tx.query_row(
            "SELECT event_seq FROM daemon_alert_cursor WHERE singleton=1",
            [],
            |r| r.get(0),
        )?;
        if through <= cursor {
            tx.commit()?;
            return Ok(vec![]);
        }
        ensure!(
            through <= max_seq(&tx)?,
            "P0 scan cannot pass the event log"
        );
        let now: i64 = tx.query_row("SELECT unixepoch('now')", [], |r| r.get(0))?;
        let events = {
            let mut stmt=tx.prepare(&format!("SELECT event_seq,event_type,entity_id FROM events WHERE event_seq>?1 AND event_seq<=?2
                AND event_type IN ({P0_TYPES}) ORDER BY event_seq LIMIT ?3"))?;
            stmt.query_map(params![cursor, through, ALERT_BATCH_SIZE], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let next = if events.len() == ALERT_BATCH_SIZE {
            events[ALERT_BATCH_SIZE - 1].0
        } else {
            through
        };
        let mut alerts = Vec::new();
        for (event_seq, reason, entity) in events {
            let headline = match reason.as_str() {
                "CRITICAL_CLAIM_REFUTED" => "关键命题被内核证伪，或关键命题的已知路线全部失效",
                "ALL_TRACKS_BLOCKED" => "研究轨道全部阻塞",
                "CRITICAL_BREAKER_TRIPPED" => "关键路径局部熔断",
                "BROKER_REGRESSION_MELTDOWN" => "验证底座持续异常",
                _ => continue,
            };
            let key = serde_json::to_string(&(&reason, &entity))?;
            if tx.query_row("SELECT EXISTS(SELECT 1 FROM daemon_alerts WHERE dedupe_key=?1 AND dispatched_unix>?2)",params![&key,now-COOLDOWN_SECONDS],|r|r.get::<_,bool>(0))? {continue;}
            let id = format!("p0-{event_seq}");
            let alert=tx.query_row("INSERT INTO daemon_alerts(alert_id,event_seq,created_at,reason_code,dedupe_key,headline,entity_id,briefing_id,dispatched_unix)
                VALUES(?1,?2,strftime('%Y-%m-%dT%H:%M:%fZ','now'),?3,?4,?5,?6,?7,?8)
                RETURNING alert_id,event_seq,created_at,reason_code,dedupe_key,headline,entity_id,briefing_id",
                params![id,event_seq,reason,key,headline,entity,briefing_id,now],read_alert)?;
            alerts.push(alert);
        }
        if !alerts.is_empty() {
            retain()?;
        }
        tx.execute(
            "UPDATE daemon_alert_cursor SET event_seq=?1 WHERE singleton=1",
            [next],
        )?;
        tx.commit()?;
        Ok(alerts)
    }

    pub fn has_alert_candidates(&self, through: i64) -> Result<bool> {
        Ok(self.db.query_row(&format!("SELECT EXISTS(SELECT 1 FROM events WHERE event_seq>(SELECT event_seq FROM daemon_alert_cursor WHERE singleton=1)
            AND event_seq<=?1 AND event_type IN ({P0_TYPES}))"),[through],|r|r.get(0))?)
    }

    pub fn alert_history(&self, after: i64, limit: usize) -> Result<Page<PushAlert>> {
        ensure!(
            after >= 0 && (1..=MAX_LIST).contains(&limit),
            "invalid alert history bounds"
        );
        let mut stmt=self.db.prepare("SELECT alert_id,event_seq,created_at,reason_code,dedupe_key,headline,entity_id,briefing_id
          FROM daemon_alerts WHERE event_seq>?1 ORDER BY event_seq LIMIT ?2")?;
        Ok(Page::limit(
            stmt.query_map(params![after, limit + 1], read_alert)?
                .collect::<rusqlite::Result<Vec<_>>>()?,
            limit,
        ))
    }
}

fn read_alert(r: &rusqlite::Row<'_>) -> rusqlite::Result<PushAlert> {
    Ok(PushAlert {
        alert_id: r.get(0)?,
        event_seq: r.get(1)?,
        created_at: r.get(2)?,
        severity: "P0",
        reason_code: r.get(3)?,
        dedupe_key: r.get(4)?,
        headline: r.get(5)?,
        related_entities: vec![r.get(6)?],
        briefing_id: r.get(7)?,
    })
}
