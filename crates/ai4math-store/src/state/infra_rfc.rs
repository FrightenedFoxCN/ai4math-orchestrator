//! Operator-only durable RFC intake and 7-state lifecycle.
//! Approval and lifecycle transitions record operator-verified milestones, not work admission or trust.
use super::*;
use ai4math_core::{Engine, InfraRFC, InfraStatus, validate_source_path};
use ai4math_evidence::{InfraCompletionEvidence, InfraSmokeEvidence};
use anyhow::bail;
use sha2::{Digest, Sha256};

pub const INFRA_RFC_MAX_BYTES: usize = 16 * 1024;
pub(crate) const SUBMITTED: &str = "INFRA_RFC_SUBMITTED";
pub(crate) const APPROVED: &str = "INFRA_RFC_APPROVED";
pub(crate) const SMOKE_STARTED: &str = "INFRA_RFC_SMOKE_STARTED";
pub(crate) const FROZEN: &str = "INFRA_RFC_FROZEN";
pub(crate) const COMPLETED: &str = "INFRA_RFC_COMPLETED";
pub(crate) const SUPERSEDED: &str = "INFRA_RFC_SUPERSEDED";
pub(crate) const REJECTED: &str = "INFRA_RFC_REJECTED";
const ACTOR: &str = "operator:infra-rfc";
const ENTITY: &str = "infra_rfc";
const READ: &str = "SELECT module_path,reviewed_commit,draft_sha256,consumer_sha256,draft_json,submitted_event_seq,approved_event_seq FROM infra_rfcs WHERE rfc_id=?1 AND repo=?2";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum InfraRfcStatus {
    Draft,
    Approved,
    BuildingSmoke,
    FrozenProving,
    Completed,
    Superseded,
    Rejected,
}

impl InfraRfcStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Draft => "DRAFT",
            Self::Approved => "APPROVED",
            Self::BuildingSmoke => "BUILDING_SMOKE",
            Self::FrozenProving => "FROZEN_PROVING",
            Self::Completed => "COMPLETED",
            Self::Superseded => "SUPERSEDED",
            Self::Rejected => "REJECTED",
        }
    }

    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "DRAFT" => Some(Self::Draft),
            "APPROVED" => Some(Self::Approved),
            "BUILDING_SMOKE" => Some(Self::BuildingSmoke),
            "FROZEN_PROVING" => Some(Self::FrozenProving),
            "COMPLETED" => Some(Self::Completed),
            "SUPERSEDED" => Some(Self::Superseded),
            "REJECTED" => Some(Self::Rejected),
            _ => None,
        }
    }

    fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Superseded | Self::Rejected)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InfraRfcRecord {
    schema: &'static str,
    pub module_path: String,
    pub reviewed_commit: String,
    pub draft_sha256: String,
    pub status: InfraRfcStatus,
    pub draft: InfraRFC,
    pub submitted_event_seq: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approved_event_seq: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub smoke_started_event_seq: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub smoke_commit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub smoke_receipt_sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frozen_event_seq: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completion_commit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completion_receipt_sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_event_seq: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub superseded_by_rfc_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub superseded_event_seq: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rejected_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rejected_event_seq: Option<i64>,
}

fn id(text: &str, max: usize) -> bool {
    !text.is_empty()
        && text.len() <= max
        && text.as_bytes()[0].is_ascii_alphanumeric()
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b':'))
}
fn hex(text: &str, n: usize) -> bool {
    text.len() == n
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'a'..=b'f'))
}
fn commit(text: &str) -> bool {
    (hex(text, 40) || hex(text, 64)) && text.bytes().any(|b| b != b'0')
}
fn sha(data: &impl Serialize) -> Result<String> {
    Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(data)?)))
}
fn text_ok(s: &str, max: usize, multiline: bool) -> bool {
    !s.is_empty() && s.len() <= max && s.trim() == s && !s.chars().any(|c| {
        (c.is_control() && !(multiline && matches!(c, '\n' | '\t')))
            || matches!(
                c,
                '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
            )
    })
}
fn list_ok(items: &[String], nonempty: bool) -> bool {
    (!nonempty || !items.is_empty())
        && items.len() <= 16
        && items.iter().all(|item| text_ok(item, 512, false))
        && items.iter().collect::<BTreeSet<_>>().len() == items.len()
}
fn canonical(draft: &InfraRFC) -> Result<String> {
    ensure!(
        draft.status == InfraStatus::Draft
            && id(&draft.rfc_id, 100)
            && id(&draft.requested_by_claim, 256),
        "InfraRFC requires DRAFT and valid RFC and consumer Claim IDs"
    );
    ensure!(
        text_ok(&draft.motivation, 2048, true)
            && text_ok(&draft.consumer_usage_sketch, 4096, true)
            && list_ok(&draft.proposed_definitions, true)
            && list_ok(&draft.required_api_lemmas, false)
            && list_ok(&draft.smoke_test_criteria, true),
        "invalid bounded InfraRFC text, definitions, lemmas or smoke criteria"
    );
    let bytes = serde_json::to_string(draft)?;
    ensure!(
        bytes.len() <= INFRA_RFC_MAX_BYTES,
        "canonical InfraRFC exceeds 16 KiB"
    );
    Ok(bytes)
}

/// Parse the strict object form shared by the operator and store readback.
pub fn parse_infra_rfc_draft(bytes: &[u8]) -> Result<InfraRFC> {
    ensure!(
        bytes.len() <= INFRA_RFC_MAX_BYTES
            && bytes.iter().find(|b| !b.is_ascii_whitespace()) == Some(&b'{'),
        "InfraRFC draft must be a JSON object of at most 16 KiB"
    );
    let draft: InfraRFC = serde_json::from_slice(bytes)?;
    canonical(&draft)?;
    Ok(draft)
}

