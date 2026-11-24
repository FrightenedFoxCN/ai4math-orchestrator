#![cfg(unix)]

use ai4math_store::StateStore;
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    env,
    fs::{self, OpenOptions},
    io::{BufRead, BufReader, ErrorKind, Write},
    os::unix::{
        fs::{DirBuilderExt, FileTypeExt, OpenOptionsExt, PermissionsExt},
        net::UnixStream,
    },
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

const SOCKET: &str = ".runtime/ai4mathd.sock";
const MAIN: &str = "refs/heads/main";
const ANCHOR: &str = "Proof.lean";
const WAIT: Duration = Duration::from_secs(8);
const INTERVAL_MS: u64 = 100;

fn short_tempdir() -> Result<tempfile::TempDir> {
    // Keep the canonical endpoint below macOS's Unix socket path limit.
    Ok(tempfile::Builder::new()
        .prefix("a4")
        .rand_bytes(4)
        .tempdir()?)
}

fn write_config(path: &Path, mode: u32, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)?;
    // Test the exact requested mode regardless of the caller's umask.
    file.set_permissions(fs::Permissions::from_mode(mode))?;
    file.write_all(bytes)?;
    Ok(())
}

struct Daemon {
    child: Child,
    log: tempfile::NamedTempFile,
}

impl Daemon {
    fn spawn(root: &Path, config: Option<&Path>) -> Result<Self> {
        let log = tempfile::NamedTempFile::new_in(root)?;
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_ai4mathd"));
        cmd.arg("--project").arg(root);
        if let Some(path) = config {
            cmd.arg("--t3-discovery").arg(path);
        }
        let child = cmd
            .args(["--tick-ms", "100"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log.reopen()?)
            .spawn()?;
        Ok(Self { child, log })
    }

    fn logs(&self) -> String {
        fs::read_to_string(self.log.path()).unwrap_or_default()
    }

    fn poll<T>(
        &mut self,
        what: &str,
        mut observe: impl FnMut(&mut Child) -> Result<Option<T>>,
    ) -> Result<T> {
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(value) = observe(&mut self.child)? {
                return Ok(value);
            }
            ensure!(
                Instant::now() < deadline,
                "waiting for {what}: {}",
                self.logs()
            );
            thread::sleep(Duration::from_millis(25));
        }
    }

    fn wait_status(&mut self, root: &Path, expected: &Value, seq: i64) -> Result<()> {
        let mut last = Value::Null;
        let result = self.poll(&format!("discovery status {expected}"), |child| {
            ensure!(
                child.try_wait()?.is_none(),
                "daemon exited before status was ready"
            );
            let Some(current) = status(root)? else {
                return Ok(None);
            };
            assert_eq!(current["event_seq"], seq);
            ensure!(
                current.get("t3_discovery").is_some(),
                "opt-in status missing discovery: {current}"
            );
            last = current["t3_discovery"].clone();
            Ok((&last == expected).then_some(()))
        });
        result.with_context(|| {
            format!(
                "last discovery status: {last}; daemon stderr: {}",
                self.logs()
            )
        })
    }

