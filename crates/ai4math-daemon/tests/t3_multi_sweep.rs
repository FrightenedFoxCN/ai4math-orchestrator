#![cfg(unix)]

use ai4math_store::{Change, NewClaim, StateStore, T3PlanApproval, T3SweepState, T3SweepStatus};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OpenFlags};
use serde_json::{Value, json};
use std::{
    env,
    fs::{self, OpenOptions},
    io::{BufRead, BufReader, Write},
    os::{
        fd::AsRawFd,
        unix::{
            fs::{FileTypeExt, OpenOptionsExt, PermissionsExt},
            net::UnixStream,
        },
    },
    path::Path,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const MAIN: &str = "refs/heads/main";
const SOCKET: &str = ".runtime/ai4mathd.sock";
const DB: &str = ".runtime/state.sqlite";
const WAIT: Duration = Duration::from_secs(10);
const INTERVAL_MS: u64 = 100;
// Deliberately not sorted. The aliases stand for full digests in the catalog and ledger.
const CATALOG: [&str; 8] = ["b", "0", "e", "d", "a", "f", "c", "1"];
const CHILD_PLAN: &str = "AI4MATH_TEST_MULTI_SWEEP_PLAN";
const STOP_PREFIX: &str = "ai4mathd t3 autosweep stopped: ";

fn hash(alias: &str) -> String {
    alias.repeat(64)
}

fn short_tempdir() -> Result<tempfile::TempDir> {
    // A canonical macOS temp path is long; keep the Unix socket suffix short.
    Ok(tempfile::Builder::new()
        .prefix("a4")
        .rand_bytes(4)
        .tempdir()?)
}

fn write_private(path: &Path, mode: u32, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)?;
    file.set_permissions(fs::Permissions::from_mode(mode))?;
    file.write_all(bytes)?;
    Ok(())
}

fn git(repo: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("/usr/bin/git")
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .current_dir(repo)
        .args([
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@localhost",
            "-c",
            "commit.gpgSign=false",
        ])
        .args(args)
        .stdin(Stdio::null())
        .output()?;
    ensure!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(String::from_utf8(out.stdout)?.trim().to_owned())
}

struct Daemon {
    child: Child,
    log: tempfile::NamedTempFile,
}