fn bound(db: &Connection, repo: &str) -> Result<()> {
    ensure!(
        Path::new(repo).is_absolute() && text_ok(repo, 4096, false),
        "invalid managed InfraRFC repository path"
    );
    let bound = db
        .query_row("SELECT repo FROM managed_repo WHERE singleton=1", [], |r| {
            r.get::<_, String>(0)
        })
        .optional()?;
    ensure!(
        bound.as_deref() == Some(repo),
        "InfraRFC requires this exact bound managed repository"
    );
    Ok(())
}
fn writing_scope(tx: &Transaction<'_>, repo: &str, review: &str) -> Result<()> {
    bound(tx, repo)?;
    ensure!(
        commit(review),
        "InfraRFC requires a full reviewed Git commit ID"
    );
    let ready: bool = tx.query_row(
        "SELECT NOT EXISTS(SELECT 1 FROM promotion_intent) AND NOT EXISTS(SELECT 1 FROM pivot_resume_intent)",
        [],
        |r| r.get(0),
    )?;
    ensure!(
        ready,
        "recover pending promotion or Pivot before changing an InfraRFC"
    );
    Ok(())
}
fn consumer(db: &Connection, draft: &InfraRFC) -> Result<String> {
    let (module, target, state): (String, String, String) = db
        .query_row(
            "SELECT c.module_path,c.formal_target,
                    json_array(c.title,c.informal_statement,c.formal_signature,c.local_trust_level,c.effective_trust_level,
                               c.is_critical_path,c.receipt_id,c.wiki_id,c.updated_at,
                               (SELECT max(e.event_seq) FROM events e WHERE e.entity_type='claim' AND e.entity_id=c.claim_id))
               FROM claims c WHERE c.claim_id=?1 AND c.engine=?2 AND c.module_path IS NOT NULL AND c.formal_target IS NOT NULL
                 AND c.stale=0 AND c.effective_stale=0
                 AND c.local_trust_level NOT IN ('L2a','L2b','L5') AND c.effective_trust_level NOT IN ('L2a','L2b','L5')",
            params![draft.requested_by_claim, draft.engine.as_str()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?
        .context("InfraRFC requires an open nonstale same-engine consumer Claim with a formal target")?;
    ensure!(
        validate_source_path(draft.engine, &module).is_ok()
            && target
                .split_once('#')
                .is_some_and(|(path, decl)| path == module && id(decl, 256)),
        "InfraRFC consumer does not have an exact module#declaration target"
    );
    sha(&(
        "ai4math:infra-rfc-consumer:v1",
        &draft.requested_by_claim,
        draft.engine,
        module,
        target,
        state,
    ))
}
fn draft_sha(
    repo: &str,
    review: &str,
    module: &str,
    draft: &InfraRFC,
    consumer_sha: &str,
) -> Result<String> {
    sha(&(
        "ai4math:infra-rfc-draft:v1",
        repo,
        review,
        module,
        draft,
        consumer_sha,
    ))
}
fn valid_module(module: &str, engine: Engine) -> Result<()> {
    ensure!(
        module.len() <= 256 && validate_source_path(engine, module).is_ok(),
        "invalid InfraRFC target module"
    );
    Ok(())
}
fn event_payload(module: &str, engine: Engine, digest: &str) -> Value {
    json!({"module_path":module,"engine":engine.as_str(),"draft_sha256":digest})
}
#[allow(clippy::too_many_arguments)]
fn transition_event_payload(
    module: &str,
    engine: Engine,
    digest: &str,
    from: InfraRfcStatus,
    to: InfraRfcStatus,
    evidence_sha256: Option<&str>,
    superseded_by: Option<&str>,
    reason: Option<&str>,
) -> Value {
    let mut map = serde_json::Map::new();
    map.insert("module_path".into(), json!(module));
    map.insert("engine".into(), json!(engine.as_str()));
    map.insert("draft_sha256".into(), json!(digest));
    map.insert("from_status".into(), json!(from.as_str()));
    map.insert("to_status".into(), json!(to.as_str()));
    if let Some(evidence) = evidence_sha256 {
        map.insert("evidence_sha256".into(), json!(evidence));
    }
    if let Some(replacement) = superseded_by {
        map.insert("superseded_by_rfc_id".into(), json!(replacement));
    }
    if let Some(reason) = reason {
        map.insert("reason".into(), json!(reason));
    }
    Value::Object(map)
}
fn event_matches(
    db: &Connection,
    seq: i64,
    kind: &str,
    id: &str,
    review: &str,
    payload: &Value,
) -> Result<bool> {
    let row: Option<(String, String)> = db
        .query_row(
            "SELECT actor,payload_json FROM events WHERE event_seq=?1 AND event_type=?2 AND entity_type=?3 AND entity_id=?4 AND git_commit=?5",
            params![seq, kind, ENTITY, id, review],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((actual_actor, raw)) = row else {
        return Ok(false);
    };
    let mut stored: Value = serde_json::from_str(&raw)?;
    let bound_actor = stored
        .get("actor")
        .and_then(Value::as_str)
        .unwrap_or(ACTOR)
        .to_owned();
    let valid_actor = match kind {
        SUBMITTED => matches!(bound_actor.as_str(), ACTOR | "agent:explorer"),
        APPROVED | REJECTED | SUPERSEDED => {
            matches!(bound_actor.as_str(), ACTOR | "agent:chief")
        }
        SMOKE_STARTED => matches!(bound_actor.as_str(), ACTOR | "agent:builder"),
        _ => bound_actor == ACTOR,
    };
    if !valid_actor {
        return Ok(false);
    }
    if actual_actor != bound_actor {
        return Ok(false);
    }
    if stored.get("actor").is_some() {
        stored
            .as_object_mut()
            .context("stored InfraRFC payload is not an object")?
            .remove("actor");
    }
    Ok(&stored == payload)
}

struct Stored {
    row: InfraRfcRecord,
    consumer_sha: String,
    smoke_started_commit: Option<String>,
    superseded_commit: Option<String>,
    superseded_reason: Option<String>,
    rejected_commit: Option<String>,
}

struct TransitionRow {
    event_seq: i64,
    from_status: String,
    to_status: String,
    git_commit: String,
    evidence_sha256: Option<String>,
    superseded_by_rfc_id: Option<String>,
    reason: Option<String>,
}

// The caller owns the surrounding read or write transaction. Keeping every
// projection read on that one connection snapshot prevents mixed event chains.
fn fetch_in(db: &Connection, repo: &str, rfc_id: &str) -> Result<Option<Stored>> {
    let found = db
        .query_row(READ, params![rfc_id, repo], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, Option<i64>>(6)?,
            ))
        })
        .optional()?;
    let mut retained = db.prepare(
        "SELECT event_seq FROM events WHERE entity_type=?1 AND entity_id=?2
          AND event_type IN (?3,?4,?5,?6,?7,?8,?9) ORDER BY event_seq LIMIT 8",
    )?;
    let retained = retained
        .query_map(
            params![
                ENTITY,
                rfc_id,
                SUBMITTED,
                APPROVED,
                SMOKE_STARTED,
                FROZEN,
                COMPLETED,
                SUPERSEDED,
                REJECTED
            ],
            |r| r.get::<_, i64>(0),
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let Some((module, review, digest, consumer_sha, body, submitted, approved)) = found else {
        ensure!(
            retained.is_empty(),
            "InfraRFC events without their immutable draft"
        );
        return Ok(None);
    };
    let mut linked = vec![submitted];
    linked.extend(approved);
    let draft = parse_infra_rfc_draft(body.as_bytes())?;
    valid_module(&module, draft.engine)?;
    ensure!(
        draft.rfc_id == rfc_id
            && commit(&review)
            && hex(&consumer_sha, 64)
            && canonical(&draft)? == body
            && draft_sha(repo, &review, &module, &draft, &consumer_sha)? == digest,
        "stored InfraRFC does not match its immutable draft binding"
    );
    let payload = event_payload(&module, draft.engine, &digest);
    ensure!(
        submitted > 0 && event_matches(db, submitted, SUBMITTED, rfc_id, &review, &payload)?,
        "invalid InfraRFC submission event binding"
    );
    if let Some(seq) = approved {
        ensure!(
            seq > submitted && event_matches(db, seq, APPROVED, rfc_id, &review, &payload)?,
            "invalid InfraRFC approval event binding"
        );
    }

    let mut status = if approved.is_some() {
        InfraRfcStatus::Approved
    } else {
        InfraRfcStatus::Draft
    };
    let mut last_seq = approved.unwrap_or(submitted);
    let mut smoke_started_event_seq = None;
    let mut smoke_started_commit = None;
    let mut smoke_commit = None;
    let mut smoke_receipt_sha256 = None;
    let mut frozen_event_seq = None;
    let mut completion_commit = None;
    let mut completion_receipt_sha256 = None;
    let mut completed_event_seq = None;
    let mut superseded_by_rfc_id = None;
    let mut superseded_event_seq = None;
    let mut superseded_commit = None;
    let mut superseded_reason = None;
    let mut rejected_reason = None;
    let mut rejected_event_seq = None;
    let mut rejected_commit = None;

    let has_transitions: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='infra_rfc_transitions')",
        [],
        |r| r.get(0),
    )?;
    if has_transitions {
        let mut stmt = db.prepare(
            "SELECT event_seq,from_status,to_status,git_commit,evidence_sha256,superseded_by_rfc_id,reason
               FROM infra_rfc_transitions
              WHERE rfc_id=?1
              ORDER BY event_seq ASC",
        )?;
        let transitions = stmt
            .query_map([rfc_id], |r| {
                Ok(TransitionRow {
                    event_seq: r.get(0)?,
                    from_status: r.get(1)?,
                    to_status: r.get(2)?,
                    git_commit: r.get(3)?,
                    evidence_sha256: r.get(4)?,
                    superseded_by_rfc_id: r.get(5)?,
                    reason: r.get(6)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        for t in transitions {
            ensure!(
                !status.is_terminal(),
                "invalid InfraRFC transition after terminal status"
            );
            ensure!(
                t.event_seq > last_seq,
                "invalid InfraRFC transition sequence ordering"
            );
            let from = InfraRfcStatus::parse(&t.from_status)
                .context("invalid InfraRFC transition from_status")?;
            let to = InfraRfcStatus::parse(&t.to_status)
                .context("invalid InfraRFC transition to_status")?;
            ensure!(
                from == status,
                "InfraRFC transition chain mismatch: expected {} but found {}",
                status.as_str(),
                from.as_str()
            );
            ensure!(
                commit(&t.git_commit),
                "invalid InfraRFC transition Git commit"
            );
            let event_type = match to {
                InfraRfcStatus::BuildingSmoke => {
                    ensure!(
                        from == InfraRfcStatus::Approved
                            && t.git_commit == review
                            && t.evidence_sha256.is_none()
                            && t.superseded_by_rfc_id.is_none()
                            && t.reason.is_none(),
                        "invalid BUILDING_SMOKE InfraRFC transition row"
                    );
                    smoke_started_event_seq = Some(t.event_seq);
                    smoke_started_commit = Some(t.git_commit.clone());
                    SMOKE_STARTED
                }
                InfraRfcStatus::FrozenProving => {
                    let receipt = t
                        .evidence_sha256
                        .as_deref()
                        .context("FROZEN_PROVING transition requires smoke receipt")?;
                    ensure!(
                        from == InfraRfcStatus::BuildingSmoke
                            && draft.engine == Engine::Lean4
                            && hex(receipt, 64)
                            && t.superseded_by_rfc_id.is_none()
                            && t.reason.is_none(),
                        "invalid FROZEN_PROVING InfraRFC transition row"
                    );
                    smoke_commit = Some(t.git_commit.clone());
                    smoke_receipt_sha256 = Some(receipt.to_owned());
                    frozen_event_seq = Some(t.event_seq);
                    FROZEN
                }
                InfraRfcStatus::Completed => {
                    let receipt = t
                        .evidence_sha256
                        .as_deref()
                        .context("COMPLETED transition requires completion receipt")?;
                    ensure!(
                        from == InfraRfcStatus::FrozenProving
                            && draft.engine == Engine::Lean4
                            && smoke_commit.as_deref() == Some(t.git_commit.as_str())
                            && hex(receipt, 64)
                            && t.superseded_by_rfc_id.is_none()
                            && t.reason.is_none(),
                        "invalid COMPLETED InfraRFC transition row"
                    );
                    completion_commit = Some(t.git_commit.clone());
                    completion_receipt_sha256 = Some(receipt.to_owned());
                    completed_event_seq = Some(t.event_seq);
                    COMPLETED
                }
                InfraRfcStatus::Rejected => {
                    let reason = t
                        .reason
                        .as_deref()
                        .context("REJECTED transition requires a reason")?;
                    ensure!(
                        matches!(
                            from,
                            InfraRfcStatus::Draft
                                | InfraRfcStatus::Approved
                                | InfraRfcStatus::BuildingSmoke
                        ) && t.evidence_sha256.is_none()
                            && t.superseded_by_rfc_id.is_none()
                            && text_ok(reason, 512, false),
                        "invalid REJECTED InfraRFC transition row"
                    );
                    rejected_reason = Some(reason.to_owned());
                    rejected_event_seq = Some(t.event_seq);
                    rejected_commit = Some(t.git_commit.clone());
                    REJECTED
                }
                InfraRfcStatus::Superseded => {
                    let replacement = t
                        .superseded_by_rfc_id
                        .as_deref()
                        .context("SUPERSEDED transition requires replacement RFC ID")?;
                    let reason = t
                        .reason
                        .as_deref()
                        .context("SUPERSEDED transition requires a reason")?;
                    ensure!(
                        matches!(
                            from,
                            InfraRfcStatus::Approved
                                | InfraRfcStatus::BuildingSmoke
                                | InfraRfcStatus::FrozenProving
                        ) && t.evidence_sha256.is_none()
                            && id(replacement, 100)
                            && replacement != rfc_id
                            && text_ok(reason, 512, false),
                        "invalid SUPERSEDED InfraRFC transition row"
                    );
                    let replacement_valid: bool = db.query_row(
                        "SELECT EXISTS(
                            SELECT 1 FROM infra_rfcs r
                             WHERE r.rfc_id=?1 AND r.repo=?2 AND r.submitted_event_seq<?4
                               AND json_extract(r.draft_json,'$.requested_by_claim')=?3
                               AND NOT EXISTS (
                                   SELECT 1 FROM events e WHERE e.entity_type=?5 AND e.entity_id=r.rfc_id
                                     AND e.event_seq<?4 AND e.event_type IN (?6,?7)
                               )
                         )",
                        params![replacement, repo, &draft.requested_by_claim, t.event_seq, ENTITY, REJECTED, SUPERSEDED],
                        |r| r.get(0),
                    )?;
                    ensure!(
                        replacement_valid,
                        "SUPERSEDED transition replacement RFC was not valid for this repository and consumer at supersession"
                    );
                    superseded_by_rfc_id = Some(replacement.to_owned());
                    superseded_event_seq = Some(t.event_seq);
                    superseded_commit = Some(t.git_commit.clone());
                    superseded_reason = Some(reason.to_owned());
                    SUPERSEDED
                }
                InfraRfcStatus::Draft | InfraRfcStatus::Approved => {
                    bail!("invalid InfraRFC transition target status");
                }
            };

            let t_payload = transition_event_payload(
                &module,
                draft.engine,
                &digest,
                from,
                to,
                t.evidence_sha256.as_deref(),
                t.superseded_by_rfc_id.as_deref(),
                t.reason.as_deref(),
            );
            ensure!(
                event_matches(
                    db,
                    t.event_seq,
                    event_type,
                    rfc_id,
                    &t.git_commit,
                    &t_payload
                )?,
                "invalid InfraRFC transition event binding"
            );
            linked.push(t.event_seq);
            status = to;
            last_seq = t.event_seq;
        }
    }
    ensure!(
        retained == linked,
        "InfraRFC event history has missing projection rows"
    );

    Ok(Some(Stored {
        consumer_sha,
        smoke_started_commit,
        superseded_commit,
        superseded_reason,
        rejected_commit,
        row: InfraRfcRecord {
            schema: "ai4math.infra-rfc.v1",
            module_path: module,
            reviewed_commit: review,
            draft_sha256: digest,
            status,
            draft,
            submitted_event_seq: submitted,
            approved_event_seq: approved,
            smoke_started_event_seq,
            smoke_commit,
            smoke_receipt_sha256,
            frozen_event_seq,
            completion_commit,
            completion_receipt_sha256,
            completed_event_seq,
            superseded_by_rfc_id,
            superseded_event_seq,
            rejected_reason,
            rejected_event_seq,
        },
    }))
}

fn read_infra_rfc(db: &Connection, repo: &str, rfc_id: &str) -> Result<Option<InfraRfcRecord>> {
    bound(db, repo)?;
    let table: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='infra_rfcs')",
        [],
        |r| r.get(0),
    )?;
    Ok(if table {
        fetch_in(db, repo, rfc_id)?.map(|stored| stored.row)
    } else {
        None
    })
}