    fn stop_for_restart(mut self, root: &Path) -> Result<()> {
        self.child.kill()?;
        self.child.wait()?;
        // SIGKILL cannot run socket cleanup. Remove only the known socket in
        // this private fixture after reaping its daemon.
        let socket = root.join(SOCKET);
        let parent = socket.parent().context("fixture socket without parent")?;
        ensure!(
            parent.canonicalize()? == parent,
            "fixture runtime path changed"
        );
        ensure!(fs::symlink_metadata(&socket)?.file_type().is_socket());
        fs::remove_file(socket)?;
        Ok(())
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn exchange(mut stream: UnixStream, method: &str, params: Value) -> Result<Value> {
    stream.set_read_timeout(Some(WAIT))?;
    stream.set_write_timeout(Some(WAIT))?;
    writeln!(
        stream,
        "{}",
        json!({"jsonrpc":"2.0","id":1,"method":method,"params":params})
    )?;
    let mut line = String::new();
    ensure!(
        BufReader::new(stream).read_line(&mut line)? > 0,
        "daemon closed {method} RPC"
    );
    let reply: Value = serde_json::from_str(&line)?;
    assert_eq!(reply["jsonrpc"], "2.0");
    assert_eq!(reply["id"], 1);
    Ok(reply)
}

fn status(root: &Path) -> Result<Option<Value>> {
    let stream = match UnixStream::connect(root.join(SOCKET)) {
        Ok(stream) => stream,
        Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::ConnectionRefused) => {
            return Ok(None);
        }
        Err(e) => return Err(e.into()),
    };
    let reply = exchange(stream, "status", json!({}))?;
    ensure!(
        reply["result"]["event_seq"].is_i64(),
        "unexpected status reply: {reply}"
    );
    Ok(Some(reply["result"].clone()))
}

fn success(output: Output, description: &str) -> Result<String> {
    ensure!(
        output.status.success(),
        "{description} ({}): {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8(output.stdout)?)
}

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("/usr/bin/git")
        .current_dir(dir)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
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
        .stdin(Stdio::null())
        .output()?;
    Ok(success(output, &format!("git {args:?}"))?.trim().into())
}

fn utf8(path: &Path) -> &str {
    path.to_str().expect("UTF-8 fixture path")
}

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    bare: PathBuf,
    work: PathBuf,
    cli: PathBuf,
    config: PathBuf,
    base: String,
    ancestor: String,
}

impl Fixture {
    fn new() -> Result<Self> {
        let temp = short_tempdir()?;
        let home = temp.path().canonicalize()?;
        let root = home.join("proj");
        let work = home.join("work");
        let bare = home.join("repo.git");
        let cli = home.join("ai4math");
        let binary = PathBuf::from(
            env::var_os("AI4MATH_TEST_CLI")
                .context("set AI4MATH_TEST_CLI to the built ai4math binary")?,
        )
        .canonicalize()?;
        // Cargo may hard-link its outputs. Pin a private ordinary copy of the
        // exact binary under this fixture, not a script or a PATH lookup.
        fs::copy(binary, &cli)?;
        fs::set_permissions(&cli, fs::Permissions::from_mode(0o700))?;
        fs::create_dir(&work)?;
        git(
            &work,
            &["init", "--quiet", "--initial-branch=main", "--template="],
        )?;
        fs::write(work.join("Defs.lean"), "def x : Nat := 2\n")?;
        fs::write(work.join("Other.lean"), "def y : Nat := 0\n")?;
        fs::write(
            work.join(ANCHOR),
            "import Defs\nimport Other\ntheorem target : x = 2 := {{PROOF_BODY}}\n",
        )?;
        fs::write(
            work.join("ai4math-modules.txt"),
            "Defs.lean\nOther.lean\nProof.lean\n",
        )?;
        git(&work, &["add", "."])?;
        git(&work, &["commit", "--quiet", "-m", "Seed"])?;
        let ancestor = git(&work, &["rev-parse", "HEAD"])?;
        git(
            &work,
            &["commit", "--quiet", "--allow-empty", "-m", "Pin baseline"],
        )?;
        let base = git(&work, &["rev-parse", "HEAD"])?;
        git(&home, &["clone", "--quiet", "--bare", "work", "repo.git"])?;
        let fixture = Self {
            _temp: temp,
            root,
            bare: bare.canonicalize()?,
            work,
            cli: cli.canonicalize()?,
            config: home.join("discovery.json"),
            base,
            ancestor,
        };
        fixture.run("init", &["lean4"])?;
        for (id, file, target) in [
            ("Definition", "Defs.lean", "x"),
            ("Proof", ANCHOR, "target"),
        ] {
            fixture.run(
                "declare-claim",
                &[id, file, target, id, "Fixture statement"],
            )?;
        }
        fixture.run("task-bind", &[utf8(&fixture.bare), ANCHOR, &fixture.base])?;
        write_config(
            &fixture.config,
            0o600,
            &serde_json::to_vec(&fixture.catalog())?,
        )?;
        Ok(fixture)
    }

