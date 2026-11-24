#![cfg(unix)]

use ai4math_store::{Change, NewClaim, StateStore};
use anyhow::{Context, Result, ensure};
use rusqlite::Connection;
use serde_json::{Value, json};
use std::{
    fs::{self, OpenOptions},
    io::{BufRead, BufReader, ErrorKind, Write},
    os::unix::{
        fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
        net::UnixStream,
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const SOCKET: &str = ".runtime/ai4mathd.sock";
const DB: &str = ".runtime/state.sqlite";
const MAIN: &str = "refs/heads/main";
const ANCHOR: &str = "Proof.lean";
const INTERVAL_MS: u64 = 1_000;
const WAIT: Duration = Duration::from_secs(8);
const PIN_FIELDS: [&str; 4] = [
    "policy_sha256",
    "compiler_sha256",
    "exporter_sha256",
    "checker_sha256",
];

fn release_pins() -> [String; 4] {
    ["a", "b", "c", "d"].map(|hex| hex.repeat(64))
}

fn write_private(path: &Path, mode: u32, content: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)?;
    file.set_permissions(fs::Permissions::from_mode(mode))?;
    file.write_all(content)?;
    Ok(())
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
    ensure!(
        output.status.success(),
        "git {args:?} ({}): {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    repo: PathBuf,
    reviewed: String,
    successor: String,
    seq: i64,
}

impl Fixture {
    fn new() -> Result<Self> {
        // Keep the canonical endpoint below macOS's Unix socket path limit.
        let temp = tempfile::Builder::new()
            .prefix("a4")
            .rand_bytes(4)
            .tempdir()?;
        let root = temp.path().canonicalize()?;
        let work = root.join("work");
        fs::DirBuilder::new().mode(0o700).create(&work)?;
        git(
            &work,
            &["init", "--quiet", "--initial-branch=main", "--template="],
        )?;
        fs::write(work.join(ANCHOR), "theorem proof : True := True.intro\n")?;
        git(&work, &["add", ANCHOR])?;
        git(&work, &["commit", "--quiet", "-m", "reviewed main"])?;
        let reviewed = git(&work, &["rev-parse", MAIN])?;
        git(
            &work,
            &[
                "commit",
                "--quiet",
                "--allow-empty",
                "-m",
                "unreviewed main",
            ],
        )?;
        let successor = git(&work, &["rev-parse", MAIN])?;
        git(&root, &["clone", "--quiet", "--bare", "work", "repo.git"])?;
        let repo = root.join("repo.git");
        git(&repo, &["update-ref", MAIN, &reviewed, &successor])?;

        let runtime = root.join(".runtime");
        fs::DirBuilder::new().mode(0o700).create(&runtime)?;
        write_private(&runtime.join("primary_engine"), 0o600, b"lean4\n")?;
        let mut state = StateStore::open(root.join(DB))?;
        state.bind_task_repository(repo.to_str().context("fixture repo path")?, &reviewed)?;
        let seq = state.status()?.last_event_seq;
        assert!(seq > 0, "fixture must exercise a real bound ledger");
        drop(state);

        fs::write(root.join("reviewed-oid"), format!("{reviewed}\n"))?;
        fs::write(root.join("release-pins"), release_pins().join(" ") + "\n")?;
        write_private(
            &root.join("fixture-cli"),
            0o700,
            br#"#!/bin/sh
set -eu
IFS= read -r reviewed < reviewed-oid
[ "$2" = "$PWD" ] && [ "$3" = "$2/repo.git" ] && [ "$4" = Proof.lean ] &&
    [ "$5" = "$reviewed" ] && [ "$6" = "$2/unused-native" ] || exit 97
case "$1" in
    t2-audit)
        [ "$#" -eq 7 ] && [ "$7" = --daemon-parent ] || exit 97
        ;;
    t2-release-check)
        read -r policy compiler exporter checker < release-pins
        [ "$#" -eq 11 ] && [ "$7" = "$policy" ] && [ "$8" = "$compiler" ] &&
            [ "$9" = "$exporter" ] && [ "${10}" = "$checker" ] &&
            [ "${11}" = --daemon-parent ] || exit 97
        ;;
    *) exit 97 ;;
esac
if ! /bin/mkdir child-active 2>/dev/null; then
    printf 'overlap\n' >> calls
    exit 98
fi
trap '/bin/rmdir child-active' EXIT
printf '%s\n' "$$" > child-pid
printf 'begin\n' >> calls
while [ -e hold ] && [ ! -e release ]; do /bin/sleep 0.02; done
/bin/cat sample.json
printf 'end\n' >> calls
"#,
        )?;
        write_private(&root.join("unused-native"), 0o700, b"#!/bin/sh\nexit 99\n")?;
        Ok(Self {
            _temp: temp,
            root,
            repo,
            reviewed,
            successor,
            seq,
        })
    }

    fn config(&self, max_runs: u64, timeout_ms: u64) -> Value {
        json!({
            "version": 1,
            "cli_executable": self.root.join("fixture-cli"),
            "bare_repo": self.repo,
            "anchor_module": ANCHOR,
            "reviewed_main": self.reviewed,
            "native_executable": self.root.join("unused-native"),
            "interval_ms": INTERVAL_MS,
            "audit_timeout_ms": timeout_ms,
            "max_runs": max_runs,
        })
    }

    fn release_config(&self, max_runs: u64, timeout_ms: u64) -> Value {
        let mut config = self.config(max_runs, timeout_ms);
        config["version"] = json!(2);
        for (field, pin) in PIN_FIELDS.into_iter().zip(release_pins()) {
            config[field] = json!(pin);
        }
        config
    }

    fn write_config(&self, label: &str, config: &Value) -> Result<PathBuf> {
        let path = self.root.join(format!("{label}.json"));
        write_private(&path, 0o600, &serde_json::to_vec(config)?)?;
        Ok(path)
    }

    fn sample(&self) -> Value {
        json!({
            "schema": "ai4math.t2.resident-sample.v1",
            "advisory_only": true,
            "reviewed_commit": self.reviewed,
            "ledger_event_seq": self.seq,
            "compile_ms": 27,
            "module_count": 2,
            "debt_findings": 1,
        })
    }

    fn release_sample(&self) -> Value {
        let mut sample = self.sample();
        sample["schema"] = json!("ai4math.t2.resident-sample.v2");
        sample["debt_findings"] = json!(0);
        sample["independent_checks"] = json!(2);
        sample["policy_sha256"] = json!(release_pins()[0]);
        sample
    }

    fn write_sample(&self, payload: &Value) -> Result<()> {
        Ok(fs::write(
            self.root.join("sample.json"),
            serde_json::to_vec(payload)?,
        )?)
    }

    fn move_main(&self) -> Result<()> {
        git(
            &self.repo,
            &["update-ref", MAIN, &self.successor, &self.reviewed],
        )?;
        Ok(())
    }

    fn check_state(&self, expected_head: &str) -> Result<()> {
        let state = StateStore::open_readonly(self.root.join(DB))?;
        assert_eq!(state.status()?.last_event_seq, self.seq);
        assert!(state.events_after(self.seq, 1)?.is_empty());
        assert_eq!(git(&self.repo, &["rev-parse", MAIN])?, expected_head);
        Ok(())
    }

    fn calls(&self) -> Result<String> {
        match fs::read_to_string(self.root.join("calls")) {
            Ok(text) => Ok(text),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(String::new()),
            Err(e) => Err(e.into()),
        }
    }

    fn status(&self) -> Result<Option<Value>> {
        let mut stream = match UnixStream::connect(self.root.join(SOCKET)) {
            Ok(stream) => stream,
            Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::ConnectionRefused) => {
                return Ok(None);
            }
            Err(e) => return Err(e.into()),
        };
        stream.set_read_timeout(Some(Duration::from_secs(2)))?;
        stream.set_write_timeout(Some(Duration::from_secs(2)))?;
        writeln!(
            stream,
            "{}",
            json!({"jsonrpc":"2.0", "id":7, "method":"status", "params":{}})
        )?;
        let mut line = String::new();
        ensure!(
            BufReader::new(stream).read_line(&mut line)? > 0,
            "daemon closed status RPC"
        );
        let reply: Value = serde_json::from_str(&line)?;
        assert_eq!(reply["jsonrpc"], "2.0");
        assert_eq!(reply["id"], 7);
        assert_eq!(
            reply["result"]["event_seq"], self.seq,
            "status reply: {reply}"
        );
        Ok(Some(reply["result"].clone()))
    }
}