#[allow(clippy::too_many_arguments)]
fn record_transition(
    tx: &Transaction<'_>,
    repo: &str,
    row: &InfraRfcRecord,
    to: InfraRfcStatus,
    event_type: &str,
    git_commit: &str,
    evidence_sha256: Option<&str>,
    superseded_by: Option<&str>,
    reason: Option<&str>,
    confirm_git: impl FnOnce() -> Result<()>,
) -> Result<InfraRfcRecord> {
    record_transition_actor(
        tx,
        repo,
        row,
        to,
        event_type,
        ACTOR,
        git_commit,
        evidence_sha256,
        superseded_by,
        reason,
        confirm_git,
    )
}

#[allow(clippy::too_many_arguments)]
fn record_transition_actor(
    tx: &Transaction<'_>,
    repo: &str,
    row: &InfraRfcRecord,
    to: InfraRfcStatus,
    event_type: &str,
    actor: &str,
    git_commit: &str,
    evidence_sha256: Option<&str>,
    superseded_by: Option<&str>,
    reason: Option<&str>,
    confirm_git: impl FnOnce() -> Result<()>,
) -> Result<InfraRfcRecord> {
    let mut payload = transition_event_payload(
        &row.module_path,
        row.draft.engine,
        &row.draft_sha256,
        row.status,
        to,
        evidence_sha256,
        superseded_by,
        reason,
    );
    if actor != ACTOR {
        payload["actor"] = json!(actor);
    }
    let seq = event(
        tx,
        Change {
            actor,
            git_commit: Some(git_commit),
        },
        event_type,
        ENTITY,
        &row.draft.rfc_id,
        payload,
    )?;
    tx.execute(
        "INSERT INTO infra_rfc_transitions(event_seq,rfc_id,from_status,to_status,git_commit,evidence_sha256,superseded_by_rfc_id,reason) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
        params![
            seq,
            &row.draft.rfc_id,
            row.status.as_str(),
            to.as_str(),
            git_commit,
            evidence_sha256,
            superseded_by,
            reason,
        ],
    )?;
    let updated = fetch_in(tx, repo, &row.draft.rfc_id)?
        .context("updated InfraRFC disappeared")?
        .row;
    confirm_git()?;
    Ok(updated)
}