    fn catalog(&self) -> Value {
        json!({
            "version": 1,
            "cli_executable": self.cli,
            "bare_repo": self.bare,
            "anchor_module": ANCHOR,
            "baseline_commit": self.base,
            "interval_ms": INTERVAL_MS,
            "probe_timeout_ms": 5_000
        })
    }

    fn cli_output(&self, command: &str, args: &[&str]) -> Result<Output> {
        Ok(Command::new(&self.cli)
            .arg(command)
            .arg(&self.root)
            .args(args)
            .stdin(Stdio::null())
            .output()?)
    }

    fn run(&self, command: &str, args: &[&str]) -> Result<String> {
        success(self.cli_output(command, args)?, command)
    }

    fn state(&self) -> Result<StateStore> {
        StateStore::open_readonly(self.root.join(".runtime/state.sqlite"))
    }

    fn claims(&self) -> Result<Value> {
        let state = self.state()?;
        let mut claims = Vec::new();
        for id in ["Definition", "Proof"] {
            let claim = state
                .claim(id)?
                .with_context(|| format!("missing fixture claim {id}"))?;
            assert!(!claim.stale && !claim.effective_stale);
            assert!(claim.taint_root_commit.is_none());
            claims.push(claim);
        }
        Ok(serde_json::to_value(claims)?)
    }

    fn push_change(&self) -> Result<String> {
        fs::write(self.work.join("Defs.lean"), "def x : Nat := 3\n")?;
        // These imports are independent; their dependency order is still valid.
        fs::write(
            self.work.join("ai4math-modules.txt"),
            "Other.lean\nDefs.lean\nProof.lean\n",
        )?;
        git(&self.work, &["add", "Defs.lean", "ai4math-modules.txt"])?;
        git(
            &self.work,
            &[
                "commit",
                "--quiet",
                "-m",
                "Source and manifest order change",
            ],
        )?;
        git(
            &self.work,
            &["push", "--quiet", utf8(&self.bare), "HEAD:refs/heads/main"],
        )?;
        git(&self.bare, &["rev-parse", MAIN])
    }

    fn move_main(&self, new: &str, old: &str) -> Result<()> {
        git(&self.bare, &["update-ref", MAIN, new, old])?;
        Ok(())
    }

    fn summary(&self, status: &str, head: &str, seq: i64, hash: Option<&str>) -> Value {
        json!({
            "status": status,
            "base_commit": self.base,
            "observed_head": head,
            "ledger_event_seq": seq,
            "module_order_changed": hash.is_some(),
            "changed_module_count": if hash.is_some() { 1 } else { 0 },
            "plan_sha256": hash,
            "advisory_only": true,
        })
    }
}

fn assert_invalid(root: &Path, config: &Path) -> Result<()> {
    let mut daemon = Daemon::spawn(root, Some(config))?;
    let status = daemon.poll("startup rejection", |child| Ok(child.try_wait()?))?;
    assert!(!status.success());
    let logs = daemon.logs();
    assert_eq!(
        logs.trim(),
        "ai4mathd: invalid --t3-discovery configuration"
    );
    assert!(
        !root.join(SOCKET).exists(),
        "published socket for invalid configuration"
    );
    Ok(())
}

#[test]
fn malformed_discovery_configuration_does_not_publish_a_socket() -> Result<()> {
    let temp = short_tempdir()?;
    let root = temp.path().canonicalize()?;
    let config = root.join("discovery.json");
    write_config(&config, 0o600, b"{not JSON")?;
    assert_invalid(&root, &config)
}

fn claim_catalog(cli: &Path, bare: &Path, claim_id: &str) -> Value {
    json!({
        "version": 2,
        "cli_executable": cli,
        "bare_repo": bare,
        "claim_id": claim_id,
        "interval_ms": INTERVAL_MS,
        "probe_timeout_ms": 5_000,
    })
}