struct Daemon {
    child: Child,
    log: tempfile::NamedTempFile,
}

impl Daemon {
    fn spawn(f: &Fixture, config: Option<&Path>) -> Result<Self> {
        let log = tempfile::NamedTempFile::new_in(&f.root)?;
        let mut command = Command::new(env!("CARGO_BIN_EXE_ai4mathd"));
        command
            .arg("--project")
            .arg(&f.root)
            .args(["--tick-ms", "100"]);
        if let Some(config) = config {
            command.arg("--t2-audit").arg(config);
        }
        let child = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log.reopen()?)
            .spawn()?;
        Ok(Self { child, log })
    }

    fn log(&self) -> String {
        fs::read_to_string(self.log.path()).unwrap_or_default()
    }

    fn wait_for(
        &mut self,
        f: &Fixture,
        what: &str,
        ready: impl Fn(&Value) -> bool,
    ) -> Result<Value> {
        let deadline = Instant::now() + WAIT;
        let mut last = Value::Null;
        loop {
            if let Some(exit) = self.child.try_wait()? {
                anyhow::bail!(
                    "daemon exited ({exit}) while waiting for {what}: {}",
                    self.log()
                );
            }
            if let Some(status) = f.status()? {
                if ready(&status) {
                    return Ok(status);
                }
                last = status;
            }
            ensure!(
                Instant::now() < deadline,
                "waiting for {what}; last status: {last}; stderr: {}",
                self.log()
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn wait_audit(&mut self, f: &Fixture, expected: &Value) -> Result<()> {
        self.wait_for(f, &format!("audit status {expected}"), |s| {
            &s["t2_audit"] == expected
        })?;
        Ok(())
    }

    fn reject_before_socket(&mut self, f: &Fixture) -> Result<()> {
        let deadline = Instant::now() + WAIT;
        loop {
            let exit = self.child.try_wait()?;
            assert!(
                !f.root.join(SOCKET).exists(),
                "invalid config published an RPC socket"
            );
            assert!(
                !f.root.join(".mathwiki").exists(),
                "invalid config entered daemon runtime"
            );
            if let Some(exit) = exit {
                assert!(!exit.success(), "invalid config exited successfully");
                assert_eq!(
                    self.log().trim(),
                    "ai4mathd: invalid --t2-audit configuration"
                );
                return Ok(());
            }
            ensure!(
                Instant::now() < deadline,
                "invalid config did not exit: {}",
                self.log()
            );
            thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn resident_t2_is_absent_without_opt_in() -> Result<()> {
    let f = Fixture::new()?;
    f.write_config("t2-audit", &f.config(1, 5_000))?;
    f.write_sample(&f.sample())?;
    let mut daemon = Daemon::spawn(&f, None)?;
    let status = daemon.wait_for(&f, "ordinary status", |_| true)?;
    assert!(
        status.get("t2_audit").is_none(),
        "unexpected status: {status}"
    );
    assert!(f.calls()?.is_empty());
    f.check_state(&f.reviewed)
}

#[test]
fn resident_t2_rejects_invalid_config_before_socket() -> Result<()> {
    let f = Fixture::new()?;
    for (label, field, value) in [
        ("version", "version", json!(2)),
        ("interval", "interval_ms", json!(INTERVAL_MS - 1)),
        ("timeout-low", "audit_timeout_ms", json!(99)),
        ("timeout-high", "audit_timeout_ms", json!(180_001)),
        ("runs-low", "max_runs", json!(0)),
        ("runs-high", "max_runs", json!(65)),
        ("bad-review", "reviewed_main", json!("main")),
        ("unknown", "unknown", json!(true)),
    ] {
        let mut bad = f.config(1, 5_000);
        bad[field] = value;
        let path = f.write_config(label, &bad)?;
        Daemon::spawn(&f, Some(&path))?
            .reject_before_socket(&f)
            .with_context(|| format!("invalid fixture {label}"))?;
    }
    let shared = f.write_config("shared-mode", &f.config(1, 5_000))?;
    fs::set_permissions(&shared, fs::Permissions::from_mode(0o644))?;
    Daemon::spawn(&f, Some(&shared))?.reject_before_socket(&f)?;
    assert!(f.calls()?.is_empty());
    Ok(())
}

#[test]
fn resident_t2_is_serial_bounded_and_advisory_at_the_pin() -> Result<()> {
    let f = Fixture::new()?;
    let sample = f.sample();
    f.write_sample(&sample)?;
    fs::write(f.root.join("hold"), b"")?;
    let config = f.write_config("t2-audit", &f.config(2, 5_000))?;
    let mut daemon = Daemon::spawn(&f, Some(&config))?;
    let running = json!({"status":"running", "attempts":1, "exhausted":false});
    daemon.wait_audit(&f, &running)?;

    // Cross a full launch interval with the first process still blocked. RPC
    // must remain responsive and attempts must not race ahead of the worker.
    thread::sleep(Duration::from_millis(INTERVAL_MS + 150));
    let status = f.status()?.context("status while worker is in flight")?;
    assert_eq!(status["t2_audit"], running);
    assert_eq!(f.calls()?, "begin\n");
    fs::write(f.root.join("release"), b"")?;

    let observed =
        json!({"status":"observation", "attempts":2, "exhausted":true, "observation":sample});
    daemon.wait_audit(&f, &observed)?;
    // Cross another interval after exhaustion; there cannot be a third run.
    thread::sleep(Duration::from_millis(INTERVAL_MS + 150));
    assert_eq!(
        f.status()?.context("status after exhaustion")?["t2_audit"],
        observed
    );
    assert_eq!(f.calls()?, "begin\nend\nbegin\nend\n");
    f.check_state(&f.reviewed)
}

#[test]
fn resident_t2_waits_for_pending_journal_without_charging_an_attempt() -> Result<()> {
    let mut f = Fixture::new()?;
    let mut state = StateStore::open(f.root.join(DB))?;
    state.create_claim(
        NewClaim {
            id: "audit-fixture",
            title: "Fixture claim",
            informal_statement: "True",
            engine: "lean4",
            formal_target: Some("proof"),
            module_path: Some(ANCHOR),
            critical: false,
        },
        Change {
            actor: "resident-journal-fixture",
            git_commit: Some(&f.reviewed),
        },
    )?;
    f.seq = state.status()?.last_event_seq;
    let db = Connection::open(f.root.join(DB))?;
    // Test-only journal row models the coordinator pause. It is not a proof receipt.
    assert_eq!(db.execute(
        "INSERT INTO promotion_intent(singleton,repo,claim_id,base_commit,new_commit,file_path,target_decl,proof_body)
         VALUES(1,?1,'audit-fixture',?2,?3,?4,'proof','fixture')",
        [f.repo.to_str().context("private fixture repo")?,f.reviewed.as_str(),f.successor.as_str(),ANCHOR],
    )?,1);
    f.write_sample(&f.sample())?;
    let config = f.write_config("t2-audit", &f.config(1, 5_000))?;
    let mut daemon = Daemon::spawn(&f, Some(&config))?;
    daemon.wait_audit(
        &f,
        &json!({"status":"waiting","attempts":0,"exhausted":false}),
    )?;
    assert!(f.calls()?.is_empty());
    f.check_state(&f.reviewed)?;
    assert_eq!(
        db.execute("DELETE FROM promotion_intent WHERE singleton=1", [])?,
        1
    );
    daemon.wait_audit(
        &f,
        &json!({"status":"observation","attempts":1,"exhausted":true,"observation":f.sample()}),
    )?;
    assert_eq!(f.calls()?, "begin\nend\n");
    f.check_state(&f.reviewed)
}

#[test]
fn resident_t2_discards_success_after_independent_ledger_generation_change() -> Result<()> {
    let mut f = Fixture::new()?;
    f.write_sample(&f.sample())?;
    fs::write(f.root.join("hold"), b"")?;
    let config = f.write_config("t2-audit", &f.config(1, 5_000))?;
    let mut daemon = Daemon::spawn(&f, Some(&config))?;
    daemon.wait_audit(
        &f,
        &json!({"status":"running","attempts":1,"exhausted":false}),
    )?;
    let mut state = StateStore::open(f.root.join(DB))?;
    let next = state.append_event(
        Change {
            actor: "resident-fixture-concurrent",
            git_commit: None,
        },
        "NOTE_CREATED",
        "note",
        "independent",
        json!({}),
    )?;
    assert!(next > f.seq);
    f.seq = next; // Only the RPC helper's expected cursor changes, not the child's old sample.
    fs::write(f.root.join("release"), b"")?;
    daemon.wait_audit(
        &f,
        &json!({"status":"unavailable","attempts":1,"exhausted":true}),
    )?;
    assert_eq!(state.status()?.last_event_seq, next);
    assert_eq!(git(&f.repo, &["rev-parse", MAIN])?, f.reviewed);
    assert_eq!(f.calls()?, "begin\nend\n");
    Ok(())
}

#[test]
fn resident_t2_refuses_main_moved_before_dispatch() -> Result<()> {
    let f = Fixture::new()?;
    f.write_sample(&f.sample())?;
    let config = f.write_config("t2-audit", &f.config(2, 5_000))?;
    f.move_main()?;
    let mut daemon = Daemon::spawn(&f, Some(&config))?;
    daemon.wait_audit(
        &f,
        &json!({"status":"head_changed", "attempts":0, "exhausted":false}),
    )?;
    assert!(f.calls()?.is_empty());
    f.check_state(&f.successor)
}

#[test]
fn resident_t2_discards_success_when_main_moves_in_flight() -> Result<()> {
    let f = Fixture::new()?;
    f.write_sample(&f.sample())?;
    fs::write(f.root.join("hold"), b"")?;
    let config = f.write_config("t2-audit", &f.config(2, 5_000))?;
    let mut daemon = Daemon::spawn(&f, Some(&config))?;
    daemon.wait_audit(
        &f,
        &json!({"status":"running", "attempts":1, "exhausted":false}),
    )?;
    f.move_main()?;
    fs::write(f.root.join("release"), b"")?;
    daemon.wait_audit(
        &f,
        &json!({"status":"head_changed", "attempts":1, "exhausted":false}),
    )?;
    assert_eq!(f.calls()?, "begin\nend\n");
    f.check_state(&f.successor)
}

#[test]
fn resident_t2_refuses_unaudited_worker_payloads() -> Result<()> {
    for (field, bad) in [
        ("advisory_only", json!(false)),
        ("reviewed_commit", json!("a".repeat(40))),
        ("ledger_event_seq", json!(0)),
        ("repo_path", json!("private-fixture-path")),
    ] {
        let f = Fixture::new()?;
        let mut payload = f.sample();
        payload[field] = bad;
        f.write_sample(&payload)?;
        let config = f.write_config("t2-audit", &f.config(1, 5_000))?;
        let mut daemon = Daemon::spawn(&f, Some(&config))?;
        daemon
            .wait_audit(
                &f,
                &json!({"status":"unavailable", "attempts":1, "exhausted":true}),
            )
            .with_context(|| format!("worker payload with invalid {field}"))?;
        assert!(!daemon.log().contains("private-fixture-path"));
        assert_eq!(f.calls()?, "begin\nend\n");
        f.check_state(&f.reviewed)?;
    }
    Ok(())
}

#[test]
fn resident_t2_release_dispatches_pins_but_does_not_publish_them() -> Result<()> {
    let f = Fixture::new()?;
    let config = f.write_config("t2-release", &f.release_config(1, 270_000))?;
    let mut public = f.release_sample();
    f.write_sample(&public)?;
    public.as_object_mut().unwrap().remove("policy_sha256");
    let mut daemon = Daemon::spawn(&f, Some(&config))?;
    daemon.wait_audit(
        &f,
        &json!({"status":"observation", "attempts":1, "exhausted":true, "observation":public}),
    )?;
    for pin in release_pins() {
        assert!(!daemon.log().contains(&pin));
    }
    assert_eq!(f.calls()?, "begin\nend\n");
    f.check_state(&f.reviewed)
}

#[test]
fn resident_t2_release_refuses_cross_version_and_unaudited_payloads() -> Result<()> {
    for (config_v2, sample_v2, invalid_field) in [
        (true, false, None),
        (false, true, None),
        (true, true, Some(("policy_sha256", json!("f".repeat(64))))),
        (true, true, Some(("independent_checks", json!(0)))),
        (true, true, Some(("debt_findings", json!(1)))),
    ] {
        let f = Fixture::new()?;
        let config = if config_v2 {
            f.release_config(1, 5_000)
        } else {
            f.config(1, 5_000)
        };
        let mut sample = if sample_v2 {
            f.release_sample()
        } else {
            f.sample()
        };
        if let Some((field, invalid)) = invalid_field {
            sample[field] = invalid;
        }
        f.write_sample(&sample)?;
        let config = f.write_config("t2-audit", &config)?;
        let mut daemon = Daemon::spawn(&f, Some(&config))?;
        daemon.wait_audit(
            &f,
            &json!({"status":"unavailable", "attempts":1, "exhausted":true}),
        )?;
        assert_eq!(f.calls()?, "begin\nend\n");
        f.check_state(&f.reviewed)?;
    }
    Ok(())
}

#[test]
fn resident_t2_times_out_and_reaps_the_worker() -> Result<()> {
    let f = Fixture::new()?;
    f.write_sample(&f.sample())?;
    fs::write(f.root.join("hold"), b"")?;
    let config = f.write_config("t2-audit", &f.config(1, 800))?;
    let mut daemon = Daemon::spawn(&f, Some(&config))?;
    daemon.wait_audit(
        &f,
        &json!({"status":"unavailable", "attempts":1, "exhausted":true}),
    )?;
    assert_eq!(f.calls()?, "begin\n");
    let pid: libc::pid_t = fs::read_to_string(f.root.join("child-pid"))?
        .trim()
        .parse()?;
    assert!(pid > 1);
    // SAFETY: signal 0 only queries the positive PID recorded by our own worker.
    assert_eq!(
        unsafe { libc::kill(pid, 0) },
        -1,
        "timed-out child still exists"
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
    f.check_state(&f.reviewed)
}