impl StateStore {
    /// Read a historical RFC for exactly the bound repository. This is a description, not launch authorization.
    pub fn infra_rfc(&self, repo: &str, rfc_id: &str) -> Result<Option<InfraRfcRecord>> {
        ensure!(id(rfc_id, 100), "invalid InfraRFC ID");
        let tx = self.db.unchecked_transaction()?;
        let record = read_infra_rfc(&tx, repo, rfc_id)?;
        tx.commit()?;
        Ok(record)
    }

    /// Caller holds the Git lock and confirms the actual reviewed head immediately before SQLite commit.
    pub fn submit_infra_rfc(
        &mut self,
        repo: &str,
        review: &str,
        module: &str,
        draft: &InfraRFC,
        confirm_git: impl FnOnce() -> Result<()>,
    ) -> Result<InfraRfcRecord> {
        self.submit_infra_rfc_actor(ACTOR, repo, review, module, draft, confirm_git)
    }

    /// Coordinator-facing variant. The caller still has to hold the managed
    /// Git lock and prove the reviewed snapshot through `confirm_git`.
    pub fn submit_infra_rfc_actor(
        &mut self,
        actor: &str,
        repo: &str,
        review: &str,
        module: &str,
        draft: &InfraRFC,
        confirm_git: impl FnOnce() -> Result<()>,
    ) -> Result<InfraRfcRecord> {
        let body = canonical(draft)?;
        valid_module(module, draft.engine)?;
        self.write(|tx| {
            writing_scope(tx, repo, review)?;
            let consumer_sha = consumer(tx, draft)?;
            let digest = draft_sha(repo, review, module, draft, &consumer_sha)?;
            let rfc = &draft.rfc_id;
            if let Some(stored) = fetch_in(tx, repo, rfc)? {
                ensure!(
                    stored.row.draft_sha256 == digest,
                    "InfraRFC ID already identifies a different immutable draft or review scope"
                );
                confirm_git()?;
                return Ok(stored.row);
            }
            let mut payload = event_payload(module, draft.engine, &digest);
            if actor != ACTOR {
                payload["actor"] = json!(actor);
            }
            let seq = event(
                tx,
                Change {
                    actor,
                    git_commit: Some(review),
                },
                SUBMITTED,
                ENTITY,
                rfc,
                payload,
            )?;
            tx.execute(
                "INSERT INTO infra_rfcs(rfc_id,repo,module_path,reviewed_commit,draft_sha256,consumer_sha256,draft_json,submitted_event_seq) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
                params![rfc, repo, module, review, digest, consumer_sha, body, seq],
            )?;
            let row = fetch_in(tx, repo, rfc)?
                .context("submitted InfraRFC disappeared")?
                .row;
            confirm_git()?;
            Ok(row)
        })
    }