fn publish_frame(root: &Path, frame: &Value) -> Result<()> {
    // Replace the file atomically so an active probe never reads a partial frame.
    let mut next = tempfile::NamedTempFile::new_in(root)?;
    serde_json::to_writer(&mut next, frame)?;
    next.persist(root.join("worker-summary.json"))?;
    Ok(())
}

#[test]
fn resident_claim_discovery_enforces_config_and_worker_protocol() -> Result<()> {
    let temp = short_tempdir()?;
    let root = temp.path().canonicalize()?;
    let cli = root.join("fixture-cli");
    let repo = root.join("repo.git");
    fs::create_dir(&repo)?;
    // This private fake executable asserts dispatch shape without requiring Git,
    // the real CLI, or a verified proof. The real Claim lookup is tested separately.
    write_config(
        &cli,
        0o700,
        br#"#!/bin/sh
[ "$#" = 5 ] && [ "$1" = t3-discover-claim ] && [ "$2" = "$PWD" ] &&
    [ "$3" = "$PWD/repo.git" ] && [ "$4" = FixtureClaim ] && [ "$5" = --daemon-parent ] || exit 64
exec /bin/cat worker-summary.json
"#,
    )?;
    let catalog = claim_catalog(&cli, &repo, "FixtureClaim");
    let mut missing = catalog.clone();
    missing.as_object_mut().unwrap().remove("claim_id");
    let invalid = root.join("invalid.json");
    write_config(&invalid, 0o600, &serde_json::to_vec(&missing)?)?;
    assert_invalid(&root, &invalid).context("missing claim_id")?;
    for (field, bad) in [
        ("claim_id", Value::Null),
        ("claim_id", json!(42)),
        ("claim_id", json!("")),
        ("claim_id", json!(" FixtureClaim")),
        ("claim_id", json!("é".repeat(129))),
        ("claim_id", json!("Claim\nsecret")),
        ("anchor_module", json!(ANCHOR)),
        ("anchor_module", Value::Null),
        ("baseline_commit", json!("a".repeat(40))),
        ("baseline_commit", Value::Null),
    ] {
        let mut mixed = catalog.clone();
        mixed[field] = bad;
        let bytes = serde_json::to_vec(&mixed)?;
        fs::write(&invalid, bytes)?;
        assert_invalid(&root, &invalid).with_context(|| format!("invalid {field}: {mixed}"))?;
    }
    let mut mixed = catalog.clone();
    mixed["version"] = json!(1);
    mixed["anchor_module"] = json!(ANCHOR);
    mixed["baseline_commit"] = json!("a".repeat(40));
    fs::write(&invalid, serde_json::to_vec(&mixed)?)?;
    assert_invalid(&root, &invalid).context("v1 with claim_id")?;
    mixed["claim_id"] = Value::Null;
    fs::write(&invalid, serde_json::to_vec(&mixed)?)?;
    assert_invalid(&root, &invalid).context("v1 with explicit null claim_id")?;

    let runtime = root.join(".runtime");
    fs::DirBuilder::new().mode(0o700).create(&runtime)?;
    write_config(&runtime.join("primary_engine"), 0o600, b"lean4\n")?;
    let store_path = runtime.join("state.sqlite");
    let seq = StateStore::open(&store_path)?.status()?.last_event_seq;
    let config = root.join("discovery.json");
    write_config(&config, 0o600, &serde_json::to_vec(&catalog)?)?;
    let summary = json!({
        "status": "structural_change",
        "base_commit": "a".repeat(40),
        "observed_head": "b".repeat(40),
        "ledger_event_seq": seq,
        "module_order_changed": false,
        "changed_module_count": 1,
        "plan_sha256": "c".repeat(64),
        "advisory_only": true,
        "baseline_scope": "claim_t1",
    });
    publish_frame(&root, &summary)?;
    let mut daemon = Daemon::spawn(&root, Some(&config))?;
    daemon.wait_status(&root, &summary, seq)?;

    let unavailable = json!({"status": "unavailable"});
    for (field, bad) in [
        ("baseline_scope", None),
        ("baseline_scope", Some(Value::Null)),
        ("baseline_scope", Some(json!("operator"))),
        ("baseline_claim", Some(json!("FixtureClaim"))),
        ("baseline_receipt_sha256", Some(json!("d".repeat(64)))),
        ("bare_repo", Some(json!(repo))),
    ] {
        let mut frame = summary.clone();
        if let Some(bad) = bad {
            frame[field] = bad;
        } else {
            frame.as_object_mut().unwrap().remove(field);
        }
        publish_frame(&root, &frame)?;
        daemon
            .wait_status(&root, &unavailable, seq)
            .with_context(|| format!("expected rejection of frame with invalid {field}"))?;
        // Require recovery between cases, so none can pass on a previous failure.
        publish_frame(&root, &summary)?;
        daemon.wait_status(&root, &summary, seq)?;
    }
    assert!(
        StateStore::open_readonly(store_path)?
            .events_after(seq, 1)?
            .is_empty()
    );
    Ok(())
}