impl Daemon {
    fn spawn(root: &Path, config: &Path) -> Result<Self> {
        let log = tempfile::NamedTempFile::new_in(root)?;
        let child = Command::new(env!("CARGO_BIN_EXE_ai4mathd"))
            .arg("--project")
            .arg(root)
            .arg("--t3-autosweep")
            .arg(config)
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
        mut observe: impl FnMut(&mut Self) -> Result<Option<T>>,
    ) -> Result<T> {
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(result) = observe(self)? {
                return Ok(result);
            }
            ensure!(
                self.child.try_wait()?.is_none(),
                "daemon exited while waiting for {what}: {}",
                self.logs()
            );
            ensure!(
                Instant::now() < deadline,
                "timed out waiting for {what}: {}",
                self.logs()
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn sweep_stopped(&mut self, expected: &str, root: &Path) -> Result<()> {
        self.poll("terminal autosweep log", |daemon| {
            let log = daemon.logs();
            if let Some(reason) = log.lines().find_map(|line| line.strip_prefix(STOP_PREFIX)) {
                ensure!(
                    reason == expected,
                    "expected {expected:?}, got {reason:?}\n{log}"
                );
                Ok(Some(()))
            } else {
                Ok(None)
            }
        })
        .with_context(|| fs::read_to_string(root.join("driver.log")).unwrap_or_default())
    }

    fn sweep_idle(&mut self, root: &Path, expected_calls: &[&str]) -> Result<()> {
        let until = Instant::now() + Duration::from_millis(4 * INTERVAL_MS);
        self.poll("resident autosweep to remain idle", |daemon| {
            assert_eq!(calls(root)?, expected_calls);
            let log = daemon.logs();
            assert!(
                !log.lines().any(|line| line.starts_with(STOP_PREFIX)),
                "{log}"
            );
            Ok((Instant::now() >= until).then_some(()))
        })
    }

    fn stop_for_restart(mut self, root: &Path) -> Result<()> {
        self.child.kill()?;
        self.child.wait()?;
        // Only unlink the known fixture socket after reaping its daemon (SIGKILL).
        let socket = root.join(SOCKET);
        let parent = socket.parent().context("fixture socket has no parent")?;
        ensure!(parent.canonicalize()? == parent);
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

fn worker_config(root: &Path, version: u8) -> Value {
    json!({
        "version": version,
        "cli_executable": root.join("fixture-cli"),
        "native_executable": root.join("unused-native"),
        "bare_repo": root.join("repo.git"),
        "page_limit": 1,
        "interval_ms": INTERVAL_MS,
        "step_timeout_ms": 10_000,
    })
}

fn catalog(root: &Path) -> Value {
    let mut config = worker_config(root, 2);
    config["plan_sha256s"] = json!(CATALOG.map(hash));
    config
}

fn follow_config(root: &Path) -> Value {
    let mut config = worker_config(root, 3);
    config["selection"] = json!("applied_current_main");
    config
}

fn assert_startup_rejected(root: &Path, label: &str, invalid: &Value) -> Result<()> {
    let config = root.join(label);
    write_private(&config, 0o600, &serde_json::to_vec(invalid)?)?;
    let mut daemon = Daemon::spawn(root, &config)?;
    let status = daemon.poll("startup rejection", |daemon| Ok(daemon.child.try_wait()?))?;
    assert!(!status.success(), "accepted {label}");
    assert!(
        daemon
            .logs()
            .contains("invalid --t3-autosweep configuration"),
        "{label}: {}",
        daemon.logs()
    );
    assert!(!root.join(SOCKET).exists(), "published socket for {label}");
    Ok(())
}

#[test]
fn rejects_invalid_v2_catalogs_before_publishing_socket() -> Result<()> {
    let temp = short_tempdir()?;
    let root = temp.path().canonicalize()?;
    for tool in ["fixture-cli", "unused-native"] {
        write_private(&root.join(tool), 0o700, b"#!/bin/sh\nexit 1\n")?;
    }
    fs::create_dir(root.join("repo.git"))?;
    let valid = catalog(&root);
    for (label, field, value) in [
        ("empty", "plan_sha256s", json!([])),
        ("duplicate", "plan_sha256s", json!([hash("a"), hash("a")])),
        (
            "oversize",
            "plan_sha256s",
            json!((0..9).map(|i| format!("{i:064x}")).collect::<Vec<_>>()),
        ),
        ("uppercase", "plan_sha256s", json!([hash("A")])),
        ("both", "plan_sha256", json!(hash("a"))),
        ("unknown", "restart", json!(true)),
        ("v1-with-array", "version", json!(1)),
    ] {
        let mut invalid = valid.clone();
        invalid[field] = value;
        assert_startup_rejected(&root, label, &invalid)?;
    }
    Ok(())
}

#[test]
fn rejects_missing_and_mixed_v3_selection_before_publishing_socket() -> Result<()> {
    let temp = short_tempdir()?;
    let root = temp.path().canonicalize()?;
    for tool in ["fixture-cli", "unused-native"] {
        write_private(&root.join(tool), 0o700, b"#!/bin/sh\nexit 1\n")?;
    }
    fs::create_dir(root.join("repo.git"))?;
    let valid = follow_config(&root);
    for (label, field, value) in [
        ("unknown-selection", "selection", json!("all_applied")),
        ("null-selection", "selection", Value::Null),
        ("fixed-hash", "plan_sha256", json!(hash("a"))),
        ("fixed-catalog", "plan_sha256s", json!([hash("a")])),
        ("null-hash", "plan_sha256", Value::Null),
        ("null-catalog", "plan_sha256s", Value::Null),
    ] {
        let mut invalid = valid.clone();
        invalid[field] = value;
        assert_startup_rejected(&root, label, &invalid)?;
    }
    assert_startup_rejected(&root, "missing-selection", &worker_config(&root, 3))?;
    let mut legacy = catalog(&root);
    legacy["selection"] = valid["selection"].clone();
    assert_startup_rejected(&root, "legacy-with-selection", &legacy)?;
    Ok(())
}

fn checkpoint(root: &Path, alias: &str) -> Result<T3SweepStatus> {
    StateStore::open_readonly(root.join(DB))?.t3_sweep_status(&hash(alias))
}

fn step(store: &mut StateStore, alias: &str) -> Result<T3SweepStatus> {
    let out = store.t3_sweep_step(&hash(alias), 1, false, "fixture", |_| {
        anyhow::bail!("fixture claim unexpectedly needed native proof")
    })?;
    Ok(out.progress)
}

fn install_worker(root: &Path) -> Result<()> {
    let helper = root.join("sweep-driver");
    fs::copy(env::current_exe()?, &helper)?;
    fs::set_permissions(&helper, fs::Permissions::from_mode(0o700))?;
    write_private(&root.join("fixture-cli"), 0o700, br#"#!/bin/sh
set -eu
[ "$#" -eq 7 ] && [ "$1" = t3-sweep-step ] && [ "$2" = "$PWD" ] && [ "$3" = "$2/repo.git" ] && [ "$5" = 1 ] && [ "$6" = "$2/unused-native" ] && [ "$7" = --daemon-parent ] || exit 97
export AI4MATH_TEST_MULTI_SWEEP_PLAN="$4"
exec "$2/sweep-driver" --exact fixture_sweep_child_driver --nocapture >> "$2/driver.log" 2>&1
"#)?;
    write_private(&root.join("unused-native"), 0o700, b"#!/bin/sh\nexit 98\n")
}

fn await_release(root: &Path, marker: &str) -> Result<()> {
    let deadline = Instant::now() + WAIT;
    while !root.join(marker).exists() {
        ensure!(
            Instant::now() < deadline,
            "fixture did not release {marker}"
        );
        thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

// The private fixture shell script checks the actual daemon argv, then re-enters this
// test executable to run genuine SQLite checkpoint transactions without a proof engine.
#[test]
fn fixture_sweep_child_driver() -> Result<()> {
    if env::current_exe()?
        .file_name()
        .and_then(|name| name.to_str())
        != Some("sweep-driver")
    {
        return Ok(());
    }
    let digest = env::var(CHILD_PLAN)?;
    let alias = CATALOG
        .into_iter()
        .find(|x| hash(x) == digest)
        .context("unknown fixture digest")?;
    let root = env::current_dir()?;
    let lock = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join("worker.lock"))?;
    // SAFETY: lock lives until this invocation exits, and this FD is valid.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        fs::write(root.join("worker-overlap"), b"")?;
        anyhow::bail!("overlapping worker process");
    }
    let rev = if alias == "0" {
        0
    } else {
        checkpoint(&root, alias)?.revision.unwrap_or(0)
    };
    let mut calls = OpenOptions::new()
        .append(true)
        .create(true)
        .open(root.join("calls"))?;
    calls.write_all(format!("{alias}:{rev}\n").as_bytes())?;
    if root.join("follow-mode").exists() {
        ensure!(alias != "f", "nonzero fixture exit");
        let progress = step(&mut StateStore::open(root.join(DB))?, alias)?;
        if alias == "b" && rev == 0 {
            ensure!(progress.status == T3SweepState::Running);
            // Let the test place a journal and advance main before the supervisor can
            // dispatch another old-head page or observe the new head.
            await_release(&root, "release-follow-page")?;
        }
        return Ok(());
    }
    match alias {
        "0" | "d" | "c" => anyhow::bail!("ineligible plan must never launch"),
        "e" => return Ok(()), // success without SQLite progress
        "f" => anyhow::bail!("nonzero fixture exit"),
        _ => (),
    }
    let first_run = !root.join("resuming").exists();
    if first_run && alias == "b" && rev == 0 {
        // Hold the first real page in flight until the test checks single child execution.
        await_release(&root, "release-first-page")?;
    }
    let mut store = StateStore::open(root.join(DB))?;
    let progress = step(&mut store, alias)?;
    if first_run && alias == "1" {
        // A successful +2 jump is not a valid single page, even though both writes are durable.
        step(&mut store, alias)?;
    }
    if first_run && alias == "b" && progress.revision == Some(3) {
        // Cut after the second round, while other plans have durable partial checkpoints.
        let foreign = fs::read_to_string(root.join("foreign-head"))?;
        git(
            &root.join("repo.git"),
            &["update-ref", MAIN, &foreign, &progress.reviewed_commit],
        )?;
    }
    Ok(())
}

fn calls(root: &Path) -> Result<Vec<String>> {
    match fs::read_to_string(root.join("calls")) {
        Ok(log) => Ok(log.lines().map(str::to_owned).collect()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(err) => Err(err.into()),
    }
}

fn assert_checkpoint(
    root: &Path,
    alias: &str,
    head: &str,
    seq: i64,
    revision: i64,
    cursor: Option<&str>,
) -> Result<()> {
    assert_eq!(
        checkpoint(root, alias)?,
        T3SweepStatus {
            plan_sha256: hash(alias),
            reviewed_commit: head.into(),
            status: if cursor.is_some() {
                T3SweepState::Running
            } else {
                T3SweepState::Complete
            },
            next_after_claim: cursor.map(str::to_owned),
            checkpoint_event_seq: Some(seq),
            revision: Some(revision),
        }
    );
    Ok(())
}

fn assert_event_neutral(root: &Path, seq: i64) -> Result<()> {
    let mut socket = UnixStream::connect(root.join(SOCKET))?;
    socket.set_read_timeout(Some(WAIT))?;
    socket.set_write_timeout(Some(WAIT))?;
    writeln!(
        socket,
        r#"{{"jsonrpc":"2.0","id":1,"method":"status","params":{{}}}}"#
    )?;
    let mut line = String::new();
    ensure!(
        BufReader::new(socket).read_line(&mut line)? > 0,
        "daemon closed status RPC"
    );
    let reply: Value = serde_json::from_str(&line)?;
    assert_eq!(reply["id"], 1);
    assert_eq!(reply["result"]["event_seq"], seq);
    let state = StateStore::open_readonly(root.join(DB))?;
    assert_eq!(state.status()?.last_event_seq, seq);
    assert!(state.events_after(seq, 1)?.is_empty());
    let db = Connection::open_with_flags(root.join(DB), OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let receipts: i64 = db.query_row("SELECT count(*) FROM verification_receipts", [], |r| {
        r.get(0)
    })?;
    assert_eq!(receipts, 0);
    Ok(())
}

fn init_repo(root: &Path) -> Result<std::path::PathBuf> {
    git(
        root,
        &[
            "init",
            "--bare",
            "--quiet",
            "--initial-branch=main",
            "--template=",
            "repo.git",
        ],
    )?;
    let repo = root.join("repo.git");
    write_private(&repo.join("ai4math-promotion.lock"), 0o600, b"")?;
    Ok(repo)
}

fn make_claims(store: &mut StateStore, base: &str, module: &str, ids: &[&str]) -> Result<()> {
    for id in ids {
        store.create_claim(
            NewClaim {
                id,
                title: id,
                informal_statement: "Fixture claim",
                engine: "lean4",
                formal_target: None,
                module_path: Some(module),
                critical: false,
            },
            Change {
                actor: "fixture",
                git_commit: Some(base),
            },
        )?;
    }
    Ok(())
}

fn approve(
    store: &mut StateStore,
    alias: &str,
    base: &str,
    head: &str,
    module: &str,
) -> Result<i64> {
    let approval = T3PlanApproval {
        plan_sha256: hash(alias),
        base_commit: base.into(),
        reviewed_commit: head.into(),
        anchor_module: module.into(),
        preview_event_seq: store.status()?.last_event_seq,
    };
    // Synthetic operator approval: this exercises dispatch, not preview/proof authority.
    assert!(store.apply_t3_plan(&approval, &[module])?.is_some());
    Ok(store.status()?.last_event_seq)
}

#[test]
fn v2_round_robin_isolates_failed_plans_and_resumes_durable_pages() -> Result<()> {
    let temp = short_tempdir()?;
    let root = temp.path().canonicalize()?;
    let repo = init_repo(&root)?;
    let tree = git(&repo, &["mktree"])?;
    let base = git(&repo, &["commit-tree", &tree, "-m", "Base"])?;
    let head = git(
        &repo,
        &["commit-tree", &tree, "-p", &base, "-m", "Approved main"],
    )?;
    let foreign = git(
        &repo,
        &["commit-tree", &tree, "-p", &head, "-m", "Another review"],
    )?;
    git(&repo, &["update-ref", MAIN, &head])?;
    fs::write(root.join("foreign-head"), &foreign)?;

    fs::create_dir(root.join(".runtime"))?;
    let mut store = StateStore::open(root.join(DB))?;
    store.bind_task_repository(repo.to_str().context("non-UTF8 fixture path")?, &head)?;
    for (module, claims) in [
        ("Alpha.lean", ["A1", "A2", "A3"]),
        ("Beta.lean", ["B1", "B2", "B3"]),
    ] {
        make_claims(&mut store, &base, module, &claims)?;
    }
    for (alias, module) in [
        ("a", "Alpha.lean"),
        ("b", "Beta.lean"),
        ("c", "Empty.lean"),
        ("d", "Foreign.lean"),
        ("e", "Beta.lean"),
        ("f", "Beta.lean"),
        ("1", "Beta.lean"),
    ] {
        approve(
            &mut store,
            alias,
            &base,
            if alias == "d" { &foreign } else { &head },
            module,
        )?;
    }
    let seq = store.status()?.last_event_seq;
    let done = step(&mut store, "c")?;
    assert_eq!(done.status, T3SweepState::Complete);
    drop(store);

    install_worker(&root)?;
    let config = root.join("autosweep.json");
    write_private(&config, 0o600, &serde_json::to_vec(&catalog(&root))?)?;

    let mut first = Daemon::spawn(&root, &config)?;
    first.poll("first worker entering its gate", |_| {
        Ok((!calls(&root)?.is_empty()).then_some(()))
    })?;
    // A spawn-per-tick implementation would overlap the gated child.
    thread::sleep(Duration::from_millis(3 * INTERVAL_MS));
    let gated_calls = calls(&root);
    let overlap = root.join("worker-overlap").exists();
    fs::write(root.join("release-first-page"), b"")?;
    assert_eq!(gated_calls?, ["b:0"]);
    assert!(
        !overlap,
        "another worker launched while first child was gated"
    );
    first.sweep_stopped("reviewed main changed or became unavailable", &root)?;
    assert_eq!(
        calls(&root)?,
        ["b:0", "e:0", "a:0", "f:0", "1:0", "b:1", "a:1", "b:2"]
    );
    assert!(!root.join("worker-overlap").exists());
    assert_eq!(git(&repo, &["rev-parse", MAIN])?, foreign);
    assert_checkpoint(&root, "b", &head, seq, 3, None)?;
    assert_checkpoint(&root, "a", &head, seq, 2, Some("A2"))?;
    assert_checkpoint(&root, "1", &head, seq, 2, Some("B2"))?;
    assert_event_neutral(&root, seq)?;
    first.stop_for_restart(&root)?;

    git(&repo, &["update-ref", MAIN, &head, &foreign])?;
    fs::write(root.join("resuming"), b"")?;
    let mut restarted = Daemon::spawn(&root, &config)?;
    restarted.sweep_stopped("one or more plans disabled", &root)?;
    assert_eq!(
        calls(&root)?,
        [
            "b:0", "e:0", "a:0", "f:0", "1:0", "b:1", "a:1", "b:2", // first daemon
            "e:0", "a:2", "f:0", "1:2", // fresh process, durable cursors
        ]
    );
    assert!(!root.join("worker-overlap").exists());
    assert_checkpoint(&root, "a", &head, seq, 3, None)?;
    assert_checkpoint(&root, "b", &head, seq, 3, None)?;
    assert_checkpoint(&root, "1", &head, seq, 3, None)?;
    assert_eq!(checkpoint(&root, "c")?, done);
    assert!(
        StateStore::open_readonly(root.join(DB))?
            .applied_t3_plan(&hash("0"))?
            .is_none()
    );
    for alias in ["d", "e", "f"] {
        let progress = checkpoint(&root, alias)?;
        assert_eq!(progress.status, T3SweepState::NotStarted);
        assert_eq!(progress.revision, None);
        assert_eq!(progress.checkpoint_event_seq, None);
    }
    assert_eq!(git(&repo, &["rev-parse", MAIN])?, head);
    assert_event_neutral(&root, seq)?;
    Ok(())
}

#[test]
fn v3_follows_new_approvals_and_fast_forwards_without_replaying_old_work() -> Result<()> {
    let temp = short_tempdir()?;
    let root = temp.path().canonicalize()?;
    let repo = init_repo(&root)?;
    let repo_path = repo.to_str().context("non-UTF8 fixture path")?;
    let tree = git(&repo, &["mktree"])?;
    let base = git(&repo, &["commit-tree", &tree, "-m", "Base"])?;
    let head = git(
        &repo,
        &["commit-tree", &tree, "-p", &base, "-m", "Initial review"],
    )?;
    let next = git(
        &repo,
        &["commit-tree", &tree, "-p", &head, "-m", "Next review"],
    )?;
    let divergent = git(
        &repo,
        &["commit-tree", &tree, "-p", &head, "-m", "Sibling review"],
    )?;
    git(&repo, &["update-ref", MAIN, &head])?;

    let mut store = StateStore::open(root.join(DB))?;
    store.bind_task_repository(repo_path, &head)?;
    make_claims(&mut store, &base, "Alpha.lean", &["A1"])?;
    make_claims(&mut store, &base, "Beta.lean", &["B1", "B2"])?;
    make_claims(&mut store, &base, "Gamma.lean", &["G1"])?;
    let initial_seq = store.status()?.last_event_seq;
    install_worker(&root)?;
    write_private(&root.join("follow-mode"), 0o600, b"")?;
    let config = root.join("follow.json");
    write_private(&config, 0o600, &serde_json::to_vec(&follow_config(&root))?)?;
    let mut daemon = Daemon::spawn(&root, &config)?;
    daemon.poll("autosweep socket", |_| {
        Ok(root.join(SOCKET).exists().then_some(()))
    })?;
    daemon.sweep_idle(&root, &[])?;
    assert_event_neutral(&root, initial_seq)?;

    // A failed hash is attempted once but doesn't end discovery or block a later same-head approval.
    approve(&mut store, "f", &base, &head, "Fail.lean")?;
    daemon.poll("failure isolation", |daemon| {
        Ok(daemon
            .logs()
            .contains("ai4mathd t3 autosweep plan disabled: step failed")
            .then_some(()))
    })?;
    let alpha_seq = approve(&mut store, "a", &base, &head, "Alpha.lean")?;
    daemon.poll("later same-head approval", |_| {
        Ok((checkpoint(&root, "a")?.status == T3SweepState::Complete).then_some(()))
    })?;
    daemon.sweep_idle(&root, &["f:0", "a:0"])?;
    assert_checkpoint(&root, "a", &head, alpha_seq, 1, None)?;
    assert_eq!(checkpoint(&root, "f")?.status, T3SweepState::NotStarted);

    // Seed the other reviewed generations before moving main so the test can distinguish
    // exact-head admission from dispatching every approval the ledger contains.
    approve(&mut store, "1", &head, &next, "Gamma.lean")?;
    approve(&mut store, "c", &head, &divergent, "Sibling.lean")?;
    daemon.sweep_idle(&root, &["f:0", "a:0"])?;
    let beta_seq = approve(&mut store, "b", &base, &head, "Beta.lean")?;
    daemon.poll("old head's first page committed in the gated child", |_| {
        Ok((checkpoint(&root, "b")?.revision == Some(1)).then_some(()))
    })?;
    assert_checkpoint(&root, "b", &head, beta_seq, 1, Some("B1"))?;

    // Model a pending promotion across the real Git CAS without fabricating a proof receipt.
    // The worker is gated until the journal and new ref are both visible.
    let db = Connection::open_with_flags(root.join(DB), OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    assert_eq!(db.execute(
        "INSERT INTO promotion_intent(singleton,repo,claim_id,base_commit,new_commit,file_path,target_decl,proof_body)
         VALUES(1,?1,'G1',?2,?3,'Gamma.lean','Fixture.gamma','fixture')",
        [repo_path, head.as_str(), next.as_str()],
    )?, 1);
    git(&repo, &["update-ref", MAIN, &next, &head])?;
    write_private(&root.join("release-follow-page"), 0o600, b"")?;
    daemon.sweep_idle(&root, &["f:0", "a:0", "b:0"])?;
    assert_eq!(checkpoint(&root, "1")?.status, T3SweepState::NotStarted);
    assert_eq!(
        db.execute("DELETE FROM promotion_intent WHERE singleton=1", [])?,
        1
    );

    daemon.poll(
        "current-head plan completed after clearing the journal",
        |_| Ok((checkpoint(&root, "1")?.status == T3SweepState::Complete).then_some(())),
    )?;
    let expected_calls = ["f:0", "a:0", "b:0", "1:0"];
    daemon.sweep_idle(&root, &expected_calls)?;
    assert_checkpoint(&root, "1", &next, beta_seq, 1, None)?;
    assert_checkpoint(&root, "b", &head, beta_seq, 1, Some("B1"))?;
    assert_checkpoint(&root, "a", &head, alpha_seq, 1, None)?;

    let sibling_before = checkpoint(&root, "c")?;
    assert_eq!(sibling_before.status, T3SweepState::NotStarted);
    git(&repo, &["update-ref", MAIN, &divergent, &next])?;
    daemon.sweep_stopped("reviewed main did not advance by fast-forward", &root)?;
    assert_eq!(calls(&root)?, expected_calls);
    assert_eq!(checkpoint(&root, "c")?, sibling_before);
    assert!(!root.join("worker-overlap").exists());
    assert_event_neutral(&root, beta_seq)?;
    Ok(())
}