    /// Approval is compare-and-set against the draft and consumer snapshot, not a build readiness flag.
    pub fn approve_infra_rfc(
        &mut self,
        repo: &str,
        review: &str,
        rfc_id: &str,
        approval: &str,
        confirm_git: impl FnOnce() -> Result<()>,
    ) -> Result<InfraRfcRecord> {
        self.approve_infra_rfc_actor(ACTOR, repo, review, rfc_id, approval, confirm_git)
    }

    pub fn approve_infra_rfc_actor(
        &mut self,
        actor: &str,
        repo: &str,
        review: &str,
        rfc_id: &str,
        approval: &str,
        confirm_git: impl FnOnce() -> Result<()>,
    ) -> Result<InfraRfcRecord> {
        ensure!(
            id(rfc_id, 100) && hex(approval, 64),
            "invalid InfraRFC ID or approval digest"
        );
        self.write(|tx| {
            writing_scope(tx, repo, review)?;
            let stored = fetch_in(tx, repo, rfc_id)?.context("unknown InfraRFC")?;
            let row = &stored.row;
            ensure!(
                row.reviewed_commit == review && row.draft_sha256 == approval,
                "InfraRFC approval does not match the exact reviewed draft"
            );
            if row.status == InfraRfcStatus::Approved {
                // Exact replay is independent of later Claim events. Only a new
                // DRAFT -> APPROVED conversion must still match its consumer.
                confirm_git()?;
                return Ok(stored.row);
            }
            ensure!(
                row.status == InfraRfcStatus::Draft && row.approved_event_seq.is_none(),
                "InfraRFC can only be approved from DRAFT status"
            );
            ensure!(
                consumer(tx, &row.draft)? == stored.consumer_sha,
                "InfraRFC consumer changed since submission"
            );
            let mut payload = event_payload(&row.module_path, row.draft.engine, approval);
            if actor != ACTOR {
                payload["actor"] = json!(actor);
            }
            let seq = event(
                tx,
                Change {
                    actor,
                    git_commit: Some(review),
                },
                APPROVED,
                ENTITY,
                rfc_id,
                payload,
            )?;
            ensure!(
                tx.execute(
                    "UPDATE infra_rfcs SET approved_event_seq=?2 WHERE rfc_id=?1 AND approved_event_seq IS NULL",
                    params![rfc_id, seq],
                )? == 1,
                "InfraRFC approval row changed"
            );
            let approved = fetch_in(tx, repo, rfc_id)?
                .context("approved InfraRFC disappeared")?
                .row;
            confirm_git()?;
            Ok(approved)
        })
    }