#[test]
#[ignore = "requires AI4MATH_TEST_CLI pointing to a built ai4math binary; no native proof engine required"]
fn resident_claim_discovery_has_no_fallback_for_unknown_or_unverified_claims() -> Result<()> {
    let f = Fixture::new()?;
    let head = f.push_change()?;
    let refs = git(&f.bare, &["show-ref"])?;
    let seq = f.state()?.status()?.last_event_seq;
    let original_claims = f.claims()?;

    // The same repository has a real structural delta against its operator's
    // baseline. Claim mode must not fall back to this otherwise valid plan.
    let plan: Value =
        serde_json::from_str(&f.run("t3-preview", &[utf8(&f.bare), ANCHOR, &f.base, &head])?)?;
    assert_eq!(plan["changed_modules"], json!(["Defs.lean"]));
    let hash = plan["plan_sha256"]
        .as_str()
        .context("missing plan digest")?;
    let unavailable = json!({"status": "unavailable"});
    for claim in ["UnknownClaim", "Proof"] {
        let args = [utf8(&f.bare), claim];
        let direct = f.cli_output("t3-discover-claim", &args)?;
        assert!(!direct.status.success() && direct.stdout.is_empty());
        let stderr = String::from_utf8_lossy(&direct.stderr);
        assert!(
            stderr.contains("no current exact managed Lean T1 baseline"),
            "{stderr}"
        );

        let config = f.config.with_file_name(format!("claim-{claim}.json"));
        let catalog = claim_catalog(&f.cli, &f.bare, claim);
        write_config(&config, 0o600, &serde_json::to_vec(&catalog)?)?;
        let mut daemon = Daemon::spawn(&f.root, Some(&config))?;
        daemon.wait_status(&f.root, &unavailable, seq)?;
        assert!(
            !daemon
                .logs()
                .contains("no current exact managed Lean T1 baseline")
        );
        daemon.stop_for_restart(&f.root)?;
    }

    let state = f.state()?;
    assert_eq!(state.status()?.last_event_seq, seq);
    assert!(state.events_after(seq, 1)?.is_empty());
    assert!(state.claim("UnknownClaim")?.is_none());
    assert_eq!(f.claims()?, original_claims);
    assert!(state.applied_t3_plan(hash)?.is_none());
    assert_eq!(git(&f.bare, &["show-ref"])?, refs);
    Ok(())
}

