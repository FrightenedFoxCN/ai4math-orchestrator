#![cfg(unix)]

use ai4math_store::{
    Change, SedimentInput, StateStore, Wiki, WikiManagedCoreEntry, WikiManagedGeneration,
    WikiQuery, render_sediment,
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::Path,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const MAIN: &str = "refs/heads/main";
const ORIGIN: &str = "CLM-wiki-origin";
const TARGET: &str = "CLM-wiki-destination";
const STATIC: &str = r"---
id: COR-static
stratum: core
trust_level: L5
engine: lean4
status: active
title: Orbitfixture ordinary
informal_gloss: Static Wiki note
---
";
const GRAVE: &str = r"---
id: GRV-static
stratum: graveyard
trust_level: L2b
engine: lean4
status: refuted
title: Orbitfixture refutation
edges:
  - {dst_id: COR-static, edge_type: refutes}
---
";

struct Daemon(Child);
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Rpc {
    stream: BufReader<UnixStream>,
    next_id: u32,
}
impl Rpc {
    fn connect(project: &Path, daemon: &mut Daemon) -> Result<Self> {
        let socket = project.join(".runtime/ai4mathd.sock");
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(stream) = UnixStream::connect(&socket) {
                stream.set_read_timeout(Some(Duration::from_secs(5)))?;
                stream.set_write_timeout(Some(Duration::from_secs(5)))?;
                return Ok(Self {
                    stream: BufReader::new(stream),
                    next_id: 0,
                });
            }
            if let Some(status) = daemon.0.try_wait()? {
                anyhow::bail!("ai4mathd exited before accepting connections: {status}");
            }
            ensure!(Instant::now() < deadline, "ai4mathd socket not ready");
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        self.next_id += 1;
        writeln!(
            self.stream.get_mut(),
            "{}",
            json!({"jsonrpc":"2.0","id":self.next_id,"method":method,"params":params})
        )?;
        let mut line = String::new();
        ensure!(self.stream.read_line(&mut line)? > 0, "EOF during {method}");
        let response: Value = serde_json::from_str(&line)?;
        assert_eq!(response["id"], self.next_id, "{method}: {response}");
        Ok(response)
    }

    fn result(&mut self, method: &str, params: Value) -> Result<Value> {
        let mut response = self.call(method, params)?;
        ensure!(response.get("result").is_some(), "{method}: {response}");
        Ok(response["result"].take())
    }

    fn core(&mut self) -> Result<Value> {
        self.result(
            "wiki.search",
            json!({"text":"Orbitfixture","include_shadow_hint":false}),
        )
    }

    fn provenance(&mut self, id: &str) -> Result<Value> {
        self.result("wiki.provenance", json!({"id":id,"max_depth":1}))
    }

    fn graveyard(&mut self) -> Result<Value> {
        self.result(
            "wiki.check_graveyard",
            json!({"informal_statement":"Orbitfixture"}),
        )
    }
}

fn git(repo: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("/usr/bin/git")
        .env_clear()
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Wiki RPC Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@localhost")
        .env("GIT_COMMITTER_NAME", "Wiki RPC Fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@localhost")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(Stdio::null())
        .output()?;
    ensure!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8(output.stdout)?.trim_end().into())
}

fn ledger_snapshot(state: &StateStore) -> Result<Value> {
    assert!(state.promotion_intent()?.is_none());
    assert!(state.pivot_intent()?.is_none());
    Ok(json!({
        "repo": state.managed_repo()?,
        "status": state.status()?,
        "events": state.events_after(0, 32)?,
    }))
}