    /// Transition an APPROVED InfraRFC into BUILDING_SMOKE.
    pub fn start_infra_rfc_smoke(
        &mut self,
        repo: &str,
        reviewed_main: &str,
        rfc_id: &str,
        draft_sha256: &str,
        confirm_git: impl FnOnce() -> Result<()>,
    ) -> Result<InfraRfcRecord> {
        self.start_infra_rfc_smoke_actor(
            ACTOR,
            repo,
            reviewed_main,
            rfc_id,
            draft_sha256,
            confirm_git,
        )
    }

    pub fn start_infra_rfc_smoke_actor(
        &mut self,
        actor: &str,
        repo: &str,
        reviewed_main: &str,
        rfc_id: &str,
        draft_sha256: &str,
        confirm_git: impl FnOnce() -> Result<()>,
    ) -> Result<InfraRfcRecord> {
        ensure!(
            id(rfc_id, 100) && hex(draft_sha256, 64),
            "invalid InfraRFC ID or draft digest"
        );
        self.write(|tx| {
            writing_scope(tx, repo, reviewed_main)?;
            let stored = fetch_in(tx, repo, rfc_id)?.context("unknown InfraRFC")?;
            let row = &stored.row;
            ensure!(
                row.reviewed_commit == reviewed_main && row.draft_sha256 == draft_sha256,
                "InfraRFC smoke start does not match the exact reviewed draft"
            );
            if row.status == InfraRfcStatus::BuildingSmoke {
                ensure!(
                    stored.smoke_started_commit.as_deref() == Some(reviewed_main),
                    "InfraRFC smoke build already started at a different commit"
                );
                confirm_git()?;
                return Ok(stored.row);
            }
            ensure!(
                row.status == InfraRfcStatus::Approved,
                "InfraRFC smoke build can only start from APPROVED status"
            );
            ensure!(
                consumer(tx, &row.draft)? == stored.consumer_sha,
                "InfraRFC consumer changed since submission"
            );
            record_transition_actor(
                tx,
                repo,
                row,
                InfraRfcStatus::BuildingSmoke,
                SMOKE_STARTED,
                actor,
                reviewed_main,
                None,
                None,
                None,
                confirm_git,
            )
        })
    }

