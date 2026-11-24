#![cfg(unix)]
use ai4math_store::{Change, NewClaim, NewPlan, NewTask, PlanStatus, StateStore, TaskTrack};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::{fs::PermissionsExt, net::UnixStream},
    path::Path,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

struct Server(Child);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn wait_connection(socket: &Path, child: &mut Server) -> Result<Rpc> {
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(stream) = UnixStream::connect(socket) {
            return Rpc::new(stream);
        }
        ensure!(
            child.0.try_wait()?.is_none(),
            "daemon exited during startup"
        );
        ensure!(Instant::now() < until, "daemon failed to create socket");
        thread::sleep(Duration::from_millis(20));
    }
}
struct Rpc {
    reader: BufReader<UnixStream>,
    stream: UnixStream,
    next: u32,
}
impl Rpc {
    fn new(stream: UnixStream) -> Result<Self> {
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        Ok(Self {
            reader: BufReader::new(stream.try_clone()?),
            stream,
            next: 0,
        })
    }
    fn receive(&mut self) -> Result<Value> {
        let mut line = String::new();
        ensure!(
            self.reader.read_line(&mut line)? > 0,
            "daemon stream closed"
        );
        Ok(serde_json::from_str(&line)?)
    }
    fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        self.next += 1;
        let id = self.next;
        writeln!(
            self.stream,
            "{}",
            json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
        )?;
        for _ in 0..50 {
            let m = self.receive()?;
            if m["id"] == id {
                return Ok(m);
            }
        }
        anyhow::bail!("no RPC response")
    }
    fn event(&mut self, kind: &str) -> Result<Value> {
        for _ in 0..50 {
            let m = self.receive()?;
            if m["method"] == kind {
                return Ok(m["params"].clone());
            }
        }
        anyhow::bail!("missing push {kind}")
    }
}

