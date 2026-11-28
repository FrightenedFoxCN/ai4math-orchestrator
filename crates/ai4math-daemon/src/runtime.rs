use crate::{
    facts::{self, FactPack},
    proofs::{self, Probes},
    t2_audit::{self, AuditState},
    t3_discovery::{self, DiscoveryState},
    t3_worker,
    transport::{self, Connection, Hub, RpcError},
};
use ai4math_core::{
    Engine, InfraRFC, InfraStatus,
    contracts::{DecisionCategory, DecisionOption, DecisionPriority},
    validate_source_path,
};
use ai4math_git::ManagedGit;
use ai4math_store::{
    Change, IdempotencyConflict, NewClaim, NewDecision, NewPlan, NewTask, PivotLessonWarning,
    PlanStatus, PromotionRecoverySummary, PromotionStatus, StateStore, TaskTrack,
    TrustTransitionError, Wiki, WikiManagedGeneration, WikiQuery, WikiStratum, WorkerAttemptError,
    WorkerSuggestion,
};
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::{
    fs::{self, DirBuilder, OpenOptions, Permissions},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

pub(crate) mod managed_wiki;
#[cfg(test)]
mod route_lessons_tests;

const TOPICS: [&str; 3] = ["vital_signs.tick", "alert.p0", "breaker.state_changed"];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Retained {
    id: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct List {
    limit: Option<usize>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AlertList {
    limit: Option<usize>,
    #[serde(default)]
    after: i64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Topics {
    topics: Vec<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WikiSearchArgs {
    text: String,
    #[serde(default, deserialize_with = "wiki_optional")]
    scope: Option<ai4math_store::WikiStratum>,
    #[serde(default, deserialize_with = "wiki_optional")]
    engine: Option<ai4math_store::ClaimEngine>,
    #[serde(default, deserialize_with = "wiki_optional")]
    conclusion_head: Option<String>,
    #[serde(default)]
    hypothesis_heads: Vec<String>,
    #[serde(default)]
    typeclasses: Vec<String>,
    #[serde(default, deserialize_with = "wiki_optional")]
    include_shadow_hint: Option<bool>,
    #[serde(default, deserialize_with = "wiki_optional")]
    limit: Option<usize>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WikiGraveyardArgs {
    informal_statement: String,
    #[serde(default, deserialize_with = "wiki_optional")]
    formal_signature: Option<String>,
    #[serde(default, deserialize_with = "wiki_optional")]
    limit: Option<usize>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WikiProvenanceArgs {
    id: String,
    #[serde(default, deserialize_with = "wiki_optional")]
    max_depth: Option<u32>,
    #[serde(default, deserialize_with = "wiki_optional")]
    limit: Option<usize>,
}

fn wiki_optional<'de, D, T>(input: D) -> std::result::Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    // An optional key may be absent, but an explicit null is not its declared type.
    T::deserialize(input).map(Some)
}

fn wiki_text(field: &str, value: &str, max: usize) -> std::result::Result<(), RpcError> {
    if value.len() <= max && !value.trim().is_empty() && !value.contains('\0') {
        Ok(())
    } else {
        Err(RpcError::new(
            -32602,
            format!("{field} must be nonblank, at most {max} UTF-8 bytes, and without NUL"),
        ))
    }
}

fn wiki_symbols<'a>(
    field: &str,
    values: &'a [String],
) -> std::result::Result<Vec<&'a str>, RpcError> {
    let mut seen = std::collections::BTreeSet::new();
    if values.len() > 8 {
        return Err(RpcError::new(
            -32602,
            format!("{field} allows at most 8 symbols"),
        ));
    }
    for value in values {
        wiki_text(field, value, 128)?;
        if !seen.insert(value.as_str()) {
            return Err(RpcError::new(
                -32602,
                format!("{field} requires distinct symbols"),
            ));
        }
    }
    Ok(values.iter().map(String::as_str).collect())
}

// Indexed Markdown fields and edge metadata can be much larger than the row limit
// suggests. Keep the wire reply below the tool/frame budgets without splitting JSON
// or modifying IDs, proof signatures, or provenance edges in flight.
const WIKI_REPLY_BYTES: usize = 64 * 1024;
struct WikiBudget(usize);
impl std::io::Write for WikiBudget {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.0 = self
            .0
            .checked_sub(data.len())
            .ok_or_else(|| std::io::Error::other("Wiki reply budget exceeded"))?;
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn wiki_fit(value: &impl serde::Serialize, bytes: usize) -> Option<usize> {
    let mut budget = WikiBudget(bytes);
    serde_json::to_writer(&mut budget, value).ok()?;
    Some(budget.0)
}
fn wiki_value_depth(value: &Value, remaining: usize) -> bool {
    let next = remaining.checked_sub(1);
    let fits_child = |child: &Value| next.is_some_and(|next| wiki_value_depth(child, next));
    match value {
        Value::Array(items) => items.iter().all(fits_child),
        Value::Object(fields) => fields.values().all(fits_child),
        _ => true,
    }
}
fn wiki_page<T: serde::Serialize>(
    key: &str,
    rows: Vec<T>,
    limit: usize,
    mut truncated: bool,
    mut hint: Option<ai4math_store::ShadowHint>,
) -> std::result::Result<Value, RpcError> {
    truncated |= rows.len() > limit;
    // Reserve space for the enclosing object and cap the teaser separately so
    // a pathological cold title cannot crowd out ordinary search hits.
    let mut remaining = WIKI_REPLY_BYTES - 128;
    let mut page = Map::new();
    if let Some(hint) = &mut hint {
        if wiki_fit(hint, 2048).is_none() {
            truncated = true;
            hint.top_sediment = None;
            hint.top_graveyard_id = None;
        }
        remaining -= 2048;
        page.insert("shadow_hint".into(), response(hint)?);
    }
    let mut bounded = Vec::new();
    for row in rows.into_iter().take(limit) {
        let Some(next) = wiki_fit(&row, remaining.saturating_sub(1)) else {
            truncated = true;
            break;
        };
        let row = response(row)?;
        // Leave nesting headroom for the Host NDJSON and Gateway envelopes.
        if !wiki_value_depth(&row, 48) {
            truncated = true;
            break;
        }
        remaining = next;
        bounded.push(row);
    }
    page.insert(key.into(), Value::Array(bounded));
    page.insert("truncated".into(), Value::Bool(truncated));
    Ok(Value::Object(page))
}

const ROUTE_LESSON_MAX_ROWS: usize = 16;
// Worst-case punctuation and fields added to an existing, nonempty page object.
const ROUTE_LESSON_FIELDS_BYTES: usize =
    ",\"route_lessons\":[],\"route_lessons_truncated\":false".len();

fn wiki_add_route_lessons(
    page: &mut Value,
    mut rows: Vec<PivotLessonWarning>,
    limit: usize,
    mut truncated: bool,
) -> std::result::Result<(), RpcError> {
    if rows.is_empty() && !truncated {
        return Ok(());
    }
    rows.sort_unstable_by(|a, b| {
        b.event_seq
            .cmp(&a.event_seq)
            .then_with(|| a.lesson_id.cmp(&b.lesson_id))
    });
    rows.dedup_by(|a, b| a.lesson_id == b.lesson_id);
    truncated |= rows.len() > limit;

    // Do not reduce the static warning/search page to make room for the new fields.
    let mut remaining = wiki_fit(page, WIKI_REPLY_BYTES - ROUTE_LESSON_FIELDS_BYTES)
        .context("static Wiki page left insufficient reply metadata space")
        .map_err(internal)?;
    let mut bounded = Vec::new();
    for row in rows.into_iter().take(limit) {
        let Some(next) = wiki_fit(&row, remaining.saturating_sub(1)) else {
            truncated = true;
            break;
        };
        remaining = next;
        bounded.push(response(row)?);
    }
    page["route_lessons"] = Value::Array(bounded);
    page["route_lessons_truncated"] = Value::Bool(truncated);
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Proof {
    target_id: String,
    proof_body: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClaimCreate {
    claim_id: String,
    title: String,
    informal_statement: String,
    module_path: String,
    target_decl: String,
    #[serde(default)]
    critical: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FormalSketchArgs {
    task_id: String,
    observation_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClaimOracleTransition {
    claim_id: String,
    observation_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClaimRead {
    claim_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PlanDependencyInput {
    claim_id: String,
    kind: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PlanCreate {
    plan_id: String,
    target_claim_id: String,
    name: String,
    track: String,
    #[serde(default)]
    dependencies: Vec<PlanDependencyInput>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PlanRead {
    plan_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InboxPush {
    inbox_id: String,
    priority: String,
    category: String,
    summary: String,
    #[serde(default)]
    leverage_score: f64,
    #[serde(default)]
    related_claims: Vec<String>,
    #[serde(default)]
    evidence_refs: Vec<String>,
    #[serde(default)]
    options: Vec<DecisionOption>,
    #[serde(default)]
    details: Map<String, Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PromotionApprovalArgs {
    task_id: String,
    request_id: String,
    expires_unix: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PromotionExecuteArgs {
    task_id: String,
    request_id: String,
    expires_unix: u64,
    signature_hex: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PromotionPreviewArgs {
    task_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkerAcquire {
    request_id: String,
    wall_seconds: i64,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum WorkerSuggestionInput {
    ProofBody { proof_body: String },
    Review { verdict: String, rationale: String },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkerSubmit {
    attempt_id: String,
    suggestion: WorkerSuggestionInput,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkerAttemptRef {
    attempt_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskRegister {
    task_id: String,
    track: String,
    module_path: String,
    target_decl: String,
    #[serde(default)]
    dependencies: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RfcSubmit {
    module_path: String,
    reviewed_commit: String,
    draft: InfraRFC,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RfcInspect {
    rfc_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RfcFreeze {
    rfc_id: String,
    reviewed_commit: String,
    draft_sha256: String,
    smoke_lemmas: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RfcComplete {
    rfc_id: String,
    reviewed_commit: String,
    draft_sha256: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RfcReject {
    rfc_id: String,
    reviewed_commit: String,
    draft_sha256: String,
    reason: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RfcSupersede {
    rfc_id: String,
    reviewed_commit: String,
    draft_sha256: String,
    replacement_rfc_id: String,
    reason: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RfcApprove {
    rfc_id: String,
    reviewed_commit: String,
    draft_sha256: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionBind {
    role: String,
    #[serde(default)]
    binding_id: Option<String>,
}

const WRITE_ROLES: [&str; 8] = [
    "owner",
    "chief",
    "regional",
    "explorer",
    "builder",
    "prover",
    "automation",
    "reviewer",
];

fn require_write_role(role: &str, allowed: &[&str], method: &str) -> Result<(), RpcError> {
    if !WRITE_ROLES.contains(&role) || !allowed.contains(&role) {
        return Err(RpcError::new(
            -32003,
            format!("{method} is not allowed for this role"),
        ));
    }
    Ok(())
}

fn actor(role: &str) -> String {
    if role == "owner" {
        "operator:rpc".into()
    } else {
        format!("agent:{role}")
    }
}

fn parse<T: serde::de::DeserializeOwned>(
    map: Map<String, Value>,
) -> std::result::Result<T, RpcError> {
    serde_json::from_value(Value::Object(map)).map_err(|e| RpcError::new(-32602, e.to_string()))
}
fn rpc_id(field: &str, value: &str, max: usize) -> Result<(), RpcError> {
    if value.is_empty()
        || value.len() > max
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b':'))
    {
        return Err(RpcError::new(-32602, format!("invalid {field}")));
    }
    Ok(())
}

fn rpc_commit(value: &str) -> Result<(), RpcError> {
    if !matches!(value.len(), 40 | 64)
        || !value.bytes().any(|b| b != b'0')
        || !value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(RpcError::new(-32602, "invalid reviewed Git commit"));
    }
    Ok(())
}

fn rpc_sha256(value: &str) -> Result<(), RpcError> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(RpcError::new(-32602, "invalid draft SHA-256"));
    }
    Ok(())
}

fn rpc_reason(value: &str) -> Result<(), RpcError> {
    if value.is_empty()
        || value.len() > 512
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        return Err(RpcError::new(-32602, "invalid RFC transition reason"));
    }
    Ok(())
}

fn rpc_text(field: &str, value: &str, max: usize) -> Result<(), RpcError> {
    if value.trim().is_empty() || value.len() > max || value.contains('\0') {
        return Err(RpcError::new(
            -32602,
            format!("{field} must be nonblank, at most {max} UTF-8 bytes, and without NUL"),
        ));
    }
    Ok(())
}

fn cap(arg: Option<usize>, default: usize, max: usize) -> std::result::Result<usize, RpcError> {
    let n = arg.unwrap_or(default);
    if (1..=max).contains(&n) {
        Ok(n)
    } else {
        Err(RpcError::new(-32602, format!("limit must be in 1..={max}")))
    }
}
fn internal(error: anyhow::Error) -> RpcError {
    eprintln!("ai4mathd: {error:#}");
    RpcError::new(-32603, "local state operation failed")
}

fn owner_promotion_error(error: anyhow::Error) -> RpcError {
    eprintln!("ai4mathd Owner promotion: {error:#}");
    RpcError::new(
        -32003,
        "Owner promotion authorization is invalid, expired, or unavailable",
    )
}

fn native_rfc_error(error: anyhow::Error) -> RpcError {
    eprintln!("ai4mathd native RFC: {error:#}");
    RpcError::new(
        -32006,
        "native RFC verification failed or trusted tooling is unavailable",
    )
}

fn classify_promotion_status(git: &ManagedGit, status: &mut PromotionStatus) {
    if let Some(intent) = &status.pending_intent {
        let kind = if status.current_head == intent.base_commit {
            "prepared"
        } else {
            match git.is_ancestor(&intent.new_commit, &status.current_head) {
                Ok(true) => "published_unrecorded",
                Ok(false) => "main_diverged",
                Err(_) => "git_unobserved",
            }
        };
        status.recovery = PromotionRecoverySummary {
            required: true,
            kind,
            operator_action: "recover-promotion",
            automatic: false,
        };
    }
    if let Some(recorded) = &mut status.latest_recorded {
        recorded.on_main = git
            .is_ancestor(&recorded.promoted_commit, &status.current_head)
            .ok();
    }
}

fn trust_error(error: anyhow::Error) -> RpcError {
    if let Some(kind) = error.downcast_ref::<TrustTransitionError>() {
        match kind {
            TrustTransitionError::ObservationNotFound => {
                RpcError::new(-32004, "retained Oracle observation not found")
            }
            TrustTransitionError::ContextMismatch => RpcError::new(
                -32002,
                "Oracle observation does not match the live Claim context",
            ),
            TrustTransitionError::UnsupportedOutcome => RpcError::new(
                -32006,
                "Oracle outcome does not authorize a trust transition",
            ),
            TrustTransitionError::SketchEvidenceMismatch => RpcError::new(
                -32002,
                "formal sketch evidence does not match the live Claim context",
            ),
            TrustTransitionError::OracleEvidenceMismatch => RpcError::new(
                -32002,
                "Oracle-backed evidence does not match the live Claim context",
            ),
        }
    } else {
        internal(error)
    }
}

fn state_error(error: anyhow::Error) -> RpcError {
    if error.downcast_ref::<IdempotencyConflict>().is_some() {
        RpcError::new(-32005, "ID already identifies different content")
    } else if let Some(kind) = error.downcast_ref::<WorkerAttemptError>() {
        match kind {
            WorkerAttemptError::NotFound => RpcError::new(-32004, "worker attempt not found"),
            WorkerAttemptError::Forbidden => {
                RpcError::new(-32003, "worker attempt belongs to another principal")
            }
            WorkerAttemptError::Terminal => RpcError::new(-32007, "worker attempt is not running"),
        }
    } else {
        internal(error)
    }
}
fn lock_managed_git(repo: &str) -> Result<ManagedGit, RpcError> {
    ManagedGit::try_lock_existing(Path::new(repo)).map_err(|error| {
        if error.to_string().contains("busy") {
            RpcError::new(-32003, "managed Git coordination is busy")
        } else {
            RpcError::new(-32006, "trusted managed Git configuration is unavailable")
        }
    })
}

fn confirm_rfc_git(git: &ManagedGit, review: &str, module: &str) -> Result<()> {
    ensure!(git.main_head()? == review, "reviewed Git head changed");
    ensure!(
        git.contains_tracked(review, module)?,
        "InfraRFC target module is not tracked at the reviewed commit"
    );
    Ok(())
}
fn response<T: serde::Serialize>(v: T) -> std::result::Result<Value, RpcError> {
    serde_json::to_value(v).map_err(|e| internal(e.into()))
}

struct Project {
    root: PathBuf,
    store: Mutex<StateStore>,
    wiki: Mutex<Wiki>,
    retained_dir: PathBuf,
    probes: Probes,
    t3_discovery: Option<Arc<Mutex<DiscoveryState>>>,
    t2_audit: Option<Arc<Mutex<AuditState>>>,
    native_rfc: Option<crate::native_rfc::Config>,
}

impl Project {
    fn store(&self) -> Result<std::sync::MutexGuard<'_, StateStore>> {
        self.store
            .lock()
            .map_err(|_| anyhow::anyhow!("state lock poisoned"))
    }
    fn wiki(&self) -> Result<std::sync::MutexGuard<'_, Wiki>> {
        self.wiki
            .lock()
            .map_err(|_| anyhow::anyhow!("wiki lock poisoned"))
    }

    fn wiki_read<T>(
        &self,
        permit_managed: bool,
        read: impl Fn(&Wiki, Option<WikiManagedGeneration<'_>>) -> Result<T>,
    ) -> Result<(T, bool)> {
        let installed = if permit_managed {
            self.wiki()?.managed_generation()?
        } else {
            None
        };
        if let Some((head, event_seq)) = installed {
            let stamp = self.store()?.managed_wiki_stamp()?;
            if let Some((repo, seq)) = stamp
                && seq == event_seq
                && managed_wiki::still_at(&repo, &head)
            {
                let generation = WikiManagedGeneration {
                    head: &head,
                    event_seq,
                };
                let result = {
                    let wiki = self.wiki()?;
                    read(&wiki, Some(generation))?
                };
                // Do not hold either DB mutex while the local Git observation runs.
                // No response with derived data leaves the service if either authority drifted.
                if managed_wiki::still_at(&repo, &head)
                    && self.store()?.managed_wiki_stamp()?.as_ref() == Some(&(repo, event_seq))
                    && self.wiki()?.managed_generation()?.as_ref() == Some(&(head, event_seq))
                {
                    return Ok((result, true));
                }
            }
        }
        let wiki = self.wiki()?;
        Ok((read(&wiki, None)?, false))
    }

    fn append_route_lessons<'a>(
        &self,
        page: &mut Value,
        queries: impl IntoIterator<Item = &'a str>,
        limit: usize,
    ) -> std::result::Result<(), RpcError> {
        let limit = limit.min(ROUTE_LESSON_MAX_ROWS);
        let mut rows = Vec::new();
        let mut truncated = false;
        {
            let store = self.store().map_err(internal)?;
            for query in queries {
                let lesson_page = store.pivot_lessons(query, limit).map_err(internal)?;
                truncated |= lesson_page.truncated;
                rows.extend(lesson_page.items);
            }
        }
        wiki_add_route_lessons(page, rows, limit, truncated)
    }

    fn managed_repo_name(&self) -> Result<String, RpcError> {
        self.store()
            .map_err(internal)?
            .managed_repo()
            .map_err(internal)?
            .ok_or_else(|| RpcError::new(-32006, "managed repository is not bound"))
    }

    fn module_engine(&self, module: &str) -> Result<&'static str> {
        let configured = fs::read_to_string(self.root.join(".runtime/primary_engine"))?;
        let engine = configured
            .trim()
            .parse::<Engine>()
            .context("invalid project primary engine")?;
        validate_source_path(engine, module).map_err(|e| anyhow::anyhow!(e.detail))?;
        Ok(engine.as_str())
    }

    fn status(&self) -> Result<Value> {
        let managed_repo = self.store()?.managed_repo()?;
        let mut data = serde_json::to_value(self.store()?.live_telemetry()?)?;
        data["managed_repo_bound"] = json!(managed_repo.is_some());
        data["managed_repo"] = json!(managed_repo);
        data["memory"] = memory_sample().unwrap_or(Value::Null);
        // These counters cover the daemon broker, not independent CLI processes.
        data["broker"] = serde_json::to_value(self.probes.load()?)?;
        data["workers"] = Value::Null;
        if let Some(discovery) = &self.t3_discovery {
            data["t3_discovery"] = match discovery.lock() {
                Ok(observation) => serde_json::to_value(&*observation)?,
                Err(_) => serde_json::to_value(DiscoveryState::Unavailable)?,
            };
        }
        if let Some(audit) = &self.t2_audit {
            data["t2_audit"] = match audit.lock() {
                Ok(summary) => serde_json::to_value(&*summary)?,
                Err(_) => serde_json::to_value(AuditState::unavailable())?,
            };
        }
        Ok(data)
    }
    fn method(
        &self,
        connection: &Connection,
        method: &str,
        params: Map<String, Value>,
    ) -> std::result::Result<Value, RpcError> {
        match method {
            "session.bind" => {
                let args: SessionBind = parse(params)?;
                if !WRITE_ROLES.contains(&args.role.as_str()) {
                    return Err(RpcError::new(-32602, "invalid principal role"));
                }
                connection.bind_principal(&args.role, args.binding_id.as_deref())?;
                Ok(json!({"principal":args.role}))
            }
            "subscribe" | "unsubscribe" => {
                let args: Topics = parse(params)?;
                if args.topics.is_empty()
                    || args.topics.len() > TOPICS.len()
                    || args.topics.iter().any(|s| !TOPICS.contains(&s.as_str()))
                    || args
                        .topics
                        .iter()
                        .collect::<std::collections::BTreeSet<_>>()
                        .len()
                        != args.topics.len()
                {
                    return Err(RpcError::new(-32602, "request distinct supported topics"));
                }
                for topic in &args.topics {
                    if method == "subscribe" {
                        connection.subscribe(topic)?;
                    } else {
                        connection.unsubscribe(topic);
                    }
                }
                Ok(json!({"topics":args.topics}))
            }
            "status" => {
                let _: Empty = parse(params)?;
                self.status().map_err(internal)
            }
            "briefing.peek" | "briefing.pull" => {
                let _: Empty = parse(params)?;
                let facts = self
                    .store()
                    .map_err(internal)?
                    .briefing_facts()
                    .map_err(internal)?;
                let pack = FactPack::compile(facts).map_err(internal)?;
                let retained_id = if method == "briefing.pull" {
                    pack.retain(&self.retained_dir).map_err(internal)?;
                    Some(&pack.fact_sha256)
                } else {
                    None
                };
                Ok(json!({"fact_pack":pack,"retained_id":retained_id}))
            }
            "briefing.get" => {
                let args: Retained = parse(params)?;
                if !facts::valid_id(&args.id) {
                    return Err(RpcError::new(-32602, "expected a lowercase SHA-256 ID"));
                }
                let pack = facts::read_retained(&self.retained_dir, &args.id)
                    .map_err(internal)?
                    .ok_or_else(|| RpcError::new(-32004, "retained briefing not found"))?;
                Ok(json!({"retained_id":args.id,"fact_pack":pack}))
            }

            "claim.create" => {
                let args: ClaimCreate = parse(params)?;
                let role = connection
                    .principal()
                    .ok_or_else(|| RpcError::new(-32003, "connection principal is not bound"))?;
                require_write_role(&role, &["chief"], "claim.create")?;
                rpc_id("claim_id", &args.claim_id, 128)?;
                rpc_id("target_decl", &args.target_decl, 256)?;
                rpc_text("title", &args.title, 512)?;
                rpc_text("informal_statement", &args.informal_statement, 16 * 1024)?;
                let engine = self
                    .module_engine(&args.module_path)
                    .map_err(|e| RpcError::new(-32602, e.to_string()))?;
                let formal_target = format!("{}#{}", args.module_path, args.target_decl);
                let actor = actor(&role);
                let claim = {
                    let mut store = self.store().map_err(internal)?;
                    store
                        .create_claim(
                            NewClaim {
                                id: &args.claim_id,
                                title: &args.title,
                                informal_statement: &args.informal_statement,
                                engine,
                                formal_target: Some(&formal_target),
                                module_path: Some(&args.module_path),
                                critical: args.critical,
                            },
                            Change {
                                actor: &actor,
                                git_commit: None,
                            },
                        )
                        .map_err(state_error)?;
                    store
                        .claim(&args.claim_id)
                        .map_err(internal)?
                        .ok_or_else(|| RpcError::new(-32603, "created claim disappeared"))?
                };
                Ok(json!({"claim":claim}))
            }

            "claim.apply_oracle_backed" => {
                let args: FormalSketchArgs = parse(params)?;
                let role = connection
                    .principal()
                    .ok_or_else(|| RpcError::new(-32003, "connection principal is not bound"))?;
                require_write_role(&role, &["builder"], "claim.apply_oracle_backed")?;
                rpc_id("task_id", &args.task_id, 100)?;
                rpc_sha256(&args.observation_id)?;
                let config = self.native_rfc.as_ref().ok_or_else(|| {
                    RpcError::new(-32006, "trusted Oracle-backed tooling is not configured")
                })?;
                let repo = self.managed_repo_name()?;
                let git = lock_managed_git(&repo)?;
                let head = git.main_head().map_err(native_rfc_error)?;
                let candidate = self
                    .store()
                    .map_err(internal)?
                    .task_candidate(&args.task_id)
                    .map_err(internal)?
                    .ok_or_else(|| RpcError::new(-32004, "retained task candidate not found"))?;
                if candidate.role != "prover_worker" || candidate.base_commit != head {
                    return Err(RpcError::new(
                        -32002,
                        "Oracle-backed candidate is not current prover evidence",
                    ));
                }
                drop(git);
                let record = config
                    .oracle_backed_task(
                        &self.root,
                        Path::new(&repo),
                        &args.task_id,
                        &args.observation_id,
                    )
                    .map_err(native_rfc_error)?;
                Ok(record)
            }
            "claim.apply_formal_sketch" => {
                let args: FormalSketchArgs = parse(params)?;
                let role = connection
                    .principal()
                    .ok_or_else(|| RpcError::new(-32003, "connection principal is not bound"))?;
                require_write_role(&role, &["builder"], "claim.apply_formal_sketch")?;
                rpc_id("task_id", &args.task_id, 100)?;
                rpc_sha256(&args.observation_id)?;
                let config = self.native_rfc.as_ref().ok_or_else(|| {
                    RpcError::new(-32006, "trusted formal sketch tooling is not configured")
                })?;
                let repo = self.managed_repo_name()?;
                let git = lock_managed_git(&repo)?;
                let head = git.main_head().map_err(native_rfc_error)?;
                let candidate = self
                    .store()
                    .map_err(internal)?
                    .task_candidate(&args.task_id)
                    .map_err(internal)?
                    .ok_or_else(|| RpcError::new(-32004, "retained task candidate not found"))?;
                if candidate.role != "prover_worker" || candidate.base_commit != head {
                    return Err(RpcError::new(
                        -32002,
                        "formal sketch candidate is not current prover evidence",
                    ));
                }
                drop(git);
                let record = config
                    .formal_sketch_task(
                        &self.root,
                        Path::new(&repo),
                        &args.task_id,
                        &args.observation_id,
                    )
                    .map_err(native_rfc_error)?;
                Ok(record)
            }
            "claim.apply_oracle" => {
                let args: ClaimOracleTransition = parse(params)?;
                let role = connection
                    .principal()
                    .ok_or_else(|| RpcError::new(-32003, "connection principal is not bound"))?;
                require_write_role(&role, &["chief"], "claim.apply_oracle")?;
                rpc_id("claim_id", &args.claim_id, 128)?;
                rpc_sha256(&args.observation_id)?;
                let repo = self.managed_repo_name()?;
                let git = lock_managed_git(&repo)?;
                let head = git
                    .main_head()
                    .map_err(|e| RpcError::new(-32006, e.to_string()))?;
                let actor = actor(&role);
                let claim = self
                    .store()
                    .map_err(internal)?
                    .apply_oracle_trust_transition(
                        &args.claim_id,
                        &args.observation_id,
                        &head,
                        Change {
                            actor: &actor,
                            git_commit: Some(&head),
                        },
                    )
                    .map_err(trust_error)?;
                Ok(json!({"claim":claim,"proof_verified":false}))
            }
            "claim.read" => {
                let args: ClaimRead = parse(params)?;
                rpc_id("claim_id", &args.claim_id, 128)?;
                let claim = self
                    .store()
                    .map_err(internal)?
                    .claim(&args.claim_id)
                    .map_err(internal)?
                    .ok_or_else(|| RpcError::new(-32004, "claim not found"))?;
                Ok(json!({"claim":claim}))
            }

            "plan.create" => {
                let args: PlanCreate = parse(params)?;
                let role = connection
                    .principal()
                    .ok_or_else(|| RpcError::new(-32003, "connection principal is not bound"))?;
                require_write_role(&role, &["chief"], "plan.create")?;
                rpc_id("plan_id", &args.plan_id, 128)?;
                rpc_id("target_claim_id", &args.target_claim_id, 128)?;
                rpc_text("name", &args.name, 512)?;
                if !matches!(args.track.as_str(), "exploration" | "infrastructure") {
                    return Err(RpcError::new(-32602, "invalid plan track"));
                }
                if args.dependencies.len() > 64 {
                    return Err(RpcError::new(-32602, "at most 64 plan dependencies"));
                }
                let mut seen = std::collections::BTreeSet::new();
                for dep in &args.dependencies {
                    rpc_id("dependency claim_id", &dep.claim_id, 128)?;
                    if !matches!(
                        dep.kind.as_str(),
                        "lemma" | "definition" | "oracle_placeholder"
                    ) {
                        return Err(RpcError::new(-32602, "invalid dependency kind"));
                    }
                    if !seen.insert(dep.claim_id.as_str()) {
                        return Err(RpcError::new(-32602, "duplicate plan dependency claim"));
                    }
                }
                let actor = actor(&role);
                let plan = {
                    let mut store = self.store().map_err(internal)?;
                    if store
                        .claim(&args.target_claim_id)
                        .map_err(internal)?
                        .is_none()
                    {
                        return Err(RpcError::new(-32004, "target claim not found"));
                    }
                    for dep in &args.dependencies {
                        if store.claim(&dep.claim_id).map_err(internal)?.is_none() {
                            return Err(RpcError::new(-32004, "dependency claim not found"));
                        }
                    }
                    let dependencies: Vec<(&str, &str)> = args
                        .dependencies
                        .iter()
                        .map(|dep| (dep.claim_id.as_str(), dep.kind.as_str()))
                        .collect();
                    store
                        .create_plan(
                            NewPlan {
                                id: &args.plan_id,
                                target: &args.target_claim_id,
                                name: &args.name,
                                track: &args.track,
                                status: PlanStatus::Active,
                                dependencies: &dependencies,
                            },
                            Change {
                                actor: &actor,
                                git_commit: None,
                            },
                        )
                        .map_err(state_error)?;
                    store
                        .plan_state(&args.plan_id)
                        .map_err(internal)?
                        .ok_or_else(|| RpcError::new(-32603, "created plan disappeared"))?
                };
                Ok(json!({
                    "plan_id":args.plan_id,
                    "target_claim_id":args.target_claim_id,
                    "status":plan.0,
                    "effective_trust_level":plan.1,
                }))
            }
            "plan.read" => {
                let args: PlanRead = parse(params)?;
                rpc_id("plan_id", &args.plan_id, 128)?;
                let plan = self
                    .store()
                    .map_err(internal)?
                    .plan_state(&args.plan_id)
                    .map_err(internal)?
                    .ok_or_else(|| RpcError::new(-32004, "plan not found"))?;
                Ok(json!({"plan_id":args.plan_id,"status":plan.0,"effective_trust_level":plan.1}))
            }

            "inbox.push" => {
                let args: InboxPush = parse(params)?;
                let role = connection
                    .principal()
                    .ok_or_else(|| RpcError::new(-32003, "connection principal is not bound"))?;
                require_write_role(&role, &["chief"], "inbox.push")?;
                rpc_id("inbox_id", &args.inbox_id, 128)?;
                rpc_text("summary", &args.summary, 4096)?;
                let priority = args
                    .priority
                    .parse::<DecisionPriority>()
                    .map_err(|_| RpcError::new(-32602, "invalid decision priority"))?;
                let category = args
                    .category
                    .parse::<DecisionCategory>()
                    .map_err(|_| RpcError::new(-32602, "invalid decision category"))?;
                let related: Vec<&str> = args.related_claims.iter().map(String::as_str).collect();
                let evidence: Vec<&str> = args.evidence_refs.iter().map(String::as_str).collect();
                let actor = actor(&role);
                let decision = self
                    .store()
                    .map_err(internal)?
                    .push_decision(
                        NewDecision {
                            id: &args.inbox_id,
                            priority,
                            category,
                            summary: &args.summary,
                            leverage_score: args.leverage_score,
                            related_claims: &related,
                            evidence_refs: &evidence,
                            options: &args.options,
                            details: Value::Object(args.details),
                        },
                        Change {
                            actor: &actor,
                            git_commit: None,
                        },
                    )
                    .map_err(state_error)?;
                Ok(json!({"decision":decision}))
            }

            "promotion.preview" => {
                let args: PromotionPreviewArgs = parse(params)?;
                let role = connection
                    .principal()
                    .ok_or_else(|| RpcError::new(-32003, "connection principal is not bound"))?;
                require_write_role(&role, &["chief"], "promotion.preview")?;
                rpc_id("task_id", &args.task_id, 100)?;
                let repo = self.managed_repo_name()?;
                let git = lock_managed_git(&repo)?;
                let head = git
                    .main_head()
                    .map_err(|e| RpcError::new(-32006, e.to_string()))?;
                let preview = self
                    .store()
                    .map_err(internal)?
                    .promotion_preview(&args.task_id, &head)
                    .map_err(internal)?;
                let head_after = git
                    .main_head()
                    .map_err(|e| RpcError::new(-32006, e.to_string()))?;
                if head_after != head {
                    return Err(RpcError::new(
                        -32002,
                        "managed Git HEAD changed during preview",
                    ));
                }
                Ok(json!({"preview":preview}))
            }

            "promotion.approval" => {
                let args: PromotionApprovalArgs = parse(params)?;
                let role = connection
                    .principal()
                    .ok_or_else(|| RpcError::new(-32003, "connection principal is not bound"))?;
                require_write_role(&role, &["owner"], "promotion.approval")?;
                rpc_id("task_id", &args.task_id, 100)?;
                rpc_id("request_id", &args.request_id, 128)?;
                if args.expires_unix == 0 {
                    return Err(RpcError::new(
                        -32602,
                        "expiry must be positive Unix seconds",
                    ));
                }
                let config = self.native_rfc.as_ref().ok_or_else(|| {
                    RpcError::new(-32006, "trusted promotion tooling is not configured")
                })?;
                let (cli_sha256, lean_sha256) = config.fingerprints().map_err(native_rfc_error)?;
                let repo = self.managed_repo_name()?;
                let git = lock_managed_git(&repo)?;
                let head = git.main_head().map_err(native_rfc_error)?;
                let preview = self
                    .store()
                    .map_err(internal)?
                    .promotion_preview(&args.task_id, &head)
                    .map_err(internal)?;
                let approval = crate::owner_promotion::PromotionApproval::new(
                    &self.root,
                    Path::new(&repo),
                    &preview,
                    &cli_sha256,
                    &lean_sha256,
                    &args.request_id,
                    args.expires_unix,
                )
                .map_err(owner_promotion_error)?;
                Ok(json!({
                    "message_hex":crate::owner_promotion::hex_encode(approval.signing_bytes()),
                    "message_sha256":approval.message_sha256(),
                    "preview":preview,
                }))
            }
            "promotion.execute" => {
                let args: PromotionExecuteArgs = parse(params)?;
                let role = connection
                    .principal()
                    .ok_or_else(|| RpcError::new(-32003, "connection principal is not bound"))?;
                require_write_role(&role, &["owner"], "promotion.execute")?;
                rpc_id("task_id", &args.task_id, 100)?;
                rpc_id("request_id", &args.request_id, 128)?;
                if args.expires_unix == 0 {
                    return Err(RpcError::new(
                        -32602,
                        "expiry must be positive Unix seconds",
                    ));
                }
                if args.signature_hex.is_empty() || args.signature_hex.len() > 8192 {
                    return Err(RpcError::new(-32602, "invalid signature size"));
                }
                let signature = crate::owner_promotion::hex_decode(&args.signature_hex)
                    .map_err(owner_promotion_error)?;
                let config = self.native_rfc.as_ref().ok_or_else(|| {
                    RpcError::new(-32006, "trusted promotion tooling is not configured")
                })?;
                let (cli_sha256, lean_sha256) = config.fingerprints().map_err(native_rfc_error)?;
                let repo = self.managed_repo_name()?;
                let git = lock_managed_git(&repo)?;
                let head = git.main_head().map_err(native_rfc_error)?;
                let preview = self
                    .store()
                    .map_err(internal)?
                    .promotion_preview(&args.task_id, &head)
                    .map_err(internal)?;
                let approval = crate::owner_promotion::PromotionApproval::new(
                    &self.root,
                    Path::new(&repo),
                    &preview,
                    &cli_sha256,
                    &lean_sha256,
                    &args.request_id,
                    args.expires_unix,
                )
                .map_err(owner_promotion_error)?;
                approval
                    .verify_fresh(&self.root, &signature)
                    .map_err(owner_promotion_error)?;
                if self
                    .store()
                    .map_err(internal)?
                    .promotion_intent()
                    .map_err(internal)?
                    .is_some()
                {
                    return Err(RpcError::new(
                        -32002,
                        "a promotion intent is already pending",
                    ));
                }
                let consumed_unix = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_err(|error| internal(error.into()))?
                    .as_secs();
                let actor = actor(&role);
                let consumed = self
                    .store()
                    .map_err(internal)?
                    .consume_promotion_approval(
                        &args.request_id,
                        &approval.message_sha256(),
                        &preview.candidate_sha256,
                        &args.task_id,
                        consumed_unix,
                        Change {
                            actor: &actor,
                            git_commit: Some(&head),
                        },
                    )
                    .map_err(internal)?;
                if !consumed {
                    return Ok(
                        json!({"accepted":false,"replayed":true,"message":"approval already consumed; inspect promotion status"}),
                    );
                }
                approval.check_fresh().map_err(owner_promotion_error)?;
                drop(git);
                let record = config
                    .promote_task_result(&self.root, Path::new(&repo), &args.task_id)
                    .map_err(native_rfc_error)?;
                Ok(json!({"accepted":true,"replayed":false,"promotion":record}))
            }
            "promotion.recover" => {
                let _: Empty = parse(params)?;
                let role = connection
                    .principal()
                    .ok_or_else(|| RpcError::new(-32003, "connection principal is not bound"))?;
                require_write_role(&role, &["owner"], "promotion.recover")?;
                if self
                    .store()
                    .map_err(internal)?
                    .promotion_intent()
                    .map_err(internal)?
                    .is_none()
                {
                    return Err(RpcError::new(
                        -32002,
                        "no pending promotion intent to recover",
                    ));
                }
                let config = self.native_rfc.as_ref().ok_or_else(|| {
                    RpcError::new(-32006, "trusted promotion tooling is not configured")
                })?;
                let repo = self.managed_repo_name()?;
                let record = config
                    .recover_promotion(&self.root, Path::new(&repo))
                    .map_err(native_rfc_error)?;
                Ok(json!({"recovery":record}))
            }
            "promotion.status" => {
                let _: Empty = parse(params)?;
                let role = connection
                    .principal()
                    .ok_or_else(|| RpcError::new(-32003, "connection principal is not bound"))?;
                require_write_role(&role, &["chief"], "promotion.status")?;
                let repo = self.managed_repo_name()?;
                let git = lock_managed_git(&repo)?;
                let head = git
                    .main_head()
                    .map_err(|e| RpcError::new(-32006, e.to_string()))?;
                let mut status = self
                    .store()
                    .map_err(internal)?
                    .promotion_status(&head)
                    .map_err(internal)?;
                classify_promotion_status(&git, &mut status);
                let head_after = git
                    .main_head()
                    .map_err(|e| RpcError::new(-32006, e.to_string()))?;
                if head_after != head {
                    return Err(RpcError::new(
                        -32002,
                        "managed Git HEAD changed during status read",
                    ));
                }
                Ok(json!({"status":status}))
            }
            "worker.acquire" => {
                let args: WorkerAcquire = parse(params)?;
                let role = connection
                    .principal()
                    .ok_or_else(|| RpcError::new(-32003, "connection principal is not bound"))?;
                require_write_role(&role, &["prover", "reviewer"], "worker.acquire")?;
                let principal = connection
                    .principal_key()
                    .ok_or_else(|| RpcError::new(-32003, "connection principal is not bound"))?;
                rpc_id("request_id", &args.request_id, 128)?;
                if !(10..=600).contains(&args.wall_seconds) {
                    return Err(RpcError::new(-32602, "wall_seconds must be in 10..=600"));
                }
                let repo = self.managed_repo_name()?;
                let git = lock_managed_git(&repo)?;
                let base = git
                    .main_head()
                    .map_err(|e| RpcError::new(-32006, e.to_string()))?;
                let actor = actor(&role);
                let attempt = self
                    .store()
                    .map_err(internal)?
                    .acquire_worker_attempt(
                        &args.request_id,
                        &principal,
                        &base,
                        args.wall_seconds,
                        Change {
                            actor: &actor,
                            git_commit: Some(&base),
                        },
                    )
                    .map_err(state_error)?;
                Ok(json!({"attempt":attempt}))
            }
            "worker.submit" => {
                let args: WorkerSubmit = parse(params)?;
                let role = connection
                    .principal()
                    .ok_or_else(|| RpcError::new(-32003, "connection principal is not bound"))?;
                require_write_role(&role, &["prover", "reviewer"], "worker.submit")?;
                let principal = connection
                    .principal_key()
                    .ok_or_else(|| RpcError::new(-32003, "connection principal is not bound"))?;
                rpc_id("attempt_id", &args.attempt_id, 128)?;
                let suggestion = match &args.suggestion {
                    WorkerSuggestionInput::ProofBody { proof_body } => {
                        WorkerSuggestion::ProofBody(proof_body)
                    }
                    WorkerSuggestionInput::Review { verdict, rationale } => {
                        WorkerSuggestion::Review { verdict, rationale }
                    }
                };
                let actor = actor(&role);
                let attempt = self
                    .store()
                    .map_err(internal)?
                    .submit_worker_attempt(
                        &args.attempt_id,
                        &principal,
                        suggestion,
                        Change {
                            actor: &actor,
                            git_commit: None,
                        },
                    )
                    .map_err(state_error)?;
                Ok(json!({"attempt":attempt}))
            }
            "worker.cancel" => {
                let args: WorkerAttemptRef = parse(params)?;
                let role = connection
                    .principal()
                    .ok_or_else(|| RpcError::new(-32003, "connection principal is not bound"))?;
                require_write_role(&role, &["prover", "reviewer"], "worker.cancel")?;
                let principal = connection
                    .principal_key()
                    .ok_or_else(|| RpcError::new(-32003, "connection principal is not bound"))?;
                rpc_id("attempt_id", &args.attempt_id, 128)?;
                let actor = actor(&role);
                let attempt = self
                    .store()
                    .map_err(internal)?
                    .cancel_worker_attempt(
                        &args.attempt_id,
                        &principal,
                        Change {
                            actor: &actor,
                            git_commit: None,
                        },
                    )
                    .map_err(state_error)?;
                Ok(json!({"attempt":attempt}))
            }
            "worker.status" => {
                let args: WorkerAttemptRef = parse(params)?;
                let role = connection
                    .principal()
                    .ok_or_else(|| RpcError::new(-32003, "connection principal is not bound"))?;
                require_write_role(&role, &["prover", "reviewer"], "worker.status")?;
                let principal = connection
                    .principal_key()
                    .ok_or_else(|| RpcError::new(-32003, "connection principal is not bound"))?;
                rpc_id("attempt_id", &args.attempt_id, 128)?;
                let attempt = self
                    .store()
                    .map_err(internal)?
                    .worker_attempt(&args.attempt_id, &principal)
                    .map_err(internal)?
                    .ok_or_else(|| RpcError::new(-32004, "worker attempt not found"))?;
                Ok(json!({"attempt":attempt}))
            }
            "task.register" => {
                let args: TaskRegister = parse(params)?;
                let role = connection
                    .principal()
                    .ok_or_else(|| RpcError::new(-32003, "connection principal is not bound"))?;
                require_write_role(&role, &["chief"], "task.register")?;
                rpc_id("task_id", &args.task_id, 100)?;
                rpc_id("target_decl", &args.target_decl, 256)?;
                let track = args
                    .track
                    .parse::<TaskTrack>()
                    .map_err(|_| RpcError::new(-32602, "invalid task track"))?;
                if args.dependencies.len() > 32 {
                    return Err(RpcError::new(-32602, "at most 32 task dependencies"));
                }
                let mut seen = std::collections::BTreeSet::new();
                for dependency in &args.dependencies {
                    rpc_id("task dependency", dependency, 100)?;
                    if dependency == &args.task_id || !seen.insert(dependency.as_str()) {
                        return Err(RpcError::new(
                            -32602,
                            "invalid or duplicate task dependency",
                        ));
                    }
                }
                let engine = self
                    .module_engine(&args.module_path)
                    .map_err(|e| RpcError::new(-32602, e.to_string()))?;
                let repo = self.managed_repo_name()?;
                let git = lock_managed_git(&repo)?;
                let review = git
                    .main_head()
                    .map_err(|e| RpcError::new(-32006, e.to_string()))?;
                confirm_rfc_git(&git, &review, &args.module_path)
                    .map_err(|e| RpcError::new(-32002, e.to_string()))?;
                let actor = actor(&role);
                let dependencies: Vec<&str> =
                    args.dependencies.iter().map(String::as_str).collect();
                self.store()
                    .map_err(internal)?
                    .register_task(
                        NewTask {
                            id: &args.task_id,
                            track,
                            engine,
                            module_path: &args.module_path,
                            target: &args.target_decl,
                            dependencies: &dependencies,
                        },
                        Change {
                            actor: &actor,
                            git_commit: Some(&review),
                        },
                    )
                    .map_err(state_error)?;
                Ok(json!({"task_id":args.task_id,"track":track,"dispatched":false}))
            }
            "task.status" => {
                let _: Empty = parse(params)?;
                response(
                    self.store()
                        .map_err(internal)?
                        .task_scheduler_status()
                        .map_err(internal)?,
                )
            }
            "rfc.submit" => {
                let args: RfcSubmit = parse(params)?;
                let role = connection
                    .principal()
                    .ok_or_else(|| RpcError::new(-32003, "connection principal is not bound"))?;
                require_write_role(&role, &["explorer"], "rfc.submit")?;
                rpc_id("rfc_id", &args.draft.rfc_id, 100)?;
                rpc_id("requested_by_claim", &args.draft.requested_by_claim, 256)?;
                rpc_commit(&args.reviewed_commit)?;
                if args.draft.status != InfraStatus::Draft {
                    return Err(RpcError::new(-32602, "InfraRFC submission must be DRAFT"));
                }
                let engine = self
                    .module_engine(&args.module_path)
                    .map_err(|e| RpcError::new(-32602, e.to_string()))?;
                if args.draft.engine.as_str() != engine {
                    return Err(RpcError::new(
                        -32602,
                        "InfraRFC engine does not match the project",
                    ));
                }
                let repo = self.managed_repo_name()?;
                let git = lock_managed_git(&repo)?;
                confirm_rfc_git(&git, &args.reviewed_commit, &args.module_path)
                    .map_err(|e| RpcError::new(-32002, e.to_string()))?;
                let actor = actor(&role);
                let record = self
                    .store()
                    .map_err(internal)?
                    .submit_infra_rfc_actor(
                        &actor,
                        &repo,
                        &args.reviewed_commit,
                        &args.module_path,
                        &args.draft,
                        || confirm_rfc_git(&git, &args.reviewed_commit, &args.module_path),
                    )
                    .map_err(state_error)?;
                Ok(json!({"rfc":record}))
            }
            "rfc.approve" => {
                let args: RfcApprove = parse(params)?;
                let role = connection
                    .principal()
                    .ok_or_else(|| RpcError::new(-32003, "connection principal is not bound"))?;
                require_write_role(&role, &["chief"], "rfc.approve")?;
                rpc_id("rfc_id", &args.rfc_id, 100)?;
                rpc_commit(&args.reviewed_commit)?;
                rpc_sha256(&args.draft_sha256)?;
                let repo = self.managed_repo_name()?;
                let stored = self
                    .store()
                    .map_err(internal)?
                    .infra_rfc(&repo, &args.rfc_id)
                    .map_err(internal)?
                    .ok_or_else(|| RpcError::new(-32004, "InfraRFC not found"))?;
                let git = lock_managed_git(&repo)?;
                confirm_rfc_git(&git, &args.reviewed_commit, &stored.module_path)
                    .map_err(|e| RpcError::new(-32002, e.to_string()))?;
                let actor = actor(&role);
                let record = self
                    .store()
                    .map_err(internal)?
                    .approve_infra_rfc_actor(
                        &actor,
                        &repo,
                        &args.reviewed_commit,
                        &args.rfc_id,
                        &args.draft_sha256,
                        || confirm_rfc_git(&git, &args.reviewed_commit, &stored.module_path),
                    )
                    .map_err(state_error)?;
                Ok(json!({"rfc":record}))
            }

            "rfc.smoke_start" => {
                let args: RfcApprove = parse(params)?;
                let role = connection
                    .principal()
                    .ok_or_else(|| RpcError::new(-32003, "connection principal is not bound"))?;
                require_write_role(&role, &["builder"], "rfc.smoke_start")?;
                rpc_id("rfc_id", &args.rfc_id, 100)?;
                rpc_commit(&args.reviewed_commit)?;
                rpc_sha256(&args.draft_sha256)?;
                let repo = self.managed_repo_name()?;
                let stored = self
                    .store()
                    .map_err(internal)?
                    .infra_rfc(&repo, &args.rfc_id)
                    .map_err(internal)?
                    .ok_or_else(|| RpcError::new(-32004, "InfraRFC not found"))?;
                let git = lock_managed_git(&repo)?;
                confirm_rfc_git(&git, &args.reviewed_commit, &stored.module_path)
                    .map_err(|e| RpcError::new(-32002, e.to_string()))?;
                let actor = actor(&role);
                let record = self
                    .store()
                    .map_err(internal)?
                    .start_infra_rfc_smoke_actor(
                        &actor,
                        &repo,
                        &args.reviewed_commit,
                        &args.rfc_id,
                        &args.draft_sha256,
                        || confirm_rfc_git(&git, &args.reviewed_commit, &stored.module_path),
                    )
                    .map_err(state_error)?;
                Ok(json!({"rfc":record}))
            }

            "rfc.freeze" => {
                let args: RfcFreeze = parse(params)?;
                let role = connection
                    .principal()
                    .ok_or_else(|| RpcError::new(-32003, "connection principal is not bound"))?;
                require_write_role(&role, &["builder"], "rfc.freeze")?;
                rpc_id("rfc_id", &args.rfc_id, 100)?;
                rpc_commit(&args.reviewed_commit)?;
                rpc_sha256(&args.draft_sha256)?;
                if args.smoke_lemmas.is_empty() || args.smoke_lemmas.len() > 16 {
                    return Err(RpcError::new(
                        -32602,
                        "smoke_lemmas must contain 1..=16 names",
                    ));
                }
                for lemma in &args.smoke_lemmas {
                    rpc_text("smoke lemma", lemma, 512)?;
                }
                let config = self
                    .native_rfc
                    .as_ref()
                    .ok_or_else(|| RpcError::new(-32006, "native RFC tooling is not configured"))?;
                let repo = self.managed_repo_name()?;
                let stored = self
                    .store()
                    .map_err(internal)?
                    .infra_rfc(&repo, &args.rfc_id)
                    .map_err(internal)?
                    .ok_or_else(|| RpcError::new(-32004, "InfraRFC not found"))?;
                if stored.draft_sha256 != args.draft_sha256
                    || stored.draft.engine != Engine::Lean4
                    || !matches!(
                        stored.status,
                        ai4math_store::InfraRfcStatus::BuildingSmoke
                            | ai4math_store::InfraRfcStatus::FrozenProving
                    )
                {
                    return Err(RpcError::new(
                        -32002,
                        "InfraRFC is not ready for native freeze",
                    ));
                }
                let git = lock_managed_git(&repo)?;
                if git.main_head().map_err(native_rfc_error)? != args.reviewed_commit {
                    return Err(RpcError::new(-32002, "reviewed Git head changed"));
                }
                drop(git);
                let record = config
                    .freeze(
                        &self.root,
                        Path::new(&repo),
                        &args.rfc_id,
                        &args.reviewed_commit,
                        &args.draft_sha256,
                        &args.smoke_lemmas,
                    )
                    .map_err(native_rfc_error)?;
                Ok(json!({"rfc":record}))
            }
            "rfc.complete" => {
                let args: RfcComplete = parse(params)?;
                let role = connection
                    .principal()
                    .ok_or_else(|| RpcError::new(-32003, "connection principal is not bound"))?;
                require_write_role(&role, &["builder"], "rfc.complete")?;
                rpc_id("rfc_id", &args.rfc_id, 100)?;
                rpc_commit(&args.reviewed_commit)?;
                rpc_sha256(&args.draft_sha256)?;
                let config = self
                    .native_rfc
                    .as_ref()
                    .ok_or_else(|| RpcError::new(-32006, "native RFC tooling is not configured"))?;
                let repo = self.managed_repo_name()?;
                let stored = self
                    .store()
                    .map_err(internal)?
                    .infra_rfc(&repo, &args.rfc_id)
                    .map_err(internal)?
                    .ok_or_else(|| RpcError::new(-32004, "InfraRFC not found"))?;
                if stored.draft_sha256 != args.draft_sha256
                    || stored.draft.engine != Engine::Lean4
                    || stored.smoke_commit.as_deref() != Some(args.reviewed_commit.as_str())
                    || !matches!(
                        stored.status,
                        ai4math_store::InfraRfcStatus::FrozenProving
                            | ai4math_store::InfraRfcStatus::Completed
                    )
                {
                    return Err(RpcError::new(
                        -32002,
                        "InfraRFC is not ready for native completion",
                    ));
                }
                let git = lock_managed_git(&repo)?;
                if git.main_head().map_err(native_rfc_error)? != args.reviewed_commit {
                    return Err(RpcError::new(-32002, "reviewed Git head changed"));
                }
                drop(git);
                let record = config
                    .complete(
                        &self.root,
                        Path::new(&repo),
                        &args.rfc_id,
                        &args.reviewed_commit,
                        &args.draft_sha256,
                    )
                    .map_err(native_rfc_error)?;
                Ok(json!({"rfc":record}))
            }
            "rfc.reject" => {
                let args: RfcReject = parse(params)?;
                let role = connection
                    .principal()
                    .ok_or_else(|| RpcError::new(-32003, "connection principal is not bound"))?;
                require_write_role(&role, &["chief"], "rfc.reject")?;
                rpc_id("rfc_id", &args.rfc_id, 100)?;
                rpc_commit(&args.reviewed_commit)?;
                rpc_sha256(&args.draft_sha256)?;
                rpc_reason(&args.reason)?;
                let repo = self.managed_repo_name()?;
                let stored = self
                    .store()
                    .map_err(internal)?
                    .infra_rfc(&repo, &args.rfc_id)
                    .map_err(internal)?
                    .ok_or_else(|| RpcError::new(-32004, "InfraRFC not found"))?;
                let git = lock_managed_git(&repo)?;
                confirm_rfc_git(&git, &args.reviewed_commit, &stored.module_path)
                    .map_err(|e| RpcError::new(-32002, e.to_string()))?;
                let actor = actor(&role);
                let record = self
                    .store()
                    .map_err(internal)?
                    .reject_infra_rfc_actor(
                        &actor,
                        &repo,
                        &args.reviewed_commit,
                        &args.rfc_id,
                        &args.draft_sha256,
                        &args.reason,
                        || confirm_rfc_git(&git, &args.reviewed_commit, &stored.module_path),
                    )
                    .map_err(state_error)?;
                Ok(json!({"rfc":record}))
            }
            "rfc.supersede" => {
                let args: RfcSupersede = parse(params)?;
                let role = connection
                    .principal()
                    .ok_or_else(|| RpcError::new(-32003, "connection principal is not bound"))?;
                require_write_role(&role, &["chief"], "rfc.supersede")?;
                rpc_id("rfc_id", &args.rfc_id, 100)?;
                rpc_id("replacement_rfc_id", &args.replacement_rfc_id, 100)?;
                rpc_commit(&args.reviewed_commit)?;
                rpc_sha256(&args.draft_sha256)?;
                rpc_reason(&args.reason)?;
                let repo = self.managed_repo_name()?;
                let stored = self
                    .store()
                    .map_err(internal)?
                    .infra_rfc(&repo, &args.rfc_id)
                    .map_err(internal)?
                    .ok_or_else(|| RpcError::new(-32004, "InfraRFC not found"))?;
                let git = lock_managed_git(&repo)?;
                confirm_rfc_git(&git, &args.reviewed_commit, &stored.module_path)
                    .map_err(|e| RpcError::new(-32002, e.to_string()))?;
                let actor = actor(&role);
                let record = self
                    .store()
                    .map_err(internal)?
                    .supersede_infra_rfc_actor(
                        &actor,
                        &repo,
                        &args.reviewed_commit,
                        &args.rfc_id,
                        &args.draft_sha256,
                        &args.replacement_rfc_id,
                        &args.reason,
                        || confirm_rfc_git(&git, &args.reviewed_commit, &stored.module_path),
                    )
                    .map_err(state_error)?;
                Ok(json!({"rfc":record}))
            }
            "rfc.inspect" => {
                let args: RfcInspect = parse(params)?;
                rpc_id("rfc_id", &args.rfc_id, 100)?;
                let repo = self.managed_repo_name()?;
                let record = self
                    .store()
                    .map_err(internal)?
                    .infra_rfc(&repo, &args.rfc_id)
                    .map_err(internal)?
                    .ok_or_else(|| RpcError::new(-32004, "InfraRFC not found"))?;
                Ok(json!({"rfc":record}))
            }
            "frontier.list" => {
                let args: List = parse(params)?;
                let limit = cap(args.limit, 24, 64)?;
                response(
                    self.store()
                        .map_err(internal)?
                        .frontier_page(limit)
                        .map_err(internal)?,
                )
            }
            "inbox.list" => {
                let args: List = parse(params)?;
                let limit = cap(args.limit, 24, 64)?;
                response(
                    self.store()
                        .map_err(internal)?
                        .inbox_page(limit)
                        .map_err(internal)?,
                )
            }
            "alerts.list" => {
                let args: AlertList = parse(params)?;
                let limit = cap(args.limit, 16, 64)?;
                if args.after < 0 {
                    return Err(RpcError::new(-32602, "after must be nonnegative"));
                }
                response(
                    self.store()
                        .map_err(internal)?
                        .alert_history(args.after, limit)
                        .map_err(internal)?,
                )
            }
            "proof.submit" => {
                let args: Proof = parse(params)?;
                if !proofs::valid_id(&args.target_id)
                    || args.proof_body.trim().is_empty()
                    || args.proof_body.len() > proofs::PROOF_BODY_MAX
                    || args.proof_body.contains('\0')
                {
                    return Err(RpcError::new(-32602, "invalid target ID or proof body"));
                }
                if !self.probes.has(&args.target_id) {
                    return Err(RpcError::new(-32004, "proof slot unavailable"));
                }
                let checked = self
                    .probes
                    .check(&args.target_id, args.proof_body)
                    .map_err(internal)?;
                Ok(json!({"target_id":args.target_id,"check":checked,"promotion_eligible":false}))
            }
            "wiki.search" => {
                let args: WikiSearchArgs = parse(params)?;
                wiki_text("text", &args.text, 512)?;
                let limit = cap(args.limit, 16, 32)?;
                if let Some(head) = &args.conclusion_head {
                    wiki_text("conclusion_head", head, 128)?;
                }
                let hypotheses = wiki_symbols("hypothesis_heads", &args.hypothesis_heads)?;
                let classes = wiki_symbols("typeclasses", &args.typeclasses)?;
                let mut q = WikiQuery::new(&args.text);
                q.limit = (limit + 1) as u32;
                q.scope = args.scope.unwrap_or(q.scope);
                q.engine = args.engine;
                q.conclusion_head = args.conclusion_head.as_deref();
                q.hypothesis_heads = &hypotheses;
                q.typeclasses = &classes;
                q.include_shadow_hint = args.include_shadow_hint.unwrap_or(true);
                let core = q.scope == WikiStratum::Core;
                // The operator lesson ledger is a separate advisory response, not an index.
                let include_route_lessons = args.scope == Some(WikiStratum::Graveyard)
                    && !q.query.trim().is_empty()
                    && q.engine.is_none()
                    && q.conclusion_head.is_none()
                    && q.hypothesis_heads.is_empty()
                    && q.typeclasses.is_empty();
                let (result, managed) = self
                    .wiki_read(core, |wiki, generation| match generation {
                        Some(generation) => wiki.search_at(q, generation),
                        None => wiki.search(q),
                    })
                    .map_err(internal)?;
                let mut page = wiki_page("hits", result.hits, limit, false, result.shadow_hint)?;
                if core {
                    page["managed_core"] = Value::Bool(managed);
                } else if include_route_lessons {
                    self.append_route_lessons(&mut page, [args.text.as_str()], limit)?;
                }
                Ok(page)
            }
            "wiki.check_graveyard" => {
                let args: WikiGraveyardArgs = parse(params)?;
                wiki_text("informal_statement", &args.informal_statement, 512)?;
                if let Some(signature) = &args.formal_signature {
                    wiki_text("formal_signature", signature, 512)?;
                }
                let limit = cap(args.limit, 8, 16)?;
                let wiki = self
                    .wiki
                    .lock()
                    .map_err(|_| RpcError::new(-32603, "wiki lock poisoned"))?;
                let mut seen = std::collections::BTreeSet::new();
                let mut warnings = Vec::new();
                // Treat the optional signature as another retrieval signal, not a
                // required substring of the informal statement. Never search Core.
                let queries = std::iter::once(args.informal_statement.as_str())
                    .chain(args.formal_signature.as_deref());
                for text in queries.clone() {
                    let mut q = WikiQuery::new(text);
                    q.scope = WikiStratum::Graveyard;
                    q.include_shadow_hint = false;
                    q.limit = (limit + 1) as u32;
                    for hit in wiki.search(q).map_err(internal)?.hits {
                        if seen.insert(hit.summary.id.clone()) {
                            warnings.push(hit);
                        }
                    }
                }
                drop(wiki);
                let mut page = wiki_page("warnings", warnings, limit, false, None)?;
                self.append_route_lessons(&mut page, queries, limit)?;
                Ok(page)
            }
            "wiki.provenance" => {
                let args: WikiProvenanceArgs = parse(params)?;
                wiki_text("id", &args.id, 128)?;
                let max_depth = args.max_depth.unwrap_or(2);
                if !(1..=16).contains(&max_depth) {
                    return Err(RpcError::new(-32602, "max_depth must be in 1..=16"));
                }
                let limit = cap(args.limit, 24, 64)?;
                let (result, managed) = self
                    .wiki_read(true, |wiki, generation| match generation {
                        Some(generation) => {
                            wiki.provenance_at(&args.id, max_depth, limit as u32, generation)
                        }
                        None => wiki.provenance(&args.id, max_depth, limit as u32),
                    })
                    .map_err(internal)?;
                let mut page = wiki_page("edges", result.edges, limit, result.truncated, None)?;
                page["managed_core"] = Value::Bool(managed);
                Ok(page)
            }
            _ => Err(RpcError::new(-32601, "method not found")),
        }
    }

    fn tick(
        &self,
        hub: &Hub,
        breaker_after: &mut Option<i64>,
        first_scheduler_sample: &mut bool,
    ) -> Result<()> {
        // Failed or partial scheduler sampling is never a starvation observation.
        // Keep delivering telemetry and processing other P0 sources independently.
        match self
            .store()
            .and_then(|mut store| store.reconcile_scheduler_starvation(*first_scheduler_sample))
        {
            Ok(_) => *first_scheduler_sample = false,
            Err(error) => {
                *first_scheduler_sample = true;
                eprintln!("ai4mathd scheduler observation: {error:#}");
            }
        }
        let vitals = self.status()?;
        let seq = vitals["event_seq"]
            .as_i64()
            .context("telemetry is missing event sequence")?;
        hub.publish("vital_signs.tick", "vital_signs.tick", vitals);
        let store = self.store()?;
        if let Some(from) = *breaker_after {
            let page = store.breaker_events_through(from, seq, 128)?;
            let next = if page.truncated {
                page.items
                    .last()
                    .context("truncated breaker page is empty")?
                    .event_seq
            } else {
                seq
            };
            for e in page.items {
                hub.publish(
                    "breaker.state_changed",
                    "breaker.state_changed",
                    serde_json::to_value(e)?,
                );
            }
            *breaker_after = Some(next);
        } else {
            *breaker_after = Some(seq);
        }
        let alert_candidate = store.has_alert_candidates(seq)?;
        drop(store);
        if alert_candidate {
            let facts = self.store()?.briefing_for_alerts()?;
            let pack = FactPack::compile(facts)?;
            // Materialize before recording an alert reference. The same immutable pack can
            // be shared by multiple alerts in this snapshot or a human pull.
            for alert in self
                .store()?
                .scan_alerts(pack.to_seq(), &pack.fact_sha256, || {
                    pack.retain(&self.retained_dir)
                })?
            {
                hub.publish("alert.p0", "alert.p0", serde_json::to_value(alert)?);
            }
        }
        Ok(())
    }
}

fn private_dir(path: &Path) -> Result<()> {
    match DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => (),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
        Err(e) => return Err(e.into()),
    }
    let dir = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)?;
    let meta = dir.metadata()?;
    ensure!(
        meta.uid() == unsafe { libc::geteuid() } && meta.mode() & 0o022 == 0,
        "rejected non-owned or shared runtime directory"
    );
    if meta.mode() & 0o777 != 0o700 {
        dir.set_permissions(Permissions::from_mode(0o700))?;
    }
    Ok(())
}
fn ordinary_path(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(m) => ensure!(
            m.is_file() && m.nlink() == 1 && m.uid() == unsafe { libc::geteuid() },
            "unsafe local database path {}",
            path.display()
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

pub fn run(
    project: &Path,
    tick: Duration,
    catalog: Option<&Path>,
    worker: Option<t3_worker::Config>,
    discovery: Option<t3_discovery::Config>,
    audit: Option<t2_audit::Config>,
    native_rfc: Option<crate::native_rfc::Config>,
) -> Result<()> {
    let root = if worker.is_some() {
        t3_worker::verified_root(project)?
    } else if discovery.is_some() {
        t3_discovery::verified_root(project)?
    } else if audit.is_some() {
        t2_audit::verified_root(project)?
    } else {
        project.canonicalize()?
    };
    ensure!(root.is_dir(), "project root must be a directory");
    let runtime = root.join(".runtime");
    private_dir(&runtime)?;
    // A losing instance must stop before creating Wiki/briefings directories or
    // opening either database, both of which can apply schema migrations.
    let singleton = transport::acquire_singleton(&runtime)?;
    let wiki = root.join(".mathwiki");
    // Preserve a Git-authored Wiki root's existing mode, but do not follow aliases.
    match DirBuilder::new().mode(0o700).create(&wiki) {
        Ok(()) => (),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
        Err(e) => return Err(e.into()),
    }
    let wiki_meta = fs::symlink_metadata(&wiki)?;
    ensure!(
        wiki_meta.is_dir()
            && wiki_meta.uid() == unsafe { libc::geteuid() }
            && wiki_meta.mode() & 0o022 == 0,
        "unsafe local Wiki directory"
    );
    let retained_dir = runtime.join("briefings");
    private_dir(&retained_dir)?;
    for (dir, db) in [(&runtime, "state.sqlite"), (&wiki, ".index.sqlite")] {
        for suffix in ["", "-wal", "-shm", "-journal"] {
            ordinary_path(&dir.join(format!("{db}{suffix}")))?;
        }
    }
    let store = StateStore::open(runtime.join("state.sqlite"))?;
    let wiki_index = Wiki::open(wiki.join(".index.sqlite"))?;
    let discovery_state = discovery
        .as_ref()
        .map(|_| Arc::new(Mutex::new(DiscoveryState::Starting)));
    let audit_state = audit
        .as_ref()
        .map(|_| Arc::new(Mutex::new(AuditState::starting())));
    let service = Arc::new(Project {
        root: root.clone(),
        store: Mutex::new(store),
        wiki: Mutex::new(wiki_index),
        retained_dir,
        probes: Probes::open(&root, catalog)?,
        t3_discovery: discovery_state.clone(),
        t2_audit: audit_state.clone(),
        native_rfc,
    });
    let hub = Hub::new();
    let ticker = hub.clone();
    let backend = service.clone();
    transport::serve(
        &runtime.join("ai4mathd.sock"),
        singleton,
        hub,
        move || {
            thread::Builder::new()
                .name("ai4math-telemetry".into())
                .spawn(move || {
                    let mut cursor = None;
                    let mut first_scheduler_sample = true;
                    loop {
                        if let Err(e) =
                            backend.tick(&ticker, &mut cursor, &mut first_scheduler_sample)
                        {
                            eprintln!("ai4mathd telemetry: {e:#}");
                        }
                        thread::sleep(tick);
                    }
                })?;
            if let Some(worker) = worker {
                // The listener is bound now. A worker failure must not take RPC down.
                worker.start(root.clone());
            }
            if let (Some(discovery), Some(state)) = (discovery, discovery_state) {
                discovery.start(root.clone(), state);
            }
            if let (Some(audit), Some(state)) = (audit, audit_state) {
                audit.start(root, state);
            }
            Ok(())
        },
        move |connection, method, params| service.method(connection, method, params),
    )?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn memory_sample() -> Option<Value> {
    let text = fs::read_to_string("/proc/meminfo").ok()?;
    let count = |key: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(key))?
            .split_ascii_whitespace()
            .next()?
            .parse::<u64>()
            .ok()
    };
    pressure(count("MemTotal:")?, count("MemAvailable:")?)
}

#[cfg(target_os = "macos")]
fn memory_sample() -> Option<Value> {
    let out = std::process::Command::new("/usr/bin/vm_stat")
        .env_clear()
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?;
    let n = |key: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(key))?
            .trim()
            .trim_end_matches('.')
            .parse::<u64>()
            .ok()
    };
    let free = n("Pages free:")?
        .saturating_add(n("Pages speculative:")?)
        .saturating_add(n("Pages inactive:")?);
    let used = n("Pages active:")?
        .saturating_add(n("Pages wired down:")?)
        .saturating_add(n("Pages occupied by compressor:")?);
    pressure(used.saturating_add(free), free)
}
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn memory_sample() -> Option<Value> {
    None
}
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn pressure(total: u64, free: u64) -> Option<Value> {
    if total == 0 {
        return None;
    }
    let percent = ((total.saturating_sub(free) as u128 * 100) / total as u128) as u8;
    let pressure = if percent > 85 {
        "RED"
    } else if percent >= 70 {
        "YELLOW"
    } else {
        "GREEN"
    };
    Some(json!({"used_percent":percent,"pressure":pressure}))
}