    /// Transition a BUILDING_SMOKE Lean 4 InfraRFC into FROZEN_PROVING using sealed broker smoke evidence.
    pub fn freeze_infra_rfc(
        &mut self,
        repo: &str,
        rfc_id: &str,
        draft_sha256: &str,
        smoke: &dyn InfraSmokeEvidence,
        confirm_git: impl FnOnce() -> Result<()>,
    ) -> Result<InfraRfcRecord> {
        self.freeze_infra_rfc_verified(
            repo,
            rfc_id,
            draft_sha256,
            smoke.commit(),
            smoke.module_path(),
            smoke.definitions(),
            smoke.receipt_sha256(),
            confirm_git,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn freeze_infra_rfc_verified(
        &mut self,
        repo: &str,
        rfc_id: &str,
        draft_sha256: &str,
        smoke_commit: &str,
        smoke_module: &str,
        smoke_definitions: &[String],
        smoke_receipt_sha256: &str,
        confirm_git: impl FnOnce() -> Result<()>,
    ) -> Result<InfraRfcRecord> {
        ensure!(
            id(rfc_id, 100) && hex(draft_sha256, 64),
            "invalid InfraRFC ID or draft digest"
        );
        ensure!(
            commit(smoke_commit) && hex(smoke_receipt_sha256, 64),
            "invalid verified InfraRFC smoke evidence"
        );
        self.write(|tx| {
            writing_scope(tx, repo, smoke_commit)?;
            let stored = fetch_in(tx, repo, rfc_id)?.context("unknown InfraRFC")?;
            let row = &stored.row;
            ensure!(
                row.draft_sha256 == draft_sha256,
                "InfraRFC draft digest does not match"
            );
            ensure!(
                row.draft.engine == Engine::Lean4,
                "InfraRFC smoke verification requires Lean 4"
            );
            ensure!(
                smoke_module == row.module_path,
                "verified smoke module does not match InfraRFC module"
            );
            ensure!(
                smoke_definitions == row.draft.proposed_definitions.as_slice(),
                "verified smoke definitions do not match InfraRFC proposed_definitions"
            );
            if row.status == InfraRfcStatus::FrozenProving {
                ensure!(
                    row.smoke_commit.as_deref() == Some(smoke_commit)
                        && row.smoke_receipt_sha256.as_deref() == Some(smoke_receipt_sha256),
                    "InfraRFC already frozen with different smoke evidence"
                );
                confirm_git()?;
                return Ok(stored.row);
            }
            ensure!(
                row.status == InfraRfcStatus::BuildingSmoke,
                "InfraRFC can only freeze from BUILDING_SMOKE status"
            );
            record_transition(
                tx,
                repo,
                row,
                InfraRfcStatus::FrozenProving,
                FROZEN,
                smoke_commit,
                Some(smoke_receipt_sha256),
                None,
                None,
                confirm_git,
            )
        })
    }

    /// Transition a FROZEN_PROVING Lean 4 InfraRFC into COMPLETED using sealed broker completion evidence.
    pub fn complete_infra_rfc(
        &mut self,
        repo: &str,
        rfc_id: &str,
        draft_sha256: &str,
        completion: &dyn InfraCompletionEvidence,
        confirm_git: impl FnOnce() -> Result<()>,
    ) -> Result<InfraRfcRecord> {
        self.complete_infra_rfc_verified(
            repo,
            rfc_id,
            draft_sha256,
            completion.commit(),
            completion.module_path(),
            completion.definitions(),
            completion.api_lemmas(),
            completion.receipt_sha256(),
            confirm_git,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn complete_infra_rfc_verified(
        &mut self,
        repo: &str,
        rfc_id: &str,
        draft_sha256: &str,
        completion_commit: &str,
        completion_module: &str,
        completion_definitions: &[String],
        completion_api_lemmas: &[String],
        completion_receipt_sha256: &str,
        confirm_git: impl FnOnce() -> Result<()>,
    ) -> Result<InfraRfcRecord> {
        ensure!(
            id(rfc_id, 100) && hex(draft_sha256, 64),
            "invalid InfraRFC ID or draft digest"
        );
        ensure!(
            commit(completion_commit) && hex(completion_receipt_sha256, 64),
            "invalid verified InfraRFC completion evidence"
        );
        self.write(|tx| {
            writing_scope(tx, repo, completion_commit)?;
            let stored = fetch_in(tx, repo, rfc_id)?.context("unknown InfraRFC")?;
            let row = &stored.row;
            ensure!(
                row.draft_sha256 == draft_sha256,
                "InfraRFC draft digest does not match"
            );
            ensure!(
                row.draft.engine == Engine::Lean4,
                "InfraRFC completion verification requires Lean 4"
            );
            ensure!(
                completion_module == row.module_path,
                "verified completion module does not match InfraRFC module"
            );
            ensure!(
                completion_definitions == row.draft.proposed_definitions.as_slice(),
                "verified completion definitions do not match InfraRFC proposed_definitions"
            );
            ensure!(
                completion_api_lemmas == row.draft.required_api_lemmas.as_slice(),
                "verified completion API lemmas do not match InfraRFC required_api_lemmas"
            );
            if row.status == InfraRfcStatus::Completed {
                ensure!(
                    row.smoke_commit.as_deref() == Some(completion_commit)
                        && row.completion_commit.as_deref() == Some(completion_commit)
                        && row.completion_receipt_sha256.as_deref()
                            == Some(completion_receipt_sha256),
                    "InfraRFC already completed with different completion evidence"
                );
                confirm_git()?;
                return Ok(stored.row);
            }
            ensure!(
                row.status == InfraRfcStatus::FrozenProving,
                "InfraRFC can only complete from FROZEN_PROVING status"
            );
            ensure!(
                row.smoke_commit.as_deref() == Some(completion_commit),
                "InfraRFC completion commit must exactly match frozen smoke commit"
            );
            record_transition(
                tx,
                repo,
                row,
                InfraRfcStatus::Completed,
                COMPLETED,
                completion_commit,
                Some(completion_receipt_sha256),
                None,
                None,
                confirm_git,
            )
        })
    }

    /// Reject a DRAFT, APPROVED, or BUILDING_SMOKE InfraRFC with a bounded single-line reason.
    pub fn reject_infra_rfc(
        &mut self,
        repo: &str,
        reviewed_main: &str,
        rfc_id: &str,
        draft_sha256: &str,
        reason: &str,
        confirm_git: impl FnOnce() -> Result<()>,
    ) -> Result<InfraRfcRecord> {
        self.reject_infra_rfc_actor(
            ACTOR,
            repo,
            reviewed_main,
            rfc_id,
            draft_sha256,
            reason,
            confirm_git,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn reject_infra_rfc_actor(
        &mut self,
        actor: &str,
        repo: &str,
        reviewed_main: &str,
        rfc_id: &str,
        draft_sha256: &str,
        reason: &str,
        confirm_git: impl FnOnce() -> Result<()>,
    ) -> Result<InfraRfcRecord> {
        ensure!(
            id(rfc_id, 100) && hex(draft_sha256, 64),
            "invalid InfraRFC ID or draft digest"
        );
        ensure!(
            text_ok(reason, 512, false),
            "invalid InfraRFC rejection reason"
        );
        self.write(|tx| {
            writing_scope(tx, repo, reviewed_main)?;
            let stored = fetch_in(tx, repo, rfc_id)?.context("unknown InfraRFC")?;
            let row = &stored.row;
            ensure!(
                row.draft_sha256 == draft_sha256,
                "InfraRFC draft digest does not match"
            );
            if row.status == InfraRfcStatus::Rejected {
                ensure!(
                    row.rejected_reason.as_deref() == Some(reason)
                        && stored.rejected_commit.as_deref() == Some(reviewed_main),
                    "InfraRFC already rejected with different reason or commit"
                );
                confirm_git()?;
                return Ok(stored.row);
            }
            ensure!(
                matches!(
                    row.status,
                    InfraRfcStatus::Draft
                        | InfraRfcStatus::Approved
                        | InfraRfcStatus::BuildingSmoke
                ),
                "InfraRFC can only be rejected from DRAFT, APPROVED, or BUILDING_SMOKE status"
            );
            record_transition_actor(
                tx,
                repo,
                row,
                InfraRfcStatus::Rejected,
                REJECTED,
                actor,
                reviewed_main,
                None,
                None,
                Some(reason),
                confirm_git,
            )
        })
    }

    /// Supersede an APPROVED, BUILDING_SMOKE, or FROZEN_PROVING InfraRFC with a live same-claim replacement RFC.
    #[allow(clippy::too_many_arguments)]
    pub fn supersede_infra_rfc(
        &mut self,
        repo: &str,
        reviewed_main: &str,
        rfc_id: &str,
        draft_sha256: &str,
        replacement_rfc_id: &str,
        reason: &str,
        confirm_git: impl FnOnce() -> Result<()>,
    ) -> Result<InfraRfcRecord> {
        self.supersede_infra_rfc_actor(
            ACTOR,
            repo,
            reviewed_main,
            rfc_id,
            draft_sha256,
            replacement_rfc_id,
            reason,
            confirm_git,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn supersede_infra_rfc_actor(
        &mut self,
        actor: &str,
        repo: &str,
        reviewed_main: &str,
        rfc_id: &str,
        draft_sha256: &str,
        replacement_rfc_id: &str,
        reason: &str,
        confirm_git: impl FnOnce() -> Result<()>,
    ) -> Result<InfraRfcRecord> {
        ensure!(
            id(rfc_id, 100) && id(replacement_rfc_id, 100) && hex(draft_sha256, 64),
            "invalid InfraRFC ID, replacement RFC ID, or draft digest"
        );
        ensure!(
            replacement_rfc_id != rfc_id,
            "InfraRFC cannot supersede itself"
        );
        ensure!(
            text_ok(reason, 512, false),
            "invalid InfraRFC supersession reason"
        );
        self.write(|tx| {
            writing_scope(tx, repo, reviewed_main)?;
            let stored = fetch_in(tx, repo, rfc_id)?.context("unknown InfraRFC")?;
            let row = &stored.row;
            ensure!(
                row.draft_sha256 == draft_sha256,
                "InfraRFC draft digest does not match"
            );
            if row.status == InfraRfcStatus::Superseded {
                ensure!(
                    row.superseded_by_rfc_id.as_deref() == Some(replacement_rfc_id)
                        && stored.superseded_reason.as_deref() == Some(reason)
                        && stored.superseded_commit.as_deref() == Some(reviewed_main),
                    "InfraRFC already superseded with different replacement, reason, or commit"
                );
                confirm_git()?;
                return Ok(stored.row);
            }
            ensure!(
                matches!(
                    row.status,
                    InfraRfcStatus::Approved
                        | InfraRfcStatus::BuildingSmoke
                        | InfraRfcStatus::FrozenProving
                ),
                "InfraRFC can only be superseded from APPROVED, BUILDING_SMOKE, or FROZEN_PROVING status"
            );
            let replacement = fetch_in(tx, repo, replacement_rfc_id)?
                .context("replacement InfraRFC not found in this repository")?;
            ensure!(
                replacement.row.draft.requested_by_claim == row.draft.requested_by_claim,
                "replacement InfraRFC must target the same consumer Claim"
            );
            ensure!(
                !matches!(
                    replacement.row.status,
                    InfraRfcStatus::Rejected | InfraRfcStatus::Superseded
                ),
                "replacement InfraRFC cannot be REJECTED or SUPERSEDED"
            );
            record_transition_actor(
                tx,
                repo,
                row,
                InfraRfcStatus::Superseded,
                SUPERSEDED,
                actor,
                reviewed_main,
                None,
                Some(replacement_rfc_id),
                Some(reason),
                confirm_git,
            )
        })
    }
}

#[cfg(test)]
mod tests;