#[test]
#[ignore = "requires AI4MATH_TEST_CLI pointing to a built ai4math binary; no native proof engine required"]
fn resident_discovery_is_opt_in_advisory_and_tracks_managed_main() -> Result<()> {
    let f = Fixture::new()?;
    let seq = f.state()?.status()?.last_event_seq;
    let original_claims = f.claims()?;

    let mut catalog = f.catalog();
    for (name, body, mode) in [
        ("shared", catalog.clone(), 0o644),
        (
            "over-timeout",
            {
                catalog["probe_timeout_ms"] = json!(60_001);
                catalog
            },
            0o600,
        ),
    ] {
        let invalid = f.config.with_file_name(format!("{name}.json"));
        write_config(&invalid, mode, &serde_json::to_vec(&body)?)?;
        assert_invalid(&f.root, &invalid)?;
    }

    let mut disabled = Daemon::spawn(&f.root, None)?;
    disabled.poll("unconfigured daemon to be ready", |child| {
        ensure!(child.try_wait()?.is_none(), "unconfigured daemon exited");
        status(&f.root)
    })?;
    let head = f.push_change()?;
    let until = Instant::now() + Duration::from_millis(3 * INTERVAL_MS);
    loop {
        let current = status(&f.root)?.context("unconfigured daemon stopped")?;
        assert_eq!(current["event_seq"], seq);
        assert!(
            current.get("t3_discovery").is_none(),
            "discovery without opt-in: {current}"
        );
        if Instant::now() >= until {
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }
    disabled.stop_for_restart(&f.root)?;

    // The config and bound repo are valid, but no probe can report a summary
    // until main returns. This makes the transient status test deterministic.
    git(&f.bare, &["update-ref", "-d", MAIN, &head])?;
    let mut enabled = Daemon::spawn(&f.root, Some(&f.config))?;
    enabled.wait_status(&f.root, &json!({"status":"unavailable"}), seq)?;
    git(&f.bare, &["update-ref", MAIN, &f.base])?;
    enabled.wait_status(&f.root, &f.summary("unchanged", &f.base, seq, None), seq)?;

    f.move_main(&head, &f.base)?;
    let preview: Value =
        serde_json::from_str(&f.run("t3-preview", &[utf8(&f.bare), ANCHOR, &f.base, &head])?)?;
    assert_eq!(preview["changed_modules"], json!(["Defs.lean"]));
    let hash = preview["plan_sha256"]
        .as_str()
        .context("preview has no plan digest")?;
    assert_eq!(hash.len(), 64);
    enabled.wait_status(
        &f.root,
        &f.summary("structural_change", &head, seq, Some(hash)),
        seq,
    )?;

    for method in ["t3.discover", "t3.apply"] {
        let reply = exchange(UnixStream::connect(f.root.join(SOCKET))?, method, json!({}))?;
        assert_eq!(reply["error"]["code"], -32601, "{method}: {reply}");
    }
    let direct = f.cli_output(
        "t3-discover",
        &[utf8(&f.bare), ANCHOR, &f.base, "--daemon-parent"],
    )?;
    assert!(!direct.status.success() && direct.stdout.is_empty());
    assert!(String::from_utf8_lossy(&direct.stderr).contains("daemon T3 requires"));
    assert_eq!(f.claims()?, original_claims);
    assert!(f.state()?.applied_t3_plan(hash)?.is_none());
    assert_eq!(git(&f.bare, &["rev-parse", MAIN])?, head);

    // Earlier main has identical source bytes to the baseline, but the baseline
    // is not its ancestor. The resident must discard the old advisory digest.
    f.move_main(&f.ancestor, &head)?;
    enabled.wait_status(&f.root, &f.summary("diverged", &f.ancestor, seq, None), seq)?;
    f.move_main(&f.base, &f.ancestor)?;
    enabled.wait_status(&f.root, &f.summary("unchanged", &f.base, seq, None), seq)?;

    // Losing the operator-owned coordination file may not be repaired by a watcher.
    let lock = f.bare.join("ai4math-promotion.lock");
    fs::remove_file(&lock)?;
    enabled.wait_status(&f.root, &json!({"status":"unavailable"}), seq)?;
    assert!(!lock.exists());

    let state = f.state()?;
    assert_eq!(state.status()?.last_event_seq, seq);
    assert!(state.events_after(seq, 1)?.is_empty());
    assert!(state.applied_t3_plan(hash)?.is_none());
    assert_eq!(f.claims()?, original_claims);
    Ok(())
}