#[test]
fn wiki_rpc_exposes_only_the_current_managed_core_generation() -> Result<()> {
    // Keep the canonical project/socket path below the macOS sun_path limit.
    let temp = tempfile::Builder::new()
        .prefix("a4")
        .rand_bytes(4)
        .tempdir()?;
    let root = temp.path().canonicalize()?;
    let repo = root.join("managed.git");
    let project = root.join("project");
    fs::create_dir(&repo)?;
    git(&repo, &["init", "--bare", "--initial-branch=main"])?;
    let tree = git(&repo, &["mktree"])?;
    let reviewed = git(&repo, &["commit-tree", &tree, "-m", "reviewed generation"])?;
    let drift = git(
        &repo,
        &[
            "commit-tree",
            &tree,
            "-p",
            &reviewed,
            "-m",
            "later generation",
        ],
    )?;
    git(&repo, &["update-ref", MAIN, &reviewed])?;
    git(&repo, &["pack-refs", "--all", "--prune"])?;
    assert!(
        !repo.join(MAIN).exists(),
        "fixture must start with packed main"
    );

    let runtime = project.join(".runtime");
    fs::create_dir_all(&runtime)?;
    let ledger = runtime.join("state.sqlite");
    let mut state = StateStore::open(&ledger)?;
    let repo_path = repo.to_str().context("non-UTF-8 Git fixture path")?;
    state.bind_task_repository(repo_path, &reviewed)?;
    let event_seq = state.status()?.last_event_seq;
    let generation = WikiManagedGeneration {
        head: &reviewed,
        event_seq,
    };
    let initial_ledger = ledger_snapshot(&state)?;

    let wiki = project.join(".mathwiki");
    for stratum in ["core", "sediment", "graveyard"] {
        fs::create_dir_all(wiki.join(stratum))?;
    }
    let static_file = wiki.join("core/static.md");
    let grave_file = wiki.join("graveyard/static.md");
    fs::write(&static_file, STATIC)?;
    fs::write(&grave_file, GRAVE)?;
    // This fixture seeds the disposable projection; proof verification is not an RPC.
    let source = "theorem Historical.orbit : True := True.intro\n";
    let archive = render_sediment(SedimentInput {
        claim_id: ORIGIN,
        engine: "lean4",
        verified_at_commit: &reviewed,
        source_module: "Historical.lean",
        target_decl: "Historical.orbit",
        source,
        source_sha256: &format!("{:x}", Sha256::digest(source)),
        dependency_hashes: &BTreeMap::new(),
        dependency_order: &[],
    })?;
    let archive_file = project.join(archive.relative_path());
    fs::write(&archive_file, archive.markdown())?;
    let managed = WikiManagedCoreEntry {
        id: format!("CORE-ADAPT-{}", "a".repeat(64)),
        archive_id: archive.id().into(),
        origin_claim: ORIGIN.into(),
        claim_id: TARGET.into(),
        title: "Orbitfixture managed".into(),
        informal_gloss: "Reviewed adaptation metadata".into(),
        verified_at_commit: reviewed.clone(),
    };
    let index_path = wiki.join(".index.sqlite");
    assert_eq!(
        Wiki::open(&index_path)?.rebuild_with_managed_core(
            &wiki,
            generation,
            std::slice::from_ref(&managed)
        )?,
        4
    );
    let offline_index = Wiki::open_readonly(&index_path)?;
    let query = WikiQuery {
        include_shadow_hint: false,
        ..WikiQuery::new("Orbitfixture")
    };
    let ordinary = offline_index.search(query)?;
    assert_eq!(ordinary.hits.len(), 1);
    assert_eq!(ordinary.hits[0].summary.id, "COR-static");
    let static_hit = serde_json::to_value(&ordinary.hits[0])?;
    let original_archive_edges =
        serde_json::to_value(offline_index.provenance(archive.id(), 1, 24)?.edges)?;

    // Every phase uses the same daemon and socket: freshness must not be cached at boot.
    let mut daemon = Daemon(
        Command::new(env!("CARGO_BIN_EXE_ai4mathd"))
            .arg("--project")
            .arg(&project)
            .args(["--tick-ms", "60000"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .spawn()?,
    );
    let mut rpc = Rpc::connect(&project, &mut daemon)?;
    let fresh = rpc.core()?;
    assert_eq!(fresh["managed_core"], true);
    assert_eq!(fresh["truncated"], false);
    let hits = fresh["hits"].as_array().context("wiki.search hits")?;
    assert_eq!(hits.len(), 2, "{fresh}");
    assert!(hits.contains(&static_hit), "{fresh}");
    let hit = hits
        .iter()
        .find(|hit| hit["id"] == managed.id)
        .context("missing managed hit")?;
    let virtual_path = format!(".managed-core/{}", managed.id);
    assert!(hit["score"].is_number());
    assert_eq!(
        *hit,
        json!({
            "id": managed.id, "title": managed.title, "engine":"lean4", "stratum":"core",
            "status":"active", "trust_level":"L5", "verified_at_commit":reviewed,
            "informal_gloss":managed.informal_gloss, "formal_signature":null,
            "file_path":virtual_path, "score":hit["score"],
        })
    );
    assert!(
        !wiki.join(virtual_path).exists(),
        "synthetic hit must not render a file"
    );

    let fresh_core_lineage = rpc.provenance(&managed.id)?;
    let born = json!({"src_id":managed.id,"dst_id":TARGET,"src_stratum":"core",
        "dst_stratum":null,"edge_type":"born_from_claim","metadata":{},"depth":1});
    let superseded = json!({"src_id":archive.id(),"dst_id":managed.id,"src_stratum":"sediment",
        "dst_stratum":"core","edge_type":"superseded_by","metadata":{},"depth":1});
    assert_eq!(
        fresh_core_lineage,
        json!({"edges":[born,superseded],"truncated":false,"managed_core":true})
    );
    assert_eq!(
        rpc.provenance(TARGET)?,
        json!({"edges":[born],"truncated":false,"managed_core":true})
    );
    let fresh_archive_lineage = rpc.provenance(archive.id())?;
    assert_eq!(fresh_archive_lineage["managed_core"], true);
    let archive_edges = fresh_archive_lineage["edges"]
        .as_array()
        .context("archive edges")?;
    assert_eq!(archive_edges.len(), 2);
    assert!(archive_edges.contains(&superseded));
    assert!(archive_edges.contains(&original_archive_edges[0]));

    let static_lineage = rpc.provenance("COR-static")?;
    assert_eq!(
        static_lineage["edges"],
        json!([{
            "src_id":"GRV-static","dst_id":"COR-static","src_stratum":"graveyard",
            "dst_stratum":"core","edge_type":"refutes","metadata":{},"depth":1,
        }])
    );
    let warnings = rpc.graveyard()?;
    let warning_hits = warnings["warnings"]
        .as_array()
        .context("graveyard warnings")?;
    assert_eq!(warning_hits.len(), 1);
    assert_eq!(warning_hits[0]["id"], "GRV-static");
    assert!(warnings.get("managed_core").is_none());
    let cold_query = json!({"text":"Orbitfixture","scope":"graveyard"});
    let cold = rpc.result("wiki.search", cold_query.clone())?;
    assert_eq!(cold["hits"], warnings["warnings"]);
    assert!(cold.get("managed_core").is_none());

    let assert_stale = |rpc: &mut Rpc, phase: &str| -> Result<()> {
        assert_eq!(
            rpc.core()?,
            json!({"hits":[static_hit],"truncated":false,"managed_core":false}),
            "{phase}"
        );
        for id in [&managed.id, TARGET] {
            assert_eq!(
                rpc.provenance(id)?,
                json!({"edges":[],"truncated":false,"managed_core":false}),
                "{phase}: {id}"
            );
        }
        assert_eq!(
            rpc.provenance(archive.id())?,
            json!({"edges": original_archive_edges, "truncated":false,"managed_core":false}),
            "{phase}"
        );
        let mut expected_static = static_lineage.clone();
        expected_static["managed_core"] = json!(false);
        assert_eq!(rpc.provenance("COR-static")?, expected_static, "{phase}");
        assert_eq!(rpc.graveyard()?, warnings, "{phase}");
        assert_eq!(
            rpc.result("wiki.search", cold_query.clone())?,
            cold,
            "{phase}"
        );
        Ok(())
    };

    // A loose ref must take precedence over the older copy in packed-refs.
    git(&repo, &["update-ref", MAIN, &drift, &reviewed])?;
    assert!(repo.join(MAIN).is_file());
    assert_stale(&mut rpc, "loose main drift")?;
    git(&repo, &["pack-refs", "--all", "--prune"])?;
    assert!(!repo.join(MAIN).exists());
    assert_stale(&mut rpc, "packed main drift")?;
    git(&repo, &["update-ref", MAIN, &reviewed, &drift])?;
    assert_eq!(rpc.core()?, fresh);
    assert_eq!(rpc.provenance(archive.id())?, fresh_archive_lineage);

    // Even when Git resolves the expected OID, the managed main must be a direct ref.
    git(&repo, &["update-ref", "refs/heads/alias", &reviewed])?;
    git(&repo, &["symbolic-ref", MAIN, "refs/heads/alias"])?;
    assert_eq!(git(&repo, &["rev-parse", "HEAD"])?, reviewed);
    assert_stale(&mut rpc, "symbolic main")?;
    git(&repo, &["update-ref", "--no-deref", MAIN, &reviewed])?;
    assert_eq!(rpc.core()?, fresh);
    assert_eq!(rpc.provenance(&managed.id)?, fresh_core_lineage);
    assert_eq!(
        ledger_snapshot(&state)?,
        initial_ledger,
        "successful and stale Wiki reads must not mutate the ledger"
    );

    // Now change only the ledger, while main stays exactly at the indexed commit.
    let advanced = state.append_event(
        Change {
            actor: "operator:wiki-fixture",
            git_commit: Some(&reviewed),
        },
        "WIKI_RPC_FIXTURE_DRIFT",
        "project",
        "managed",
        json!({}),
    )?;
    assert_eq!(advanced, event_seq + 1);
    assert_eq!(git(&repo, &["rev-parse", MAIN])?, reviewed);
    let advanced_ledger = ledger_snapshot(&state)?;
    assert_stale(&mut rpc, "ledger sequence drift")?;

    // Try each authority claim separately: a rejected unrelated field must not
    // mask acceptance of a head, repository, sequence, or managed_core flag.
    for (field, value) in [
        ("repo", json!(repo_path)),
        ("head", json!(reviewed)),
        ("reviewed_main", json!(reviewed)),
        ("event_seq", json!(event_seq)),
        ("generation", json!({"head":reviewed,"event_seq":event_seq})),
        ("managed_core", json!(true)),
    ] {
        for (method, mut params) in [
            ("wiki.search", json!({"text":"Orbitfixture"})),
            ("wiki.provenance", json!({"id":managed.id})),
        ] {
            params[field] = value.clone();
            assert_eq!(
                rpc.call(method, params)?["error"]["code"],
                -32602,
                "{method}: {field}"
            );
        }
    }
    for (method, params) in [
        ("wiki.rebuild", json!({"repo":repo_path,"head":reviewed})),
        ("wiki.rehydrate", json!({"id":archive.id()})),
        ("wiki.adapt", json!({"id":archive.id(),"claim_id":TARGET})),
    ] {
        assert_eq!(
            rpc.call(method, params)?["error"]["code"],
            -32601,
            "{method}"
        );
    }
    assert_stale(
        &mut rpc,
        "after rejected client authority and write requests",
    )?;
    assert_eq!(
        ledger_snapshot(&state)?,
        advanced_ledger,
        "only the fixture's operator event may advance the ledger"
    );
    assert_eq!(git(&repo, &["rev-parse", "HEAD"])?, reviewed);
    assert_eq!(fs::read(&static_file)?, STATIC.as_bytes());
    assert_eq!(fs::read(&grave_file)?, GRAVE.as_bytes());
    assert_eq!(fs::read(&archive_file)?, archive.markdown().as_bytes());
    let still_indexed = offline_index.search_at(query, generation)?;
    assert_eq!(
        serde_json::to_value(still_indexed.hits)?,
        fresh["hits"],
        "RPC must not rewrite the older projection"
    );
    Ok(())
}
