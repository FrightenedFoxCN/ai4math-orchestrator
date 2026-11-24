use crate::{
    facts::{self, FactPack},
    proofs::{self, Probes},
    t3_worker,
    transport::{self, Connection, Hub, RpcError},
};
use ai4math_store::{StateStore, Wiki, WikiQuery};
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
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Proof {
    target_id: String,
    proof_body: String,
}

fn parse<T: serde::de::DeserializeOwned>(
    map: Map<String, Value>,
) -> std::result::Result<T, RpcError> {
    serde_json::from_value(Value::Object(map)).map_err(|e| RpcError::new(-32602, e.to_string()))
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
fn response<T: serde::Serialize>(v: T) -> std::result::Result<Value, RpcError> {
    serde_json::to_value(v).map_err(|e| internal(e.into()))
}

struct Project {
    store: Mutex<StateStore>,
    wiki: Mutex<Wiki>,
    retained_dir: PathBuf,
    probes: Probes,
}

impl Project {
    fn store(&self) -> Result<std::sync::MutexGuard<'_, StateStore>> {
        self.store
            .lock()
            .map_err(|_| anyhow::anyhow!("state lock poisoned"))
    }
    fn status(&self) -> Result<Value> {
        let mut data = serde_json::to_value(self.store()?.live_telemetry()?)?;
        data["memory"] = memory_sample().unwrap_or(Value::Null);
        // These counters cover the daemon broker, not independent CLI processes.
        data["broker"] = serde_json::to_value(self.probes.load()?)?;
        data["workers"] = Value::Null;
        Ok(data)
    }
    fn method(
        &self,
        connection: &Connection,
        method: &str,
        params: Map<String, Value>,
    ) -> std::result::Result<Value, RpcError> {
        match method {
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
                let result = self
                    .wiki
                    .lock()
                    .map_err(|_| RpcError::new(-32603, "wiki lock poisoned"))?
                    .search(q)
                    .map_err(internal)?;
                wiki_page("hits", result.hits, limit, false, result.shadow_hint)
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
                for text in std::iter::once(args.informal_statement.as_str())
                    .chain(args.formal_signature.as_deref())
                {
                    let mut q = WikiQuery::new(text);
                    q.scope = ai4math_store::WikiStratum::Graveyard;
                    q.include_shadow_hint = false;
                    q.limit = (limit + 1) as u32;
                    for hit in wiki.search(q).map_err(internal)?.hits {
                        if seen.insert(hit.summary.id.clone()) {
                            warnings.push(hit);
                        }
                    }
                }
                drop(wiki);
                wiki_page("warnings", warnings, limit, false, None)
            }
            "wiki.provenance" => {
                let args: WikiProvenanceArgs = parse(params)?;
                wiki_text("id", &args.id, 128)?;
                let max_depth = args.max_depth.unwrap_or(2);
                if !(1..=16).contains(&max_depth) {
                    return Err(RpcError::new(-32602, "max_depth must be in 1..=16"));
                }
                let limit = cap(args.limit, 24, 64)?;
                let result = self
                    .wiki
                    .lock()
                    .map_err(|_| RpcError::new(-32603, "wiki lock poisoned"))?
                    .provenance(&args.id, max_depth, limit as u32)
                    .map_err(internal)?;
                wiki_page("edges", result.edges, limit, result.truncated, None)
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
) -> Result<()> {
    let root = if worker.is_some() {
        t3_worker::verified_root(project)?
    } else {
        project.canonicalize()?
    };
    ensure!(root.is_dir(), "project root must be a directory");
    let runtime = root.join(".runtime");
    let wiki = root.join(".mathwiki");
    private_dir(&runtime)?;
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
    let service = Arc::new(Project {
        store: Mutex::new(StateStore::open(runtime.join("state.sqlite"))?),
        wiki: Mutex::new(Wiki::open(wiki.join(".index.sqlite"))?),
        retained_dir,
        probes: Probes::open(&root, catalog)?,
    });
    let hub = Hub::new();
    let ticker = hub.clone();
    let backend = service.clone();
    transport::serve(
        &runtime.join("ai4mathd.sock"),
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
                worker.start(root);
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