// Pre-upgrade rows still pass through the existing cooldown and retained
// projection. This fixture is deliberately not a production store API.
fn legacy_breaker_event(state: &Path, id: &str) -> Result<i64> {
    let db = rusqlite::Connection::open(state)?;
    db.busy_timeout(Duration::from_secs(5))?;
    Ok(db.query_row("INSERT INTO events(occurred_at,event_type,entity_type,entity_id,actor)
        VALUES('2026-01-01T00:00:00Z','CRITICAL_BREAKER_TRIPPED','breaker',?1,'legacy:fixture') RETURNING event_seq",[id],|r|r.get(0))?)
}

fn boot(path: &Path) -> Result<Server> {
    Ok(Server(
        Command::new(env!("CARGO_BIN_EXE_ai4mathd"))
            .args([
                "--project",
                path.to_str().context("UTF8 fixture")?,
                "--tick-ms",
                "150",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .spawn()?,
    ))
}

#[test]
fn wiki_rpc_is_read_only_strict_and_bounded() -> Result<()> {
    let dir = tempfile::Builder::new()
        .prefix("a4")
        .rand_bytes(4)
        .tempdir()?;
    let wiki = dir.path().join(".mathwiki");
    for stratum in ["core", "sediment", "graveyard"] {
        fs::create_dir_all(wiki.join(stratum))?;
    }
    fs::write(
        wiki.join("core/COR-A.md"),
        "---\nid: COR-A\nstratum: core\ntrust_level: L5\nengine: lean4\nstatus: active\ntitle: Signal\nsymbols:\n  conclusion_head: LE.le\n  hypothesis_heads: [Integrable]\n  typeclasses: [MeasureSpace]\n---\n",
    )?;
    fs::write(
        wiki.join("sediment/SED-A.md"),
        "---\nid: SED-A\nstratum: sediment\ntrust_level: L5\nengine: lean4\nstatus: superseded\ntitle: Signal\norigin_claim: CLM-42\nedges:\n  - dst_id: COR-A\n    edge_type: superseded_by\n    metadata: {reason: upstream}\n---\n",
    )?;
    fs::write(
        wiki.join("graveyard/GRV-A.md"),
        "---\nid: GRV-A\nstratum: graveyard\ntrust_level: L2b\nengine: lean4\nstatus: refuted\ntitle: Signal\nformal_signature: 'theorem grave_signal : False'\nedges:\n  - {dst_id: COR-A, edge_type: refutes}\n---\n",
    )?;
    let large = "巨".repeat(30_000);
    let deep = format!("{}0{}", "[".repeat(65), "]".repeat(65));
    fs::write(
        wiki.join("core/COR-B.md"),
        format!(
            "---\nid: COR-B\nstratum: core\ntrust_level: L5\nengine: lean4\nstatus: active\ntitle: Large\ninformal_gloss: {large}\nedges:\n  - dst_id: CLM-B\n    edge_type: born_from_claim\n    metadata: {{note: {large}}}\n  - dst_id: CLM-DEEP\n    edge_type: depends_on\n    metadata: {{note: {deep}}}\n---\n"
        ),
    )?;
    assert_eq!(
        ai4math_store::Wiki::open(wiki.join(".index.sqlite"))?.rebuild(&wiki)?,
        4
    );
    let mut child = boot(dir.path())?;
    let mut rpc = wait_connection(&dir.path().join(".runtime/ai4mathd.sock"), &mut child)?;
    for (method, params) in [
        ("wiki.search", json!({"text":"  "})),
        ("wiki.search", json!({"text":"巨".repeat(171)})),
        ("wiki.search", json!({"text":"signal","scope":null})),
        ("wiki.search", json!({"text":"signal","engine":"python"})),
        (
            "wiki.search",
            json!({"text":"signal","hypothesis_heads":["A","A"]}),
        ),
        (
            "wiki.search",
            json!({"text":"signal","include_shadow_hint":1}),
        ),
        (
            "wiki.check_graveyard",
            json!({"informal_statement":"signal","scope":"core"}),
        ),
        (
            "wiki.check_graveyard",
            json!({"informal_statement":"signal","formal_signature":null}),
        ),
        (
            "wiki.check_graveyard",
            json!({"informal_statement":"signal","limit":17}),
        ),
        ("wiki.provenance", json!({"id":"COR-A","role":"chief"})),
        ("wiki.provenance", json!({"id":"COR-A","max_depth":0})),
        ("wiki.provenance", json!({"id":"COR-A","max_depth":17})),
        ("wiki.provenance", json!({"id":"COR-A","limit":65})),
        ("wiki.provenance", json!({"id":"COR-A","limit":null})),
    ] {
        let result = rpc.call(method, params.clone())?;
        assert_eq!(result["error"]["code"], -32602, "{method}: {params}");
    }
    let signal = rpc.call("wiki.search", json!({"text":"signal","engine":"lean4","conclusion_head":"LE.le",
        "hypothesis_heads":["Integrable"],"typeclasses":["MeasureSpace"],"include_shadow_hint":false}))?;
    assert_eq!(signal["result"]["hits"][0]["id"], "COR-A");
    assert!(signal["result"].get("shadow_hint").is_none());
    let warnings = rpc.call(
        "wiki.check_graveyard",
        json!({"informal_statement":"unmatched","formal_signature":"grave_signal"}),
    )?;
    assert_eq!(warnings["result"]["warnings"].as_array().unwrap().len(), 1);
    assert_eq!(warnings["result"]["warnings"][0]["id"], "GRV-A");
    assert!(warnings["result"].get("hits").is_none());
    let limited = rpc.call(
        "wiki.provenance",
        json!({"id":"COR-A","max_depth":1,"limit":1}),
    )?;
    assert_eq!(limited["result"]["edges"].as_array().unwrap().len(), 1);
    assert_eq!(limited["result"]["truncated"], true);
    let edge = rpc.call("wiki.provenance", json!({"id":"COR-A","max_depth":2}))?;
    assert_eq!(edge["result"]["edges"][0]["edge_type"], "refutes");
    assert_eq!(edge["result"]["edges"][1]["src_id"], "SED-A");
    assert_eq!(edge["result"]["edges"][1]["dst_id"], "COR-A");
    assert_eq!(edge["result"]["edges"][1]["metadata"]["reason"], "upstream");
    assert_eq!(edge["result"]["edges"][2]["depth"], 2);
    assert_eq!(edge["result"]["edges"][2]["dst_id"], "CLM-42");
    assert_eq!(edge["result"]["truncated"], false);
    for (method, input, output) in [
        ("wiki.search", json!({"text":"large"}), "hits"),
        ("wiki.provenance", json!({"id":"COR-B"}), "edges"),
        ("wiki.provenance", json!({"id":"CLM-DEEP"}), "edges"),
    ] {
        let oversized = rpc.call(method, input)?;
        assert_eq!(oversized["result"][output], json!([]));
        assert_eq!(oversized["result"]["truncated"], true);
        assert!(serde_json::to_vec(&oversized["result"])?.len() <= 64 * 1024);
    }
    assert_eq!(
        rpc.call("wiki.rehydrate", json!({"id":"SED-A"}))?["error"]["code"],
        -32601
    );
    assert_eq!(fs::read_dir(wiki.join("sediment"))?.count(), 1);
    Ok(())
}

#[test]
fn real_daemon_subscriptions_facts_and_p0_are_consistent() -> Result<()> {
    let dir = tempfile::Builder::new()
        .prefix("a4")
        .rand_bytes(4)
        .tempdir()?;
    let runtime = dir.path().join(".runtime");
    let socket = runtime.join("ai4mathd.sock");
    let mut child = boot(dir.path())?;
    let mut listener = wait_connection(&socket, &mut child)?;
    assert_eq!(fs::metadata(&runtime)?.permissions().mode() & 0o777, 0o700);
    assert_eq!(fs::metadata(&socket)?.permissions().mode() & 0o777, 0o600);
    let subs = listener.call(
        "subscribe",
        json!({"topics":["vital_signs.tick","alert.p0","breaker.state_changed"]}),
    )?;
    assert_eq!(subs["result"]["topics"][0], "vital_signs.tick");
    let initial = listener.event("vital_signs.tick")?;
    assert_eq!(initial["event_seq"], 0);
    assert_eq!(initial["trust"]["L5"], 0);
    let mut caller = Rpc::new(UnixStream::connect(&socket)?)?;
    assert_eq!(
        caller.call("./unknown", json!({}))?["error"]["code"],
        -32601
    );
    assert_eq!(
        caller.call("status", json!({"role":"chief"}))?["error"]["code"],
        -32602
    );
    assert_eq!(
        caller.call("frontier.list", json!({"limit":99}))?["error"]["code"],
        -32602
    );
    let first = caller.call("briefing.peek", json!({}))?["result"].clone();
    assert!(first["retained_id"].is_null());
    assert_eq!(first, caller.call("briefing.peek", json!({}))?["result"]);
    assert_eq!(fs::read_dir(runtime.join("briefings"))?.count(), 0);
    let pulled = caller.call("briefing.pull", json!({}))?["result"].clone();
    let digest = pulled["retained_id"]
        .as_str()
        .context("briefing retained ID")?;
    assert_eq!(pulled["fact_pack"], first["fact_pack"]);
    assert_eq!(
        caller.call("briefing.get", json!({"id":digest}))?["result"],
        pulled
    );
    assert_eq!(
        caller.call("briefing.get", json!({"id":"../elsewhere"}))?["error"]["code"],
        -32602
    );
    assert!(
        runtime
            .join("briefings")
            .join(format!("{digest}.json"))
            .is_file()
    );
    assert!(
        runtime
            .join("briefings")
            .join(format!("{digest}.md"))
            .is_file()
    );
    assert_eq!(
        caller.call("wiki.search", json!({"text":"none"}))?["result"]["hits"],
        json!([])
    );

    let mut store = StateStore::open(runtime.join("state.sqlite"))?;
    let change = Change {
        actor: "integration",
        git_commit: Some("1111111111111111111111111111111111111111"),
    };
    store.create_claim(
        NewClaim {
            id: "C",
            title: "Critical",
            informal_statement: "Statement",
            engine: "lean4",
            formal_target: None,
            module_path: None,
            critical: true,
        },
        change,
    )?;
    let frontier = caller.call("frontier.list", json!({}))?;
    assert_eq!(frontier["result"]["items"][0]["claim_id"], "C");
    store.append_event(
        change,
        "BREAKER_STATE_CHANGED",
        "breaker",
        "B",
        json!({"state":"TRIPPED"}),
    )?;
    assert_eq!(listener.event("breaker.state_changed")?["entity_id"], "B");
    // Keep testing compatibility and cooldown for a pre-upgrade alert.
    legacy_breaker_event(&runtime.join("state.sqlite"), "C")?;
    let push = listener.event("alert.p0")?;
    assert_eq!(push["reason_code"], "CRITICAL_BREAKER_TRIPPED");
    assert_eq!(push["related_entities"], json!(["C"]));
    let alert_hash = push["briefing_id"].as_str().context("P0 briefing ID")?;
    assert!(
        runtime
            .join("briefings")
            .join(format!("{alert_hash}.json"))
            .is_file()
    );
    let retained = caller.call("briefing.get", json!({"id":alert_hash}))?["result"].clone();
    assert_eq!(
        retained["fact_pack"]["alert_sources"][0]["event_seq"],
        push["event_seq"]
    );
    let old = push["event_seq"].as_i64().context("P0 sequence")?;
    let retained_count = fs::read_dir(runtime.join("briefings"))?.count();
    let duplicate = legacy_breaker_event(&runtime.join("state.sqlite"), "C")?;
    std::thread::sleep(Duration::from_millis(400));
    assert!(duplicate > old);
    assert_eq!(
        fs::read_dir(runtime.join("briefings"))?.count(),
        retained_count
    );
    let history = caller.call("alerts.list", json!({"after":0}))?;
    assert_eq!(history["result"]["items"], json!([push]));
    assert_eq!(
        caller.call("inbox.list", json!({}))?["result"]["items"],
        json!([])
    );
    Ok(())
}

#[test]
fn legacy_p0_sources_are_distinct_from_normal_progress() -> Result<()> {
    let dir = tempfile::Builder::new()
        .prefix("a4")
        .rand_bytes(4)
        .tempdir()?;
    let runtime = dir.path().join(".runtime");
    let mut daemon = boot(dir.path())?;
    let mut rpc = wait_connection(&runtime.join("ai4mathd.sock"), &mut daemon)?;
    let mut store = StateStore::open(runtime.join("state.sqlite"))?;
    let change = Change {
        actor: "trusted-fixture",
        git_commit: None,
    };
    // Other sources are covered by native/production tests or still have no
    // dedicated detector. This tests the legacy event-to-delivery behavior.
    let signals = ["CRITICAL_BREAKER_TRIPPED"];
    store.append_event(
        change,
        "CLAIM_TRUST_CHANGED",
        "claim",
        "normal",
        json!({"local":"L2a"}),
    )?;
    legacy_breaker_event(&runtime.join("state.sqlite"), "legacy-fixture")?;
    let until = Instant::now() + Duration::from_secs(4);
    loop {
        let reply = rpc.call("alerts.list", json!({}))?;
        let list = reply["result"]["items"].as_array().context("alert page")?;
        if list.len() >= signals.len() {
            let actual = list
                .iter()
                .map(|a| a["reason_code"].as_str().unwrap())
                .collect::<Vec<_>>();
            assert_eq!(actual, signals);
            break;
        }
        ensure!(
            Instant::now() < until,
            "timed out waiting for trusted P0 signals"
        );
        thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

#[test]
fn critical_breaker_alert_requires_the_last_known_route_to_be_closed() -> Result<()> {
    let dir = tempfile::Builder::new()
        .prefix("br")
        .rand_bytes(4)
        .tempdir()?;
    let root = dir.path().join(".runtime");
    let state = root.join("state.sqlite");
    let socket = root.join("ai4mathd.sock");
    let mut daemon = boot(dir.path())?;
    let mut stream = wait_connection(&socket, &mut daemon)?;
    stream.call("subscribe", json!({"topics":["alert.p0"]}))?;
    let mut rpc = Rpc::new(UnixStream::connect(&socket)?)?;
    let mut store = StateStore::open(&state)?;
    let base = "a".repeat(40);
    let change = Change {
        actor: "critical-route-fixture",
        git_commit: Some(&base),
    };
    assert!(
        store
            .append_event(
                change,
                "CRITICAL_BREAKER_TRIPPED",
                "breaker",
                "forged",
                json!({})
            )
            .is_err()
    );
    for (id, module, decl, critical) in [
        ("A", "Alpha.lean", "a", false),
        ("B", "Beta.lean", "b", false),
        ("Relay", "Relay.lean", "lemma", false),
        ("Target", "Goal.lean", "goal", true),
        ("Unrelated", "Other.lean", "goal", true),
    ] {
        let target = format!("{module}#{decl}");
        store.create_claim(
            NewClaim {
                id,
                title: id,
                informal_statement: id,
                engine: "lean4",
                formal_target: Some(&target),
                module_path: Some(module),
                critical,
            },
            change,
        )?;
    }
    for (plan, target, deps) in [
        ("route-a", "Target", &[("A", "definition")][..]),
        ("route-b", "Target", &[("Relay", "lemma")][..]),
        ("relay-from-b", "Relay", &[("B", "definition")][..]),
    ] {
        store.create_plan(
            NewPlan {
                id: plan,
                target,
                name: plan,
                track: "exploration",
                status: PlanStatus::Active,
                dependencies: deps,
            },
            change,
        )?;
    }
    let tripped = |store: &mut StateStore, id, module, decl| {
        store.register_tripped_breaker_report(
            "/breaker-fixture.git",
            &base,
            id,
            decl,
            module,
            "lean4",
            "operator definition report",
            true,
        )
    };
    let first = tripped(&mut store, "first", "Alpha.lean", "a")?;
    assert_eq!(first.critical_claim_count, 0);
    assert_eq!(
        rpc.call("alerts.list", json!({}))?["result"]["items"],
        json!([])
    );
    let last = tripped(&mut store, "last", "Beta.lean", "b")?;
    assert_eq!(last.critical_claim_count, 1);
    let pushed = stream.event("alert.p0")?;
    assert_eq!(pushed["reason_code"], "CRITICAL_BREAKER_TRIPPED");
    assert_eq!(pushed["related_entities"], json!(["last"]));
    let kept = rpc.call("briefing.get", json!({"id":pushed["briefing_id"]}))?;
    let facts = &kept["result"]["fact_pack"];
    assert_eq!(
        facts["alert_sources"][0]["event_seq"], pushed["event_seq"],
        "{kept}"
    );
    let payload = &facts["alert_sources"][0]["payload"];
    assert_eq!(payload["critical_claim_id"], "Target");
    assert_eq!(payload["critical_claim_count"], 1);
    assert_eq!(payload["trip_event_seq"], last.event_seq);
    assert_eq!(facts["alert_sources"].as_array().unwrap().len(), 1);
    let inbox = rpc.call("inbox.list", json!({}))?;
    for (id, priority) in [("DEF-first", "P1"), ("DEF-last", "P0")] {
        assert!(
            inbox["result"]["items"]
                .as_array()
                .unwrap()
                .iter()
                .any(|i| i["inbox_id"] == id && i["priority"] == priority)
        );
    }
    assert_eq!(
        store.claim("Target")?.unwrap().effective_trust,
        ai4math_store::Trust::L0
    );
    Ok(())
}

#[test]
fn scheduler_starvation_pushes_and_retains_facts_once_across_restart() -> Result<()> {
    let dir = tempfile::Builder::new()
        .prefix("st")
        .rand_bytes(4)
        .tempdir()?;
    let runtime = dir.path().join(".runtime");
    let socket = runtime.join("ai4mathd.sock");
    let mut server = boot(dir.path())?;
    let mut listener = wait_connection(&socket, &mut server)?;
    listener
        .stream
        .set_read_timeout(Some(Duration::from_secs(10)))?;
    listener.call("subscribe", json!({"topics":["alert.p0"]}))?;
    let mut caller = Rpc::new(UnixStream::connect(&socket)?)?;
    let mut store = StateStore::open(runtime.join("state.sqlite"))?;
    let change = Change {
        actor: "scheduler-fixture",
        git_commit: None,
    };
    store.register_task(
        NewTask {
            id: "explore",
            track: TaskTrack::Exploration,
            engine: "lean4",
            module_path: "Proof.lean",
            target: "goal",
            dependencies: &[],
        },
        change,
    )?;
    // A non-critical incident alone is not a source of P0. It blocks an
    // exploration task; an independent module in the other track awaits it.
    store.register_tripped_breaker(
        "/starvation-fixture.git",
        &"a".repeat(40),
        "blocking-breaker",
        "goal",
        "Proof.lean",
        "lean4",
        "fixture blocked",
        false,
    )?;
    assert_eq!(
        caller.call("alerts.list", json!({}))?["result"]["items"],
        json!([])
    );
    store.register_task(
        NewTask {
            id: "infra",
            track: TaskTrack::Infrastructure,
            engine: "lean4",
            module_path: "Independent.lean",
            target: "helper",
            dependencies: &["explore"],
        },
        change,
    )?;
    let push = listener.event("alert.p0")?;
    assert_eq!(push["reason_code"], "ALL_TRACKS_BLOCKED");
    assert_eq!(push["related_entities"], json!(["scheduler"]));
    let hash = push["briefing_id"]
        .as_str()
        .context("missing starvation Fact Pack id")?;
    for suffix in ["json", "md"] {
        assert!(
            runtime
                .join("briefings")
                .join(format!("{hash}.{suffix}"))
                .is_file()
        );
    }
    let kept = caller.call("briefing.get", json!({"id":hash}))?;
    let source = &kept["result"]["fact_pack"]["alert_sources"][0];
    assert_eq!(source["event_seq"], push["event_seq"], "{kept}");
    assert_eq!(source["payload"]["exploration_blocked"], 1, "{source}");
    assert_eq!(source["payload"]["infrastructure_blocked"], 1, "{source}");
    let seq = store.status()?.last_event_seq;
    drop(listener);
    drop(caller);
    // The daemon intentionally does not unlink an existing socket on startup.
    // Simulate operator cleanup of this exact fixture path after confirmed exit.
    server.0.kill()?;
    server.0.wait()?;
    use std::os::unix::fs::FileTypeExt;
    assert!(fs::symlink_metadata(&socket)?.file_type().is_socket());
    fs::remove_file(&socket)?;
    let mut restarted = boot(dir.path())?;
    let mut after = wait_connection(&socket, &mut restarted)?;
    after.call("subscribe", json!({"topics":["vital_signs.tick"]}))?;
    assert_eq!(after.event("vital_signs.tick")?["event_seq"], seq);
    assert_eq!(
        after.call("alerts.list", json!({}))?["result"]["items"],
        json!([push])
    );
    assert_eq!(store.status()?.last_event_seq, seq);
    Ok(())
}

#[test]
#[cfg(target_os = "macos")]
#[ignore = "requires native Seatbelt, AI4MATH_TEST_LEAN, and built AI4MATH_TEST_CLI"]
fn native_managed_regressions_raise_one_real_meltdown_p0() -> Result<()> {
    let cli = std::env::var_os("AI4MATH_TEST_CLI").context("set AI4MATH_TEST_CLI")?;
    let lean = std::env::var("AI4MATH_TEST_LEAN").context("set native Lean")?;
    let dir = tempfile::Builder::new()
        .prefix("rm")
        .rand_bytes(4)
        .tempdir()?;
    let root = dir.path();
    let path = root.to_str().context("UTF-8 project")?;
    let source = root.join("src");
    let repo = root.join("repo.git");
    let bare = repo.to_str().context("UTF-8 repository")?;
    fs::create_dir(&source)?;
    let git = |cwd: &Path, args: &[&str]| -> Result<String> {
        let result = Command::new("/usr/bin/git")
            .current_dir(cwd)
            .args([
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@localhost",
                "-c",
                "commit.gpgSign=false",
                "-c",
                "core.hooksPath=/dev/null",
            ])
            .args(args)
            .output()?;
        ensure!(
            result.status.success(),
            "git: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        Ok(String::from_utf8(result.stdout)?.trim().to_owned())
    };
    let operator = |args: &[&str]| Command::new(&cli).args(args).output();
    git(&source, &["init", "--initial-branch=main"])?;
    fs::write(
        source.join("Proof.lean"),
        "theorem target : True := {{PROOF_BODY}}\n",
    )?;
    fs::write(
        source.join("Downstream.lean"),
        "import Proof\ntheorem broken : False := target\n",
    )?;
    fs::write(
        source.join("ai4math-modules.txt"),
        "Proof.lean\nDownstream.lean\n",
    )?;
    git(&source, &["add", "."])?;
    git(&source, &["commit", "-m", "frozen regression fixture"])?;
    let base = git(&source, &["rev-parse", "HEAD"])?;
    git(root, &["clone", "--bare", "src", "repo.git"])?;
    for args in [
        vec!["init", path, "lean4"],
        vec![
            "declare-claim",
            path,
            "C",
            "Proof.lean",
            "target",
            "Candidate",
            "The registered proposition is true",
        ],
        vec!["task-bind", path, bare, "Proof.lean", &base],
    ] {
        let out = operator(&args)?;
        ensure!(
            out.status.success(),
            "{}: {}",
            args[0],
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let mut server = boot(root)?;
    let socket = root.join(".runtime/ai4mathd.sock");
    let mut listener = wait_connection(&socket, &mut server)?;
    listener.call("subscribe", json!({"topics":["alert.p0"]}))?;
    let mut caller = Rpc::new(UnixStream::connect(&socket)?)?;
    let store = StateStore::open_readonly(root.join(".runtime/state.sqlite"))?;
    let before = store.status()?.last_event_seq;
    let body = root.join("reviewed-proof.txt");
    let run_bad = |input: &str, expected_regression: bool| -> Result<()> {
        fs::write(&body, input)?;
        let out = operator(&[
            "promote-managed",
            path,
            bare,
            "C",
            "Proof.lean",
            "target",
            &lean,
            body.to_str().context("UTF-8 body")?,
        ])?;
        let diagnostic = String::from_utf8_lossy(&out.stderr);
        ensure!(!out.status.success(), "unexpected promotion of {input}");
        assert_eq!(
            diagnostic.contains("native Lean downstream regression rejected Downstream.lean"),
            expected_regression,
            "{diagnostic}"
        );
        Ok(())
    };
    run_bad("False.elim", false)?;
    run_bad("True.intro", true)?;
    run_bad("True.intro", true)?;
    run_bad("(True.intro)", true)?;
    assert_eq!(store.status()?.last_event_seq, before);
    assert_eq!(
        caller.call("alerts.list", json!({}))?["result"]["items"],
        json!([])
    );
    run_bad("by exact True.intro", true)?;
    let alert = listener.event("alert.p0")?;
    assert_eq!(alert["reason_code"], "BROKER_REGRESSION_MELTDOWN");
    assert_eq!(alert["related_entities"], json!(["C"]));
    let retained = caller.call("briefing.get", json!({"id":alert["briefing_id"]}))?;
    let event = &retained["result"]["fact_pack"]["alert_sources"][0];
    assert_eq!(event["event_seq"], alert["event_seq"], "{retained}");
    assert_eq!(
        event["payload"],
        json!({"distinct_candidates":3,"failed_module":"Downstream.lean","window_seconds":900})
    );
    run_bad("by exact True.intro", true)?;
    assert_eq!(store.status()?.last_event_seq, before + 1);
    assert_eq!(
        store.claim("C")?.unwrap().local_trust,
        ai4math_store::Trust::L0
    );
    assert!(store.promotion_intent()?.is_none());
    assert_eq!(git(&repo, &["rev-parse", "refs/heads/main"])?, base);
    // Fix the tracked module at a new managed main and prove the same Claim.
    // The retained incident remains a fact, not a lasting promotion gate.
    fs::write(
        source.join("Downstream.lean"),
        "import Proof\ntheorem repaired : True := target\n",
    )?;
    git(&source, &["add", "Downstream.lean"])?;
    git(&source, &["commit", "-m", "repair downstream fixture"])?;
    git(&source, &["push", bare, "HEAD:refs/heads/main"])?;
    let out = operator(&[
        "promote-managed",
        path,
        bare,
        "C",
        "Proof.lean",
        "target",
        &lean,
        body.to_str().context("UTF-8 body")?,
    ])?;
    ensure!(
        out.status.success(),
        "repaired path: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        store.claim("C")?.unwrap().local_trust,
        ai4math_store::Trust::L5
    );
    assert_eq!(
        caller.call("alerts.list", json!({}))?["result"]["items"],
        json!([alert])
    );
    Ok(())
}

#[test]
#[ignore = "requires AI4MATH_TEST_CLI pointing to the built ai4math operator binary"]
fn operator_pivot_observed_by_real_daemon() -> Result<()> {
    let cli = std::env::var_os("AI4MATH_TEST_CLI").context("set AI4MATH_TEST_CLI")?;
    let root = tempfile::Builder::new()
        .prefix("a4")
        .rand_bytes(4)
        .tempdir()?;
    let project = root.path().to_str().context("UTF-8 project")?;
    let repo = root.path().join("repo.git");
    let bare = repo.to_str().context("UTF-8 repo")?;
    let source = root.path().join("src");
    fs::create_dir(&source)?;
    let git = |dir: &Path, args: &[&str]| -> Result<()> {
        let r = Command::new("/usr/bin/git")
            .current_dir(dir)
            .args([
                "-c",
                "user.name=PivotFixture",
                "-c",
                "user.email=fixture@localhost",
                "-c",
                "commit.gpgSign=false",
                "-c",
                "core.hooksPath=/dev/null",
            ])
            .args(args)
            .output()?;
        ensure!(
            r.status.success(),
            "git fixture: {}",
            String::from_utf8_lossy(&r.stderr)
        );
        Ok(())
    };
    let run = |args: &[&str]| -> Result<Value> {
        let r = Command::new(&cli).args(args).output()?;
        ensure!(
            r.status.success(),
            "ai4math {}: {}",
            args[0],
            String::from_utf8_lossy(&r.stderr)
        );
        if r.stdout.is_empty() {
            Ok(Value::Null)
        } else {
            Ok(serde_json::from_slice(&r.stdout)?)
        }
    };
    git(&source, &["init", "--initial-branch=main"])?;
    fs::write(source.join("Spec.lean"), "def target : Prop := True\n")?;
    git(&source, &["add", "Spec.lean"])?;
    git(&source, &["commit", "-m", "frozen"])?;
    git(root.path(), &["clone", "--bare", "src", "repo.git"])?;
    run(&["init", project, "lean4"])?;
    run(&[
        "declare-claim",
        project,
        "CriticalDefinition",
        "Spec.lean",
        "target",
        "Reviewed definition",
        "Recorded critical target",
        "--critical",
    ])?;
    let mut child = boot(root.path())?;
    let mut rpc = wait_connection(&root.path().join(".runtime/ai4mathd.sock"), &mut child)?;
    assert!(
        rpc.call(
            "subscribe",
            json!({"topics":["alert.p0","breaker.state_changed"]})
        )?["result"]
            .is_object()
    );
    let tripped = run(&[
        "trip-breaker",
        project,
        bare,
        "B",
        "Spec.lean",
        "target",
        "Critical smell reported",
        "--critical",
    ])?;
    assert_eq!(tripped["inbox_priority"], "P0");
    assert_eq!(tripped["critical_claim_count"], 1);
    let alert = rpc.event("alert.p0")?;
    assert_eq!(alert["reason_code"], "CRITICAL_BREAKER_TRIPPED");
    let alert_id = alert["briefing_id"]
        .as_str()
        .context("P0 retained briefing")?;
    assert_eq!(
        rpc.call("briefing.get", json!({"id":alert_id}))?["result"]["fact_pack"]["alert_sources"]
            [0]["event_seq"],
        alert["event_seq"]
    );
    let pulled = rpc.call("briefing.pull", json!({}))?;
    let fact = pulled["result"]["retained_id"]
        .as_str()
        .context("retained ID")?;
    let reason = "Keep definition after human review; require probes before release";
    let preview = run(&["pivot-preview", project, bare, fact, "B", reason])?;
    let id = preview["adr_id"].as_str().context("ADR ID")?;
    assert_eq!(
        rpc.call("inbox.list", json!({}))?["result"]["items"][0]["inbox_id"],
        "DEF-B"
    );
    assert_eq!(pulled["result"]["fact_pack"]["checkpoint"], Value::Null);
    let result = run(&["pivot-apply", project, bare, fact, "B", reason, id])?;
    assert_eq!(result["probes_started"], false);
    let transition = rpc.event("breaker.state_changed")?;
    assert_eq!(transition["payload"]["state"], "HALF_OPEN");
    assert_eq!(transition["payload"]["adr_id"], id);
    let later = rpc.call("briefing.peek", json!({}))?;
    let pack = &later["result"]["fact_pack"];
    assert_eq!(
        pack["checkpoint"]["event_seq"],
        pulled["result"]["fact_pack"]["event_to_inclusive"]
    );
    assert_eq!(pack["checkpoint"]["last_adr_id"], id);
    assert_eq!(pack["breakers"]["items"][0]["state"], "HALF_OPEN");
    assert_eq!(pack["inbox"]["items"], json!([]));
    assert!(
        pack["events"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| event["event_type"] == "PIVOT_APPLIED"
                && event["payload"]["fact_sha256"] == fact)
    );
    assert_eq!(transition["payload"]["probe_status"], "AWAITING_REVIEW");
    run(&[
        "task-register",
        project,
        bare,
        "task-E",
        "exploration",
        "Spec.lean",
        "target",
    ])?;
    run(&[
        "task-register",
        project,
        bare,
        "task-I",
        "infrastructure",
        "Spec.lean",
        "target",
    ])?;
    let status = rpc.call("status", json!({}))?;
    assert_eq!(status["result"]["half_open_breakers"], 1);
    for track in ["exploration", "infrastructure"] {
        assert_eq!(
            status["result"]["task_scheduler"][track]["breaker_blocked"],
            1
        );
    }
    let preview = run(&["breaker-probes-preview", project, bare, "B", "probeReady"])?;
    let approval = preview["approval_id"].as_str().context("probe approval")?;
    run(&[
        "breaker-probes-arm",
        project,
        bare,
        "B",
        approval,
        "probeReady",
    ])?;
    let armed = rpc.event("breaker.state_changed")?;
    assert_eq!(armed["payload"]["probe_status"], "READY");
    assert_eq!(armed["payload"]["probe_required"], 1);
    assert!(
        run(&[
            "breaker-probes-run",
            project,
            bare,
            "B",
            approval,
            "/nonexistent/lean"
        ])
        .is_err()
    );
    let final_pack = rpc.call("briefing.peek", json!({}))?;
    let breaker = &final_pack["result"]["fact_pack"]["breakers"]["items"][0];
    assert_eq!(breaker["state"], "HALF_OPEN");
    assert_eq!(breaker["probe_status"], "NEEDS_PROBE");
    assert_eq!(breaker["probe_passed"], 0);
    for unavailable in ["pivot.apply", "breaker.probes.run", "tasks.claim"] {
        assert!(rpc.call(unavailable, json!({}))?["error"].is_object());
    }
    Ok(())
}

#[test]
fn operator_oracle_and_hammer_are_not_available_over_daemon_rpc() -> Result<()> {
    let dir = tempfile::Builder::new()
        .prefix("oa")
        .rand_bytes(4)
        .tempdir()?;
    let mut server = boot(dir.path())?;
    let mut rpc = wait_connection(&dir.path().join(".runtime/ai4mathd.sock"), &mut server)?;
    for method in [
        "oracle.probe",
        "oracle.preview",
        "oracle.retain",
        "oracle.result",
        "oracle.hold",
        "oracle.release",
        "oracle.hold_status",
        "hammer.reconstruct",
        "hammer.promote",
        "tasks.runModel",
        "tasks.finishModel",
        "t2.build",
        "t2.audit",
        "t2.checkExport",
        "t2.checkManaged",
        "t2.checkBatch",
        "t2.releaseCheck",
        "t2.releaseTag",
        "t2.releaseInspect",
        "t2.heartbeats",
        "t2.compare",
        "t2.compareHeartbeats",
        "t2.compareManagedHeartbeats",
        "t2.compareReleaseHeartbeats",
        "t2.trustDebt",
        "t3.discover",
        "t3.discoverClaim",
        "claim.replay",
        "plan.replay",
        "graph.replay",
        "graph.audit",
        "graph.check",
        "t3.worklist",
        "t3.recheckOne",
        "t3.recheckBatch",
        "t3.sweepStatus",
        "t3.sweepStep",
        "wiki.sedimentPreview",
        "wiki.sedimentWakePreview",
        "wiki.sedimentWakeRecheck",
        "wiki.sedimentWakeRetain",
        "wiki.sedimentWakeResult",
        "wiki.sedimentAdaptPreview",
        "wiki.sedimentAdaptApprove",
        "wiki.sedimentAdaptResult",
        "wiki.sedimentAdaptRun",
        "wiki.sedimentAdaptLineageRetain",
        "wiki.sedimentAdaptLineageResult",
        "wiki.coreRebuild",
        "wiki.coreSearch",
        "wiki.coreProvenance",
        "wiki.sedimentArchive",
    ] {
        assert_eq!(rpc.call(method, json!({}))?["error"]["code"], -32601);
    }
    assert_eq!(rpc.call("status", json!({}))?["result"]["event_seq"], 0);
    Ok(())
}

#[cfg(target_os = "macos")]
#[test]
fn project_heavy_lease_makes_daemon_probe_unavailable_without_blocking_control() -> Result<()> {
    use ai4math_broker::ProjectNativeGate;
    let dir = tempfile::Builder::new()
        .prefix("ng")
        .rand_bytes(4)
        .tempdir()?;
    let root = dir.path();
    let runtime = root.join(".runtime");
    fs::create_dir(&runtime)?;
    // Only the Mach-O catalogue metadata is needed: busy must short-circuit
    // the check, and after release lexical rejection occurs before a compiler.
    fs::create_dir_all(root.join("tool/bin"))?;
    fs::create_dir_all(root.join("tool/lib/lean"))?;
    let exe = root.join("tool/bin/lean");
    fs::copy("/usr/bin/true", &exe)?;
    let catalog = root.join("targets.json");
    fs::write(
        &catalog,
        serde_json::to_vec(&json!({"version":1,"targets":[{
            "id":"slot", "executable":exe,
            "frozen":{"engine":"lean4","target_id":"target","file_path":"Goal.lean",
                      "prefix":"theorem target : True := ","suffix":"\n"}
        }]}))?,
    )?;
    let gate = ProjectNativeGate::new(root);
    let heavy = gate.try_heavy()?;
    let mut server = Server(
        Command::new(env!("CARGO_BIN_EXE_ai4mathd"))
            .arg("--project")
            .arg(root)
            .arg("--tick-ms")
            .arg("150")
            .arg("--proof-targets")
            .arg(&catalog)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .spawn()?,
    );
    let mut rpc = wait_connection(&runtime.join("ai4mathd.sock"), &mut server)?;
    let payload = json!({"target_id":"slot","proof_body":"by sorry"});
    let busy = rpc.call("proof.submit", payload.clone())?;
    assert_eq!(busy["result"]["check"], "unavailable", "{busy}");
    assert_eq!(busy["result"]["promotion_eligible"], false);
    let control = rpc.call("status", json!({}))?;
    assert_eq!(control["result"]["event_seq"], 0, "{control}");
    drop(heavy);
    let evaluated = rpc.call("proof.submit", payload)?;
    assert_eq!(evaluated["result"]["check"], "rejected", "{evaluated}");
    assert_eq!(evaluated["result"]["promotion_eligible"], false);
    assert_eq!(rpc.call("status", json!({}))?["result"]["event_seq"], 0);
    Ok(())
}

#[test]
fn refuses_runtime_directory_symlink() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let destination = dir.path().join("outside");
    fs::create_dir(&destination)?;
    std::os::unix::fs::symlink(&destination, dir.path().join(".runtime"))?;
    let mut child = boot(dir.path())?;
    assert!(!child.0.wait()?.success());
    assert_eq!(fs::read_dir(destination)?.count(), 0);
    Ok(())
}

#[test]
#[cfg(target_os = "macos")]
#[ignore = "requires native host without the outer Seatbelt and AI4MATH_TEST_LEAN"]
fn native_daemon_checks_only_operator_frozen_slots() -> Result<()> {
    let executable = std::env::var("AI4MATH_TEST_LEAN")?;
    let dir = tempfile::Builder::new()
        .prefix("a4")
        .rand_bytes(4)
        .tempdir()?;
    let path = dir.path().join("operator.json");
    fs::write(
        &path,
        serde_json::to_vec(
            &json!({"version":1,"targets":[{"id":"slot-A","frozen":{"engine":"lean4","target_id":"target","file_path":"Proof.lean","prefix":"theorem target : True := ","suffix":"\n"},"executable":executable}]}),
        )?,
    )?;
    let mut child = Server(
        Command::new(env!("CARGO_BIN_EXE_ai4mathd"))
            .arg("--project")
            .arg(dir.path())
            .arg("--proof-targets")
            .arg(path)
            .spawn()?,
    );
    let mut rpc = wait_connection(&dir.path().join(".runtime/ai4mathd.sock"), &mut child)?;
    assert_eq!(
        rpc.call(
            "proof.submit",
            json!({"target_id":"slot-A","proof_body":"True.intro","toolchain":"unknown"})
        )?["error"]["code"],
        -32602
    );
    assert_eq!(
        rpc.call(
            "proof.submit",
            json!({"target_id":"slot-B","proof_body":"True.intro"})
        )?["error"]["code"],
        -32004
    );
    let valid = rpc.call(
        "proof.submit",
        json!({"target_id":"slot-A","proof_body":"True.intro"}),
    )?;
    assert_eq!(
        valid["result"],
        json!({"target_id":"slot-A","check":"checked","promotion_eligible":false})
    );
    let invalid = rpc.call(
        "proof.submit",
        json!({"target_id":"slot-A","proof_body":"False.elim"}),
    )?;
    assert_eq!(invalid["result"]["check"], "rejected");
    let status = rpc.call("status", json!({}))?;
    assert_eq!(status["result"]["event_seq"], 0);
    assert_eq!(status["result"]["broker"]["fast_busy"], 0);
    assert!(
        status["result"]["broker"]["fast_capacity"]
            .as_u64()
            .unwrap()
            > 0
    );
    Ok(())
}

#[test]
#[cfg(target_os = "macos")]
#[ignore = "requires native Seatbelt and AI4MATH_TEST_ROCQ / AI4MATH_TEST_ROCQ_DATA"]
fn native_daemon_rocq_slot_checks_without_promotion_authority() -> Result<()> {
    let executable = std::env::var("AI4MATH_TEST_ROCQ")?;
    let data = std::env::var("AI4MATH_TEST_ROCQ_DATA")?;
    let runtime_reads: Vec<_> = std::env::var_os("AI4MATH_TEST_ROCQ_READS")
        .map(|s| std::env::split_paths(&s).collect())
        .unwrap_or_default();
    let dir = tempfile::Builder::new()
        .prefix("aq")
        .rand_bytes(4)
        .tempdir()?;
    let path = dir.path().join("operator.json");
    fs::write(
        &path,
        serde_json::to_vec(&json!({"version":1,"targets":[{
            "id":"rocq-frozen",
            "frozen":{"engine":"rocq","target_id":"target","file_path":"Goal.v",
                "prefix":"Theorem target : True.\nProof.\n  exact (","suffix":").\nQed.\n"},
            "executable":executable,"rocq_data":data,"runtime_reads":runtime_reads
        }]}))?,
    )?;
    let mut child = Server(
        Command::new(env!("CARGO_BIN_EXE_ai4mathd"))
            .arg("--project")
            .arg(dir.path())
            .arg("--proof-targets")
            .arg(path)
            .spawn()?,
    );
    let mut rpc = wait_connection(&dir.path().join(".runtime/ai4mathd.sock"), &mut child)?;
    assert_eq!(
        rpc.call(
            "proof.submit",
            json!({"target_id":"rocq-frozen","proof_body":"I","rocq_data":"/tmp/not-operator-data"})
        )?["error"]["code"],
        -32602
    );
    let valid = rpc.call(
        "proof.submit",
        json!({"target_id":"rocq-frozen","proof_body":"I"}),
    )?;
    assert_eq!(
        valid["result"],
        json!({"target_id":"rocq-frozen","check":"checked","promotion_eligible":false})
    );
    let invalid = rpc.call(
        "proof.submit",
        json!({"target_id":"rocq-frozen","proof_body":"I). Admitted. (*"}),
    )?;
    assert_eq!(invalid["result"]["check"], "rejected");
    assert_eq!(rpc.call("status", json!({}))?["result"]["event_seq"], 0);
    Ok(())
}
