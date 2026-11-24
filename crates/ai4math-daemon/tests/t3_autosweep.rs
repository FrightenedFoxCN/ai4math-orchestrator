#![cfg(unix)]

use anyhow::{Result, ensure};
use serde_json::{Value, json};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::Path,
    process::{Child, Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

const SOCKET: &str = ".runtime/ai4mathd.sock";
const STARTUP: Duration = Duration::from_secs(5);

fn short_tempdir() -> Result<tempfile::TempDir> {
    // Keep the canonical socket path below the Unix sun_path limit on macOS.
    Ok(tempfile::Builder::new()
        .prefix("a4")
        .rand_bytes(4)
        .tempdir()?)
}

struct Daemon {
    child: Child,
    log: tempfile::NamedTempFile,
}

impl Daemon {
    fn spawn(root: &Path, config: Option<&Path>) -> Result<Self> {
        Self::spawn_with_flag(root, config, "--t3-autosweep")
    }

    fn spawn_with_flag(root: &Path, config: Option<&Path>, flag: &str) -> Result<Self> {
        let log = tempfile::NamedTempFile::new_in(root)?;
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_ai4mathd"));
        cmd.arg("--project").arg(root).args(["--tick-ms", "100"]);
        if let Some(config) = config {
            cmd.arg(flag).arg(config);
        }
        let child = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log.reopen()?)
            .spawn()?;
        Ok(Self { child, log })
    }

    fn logs(&self) -> String {
        fs::read_to_string(self.log.path()).unwrap_or_default()
    }

    #[cfg(target_os = "macos")]
    fn poll<T>(
        &mut self,
        timeout: Duration,
        what: &str,
        mut observe: impl FnMut() -> Result<Option<T>>,
    ) -> Result<T> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(value) = observe()? {
                return Ok(value);
            }
            if let Some(status) = self.child.try_wait()? {
                anyhow::bail!(
                    "daemon exited ({status}) while waiting for {what}: {}",
                    self.logs()
                );
            }
            ensure!(
                Instant::now() < deadline,
                "waiting for {what}: {}",
                self.logs()
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn startup_error(&mut self) -> Result<ExitStatus> {
        let deadline = Instant::now() + STARTUP;
        loop {
            if let Some(status) = self.child.try_wait()? {
                return Ok(status);
            }
            ensure!(
                Instant::now() < deadline,
                "invalid config did not fail startup: {}",
                self.logs()
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

fn catalog(cli: &Path, bare: &Path, native: &Path, hash: &str, interval_ms: u64) -> Value {
    json!({
        "version": 1,
        "cli_executable": cli,
        "bare_repo": bare,
        "plan_sha256": hash,
        "native_executable": native,
        "page_limit": 1,
        "interval_ms": interval_ms,
    })
}

fn write_new(path: &Path, mode: u32, bytes: &[u8]) -> Result<()> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)?
        .write_all(bytes)?;
    Ok(())
}

#[test]
fn invalid_autosweep_catalog_fails_before_publishing_socket() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let tmp = short_tempdir()?;
    let root = tmp.path().canonicalize()?;
    let cli = root.join("fixture-cli");
    let native = root.join("fixture-native");
    for tool in [&cli, &native] {
        write_new(tool, 0o700, b"#!/bin/sh\nexit 1\n")?;
        fs::set_permissions(tool, fs::Permissions::from_mode(0o700))?;
    }
    let bare = root.join("repo.git");
    fs::create_dir(&bare)?;
    let valid = catalog(&cli, &bare, &native, &"b".repeat(64), 3_000);
    for (name, body, mode) in [
        ("malformed", b"{not JSON".to_vec(), 0o600),
        ("nonprivate", serde_json::to_vec(&valid)?, 0o644),
        (
            "extra-authority",
            {
                let mut extra = valid.clone();
                extra["restart"] = json!(true);
                serde_json::to_vec(&extra)?
            },
            0o600,
        ),
        (
            "oversized-page",
            {
                let mut extra = valid.clone();
                extra["page_limit"] = json!(9);
                serde_json::to_vec(&extra)?
            },
            0o600,
        ),
    ] {
        let path = root.join(name);
        write_new(&path, mode, &body)?;
        // Test the requested mode even when the caller has a stricter umask.
        fs::set_permissions(&path, fs::Permissions::from_mode(mode))?;
        let mut daemon = Daemon::spawn(&root, Some(&path))?;
        assert!(!daemon.startup_error()?.success(), "accepted {name}");
        assert!(
            daemon
                .logs()
                .contains("invalid --t3-autosweep configuration"),
            "{name}: {}",
            daemon.logs()
        );
        assert!(!root.join(SOCKET).exists(), "published socket for {name}");
    }
    Ok(())
}

#[cfg(target_os = "macos")]
mod native {
    use super::*;
    use ai4math_store::{
        Change, ClaimState, NewClaim, StateStore, T3SweepState, T3SweepStatus, Trust,
    };
    use anyhow::Context;
    use rusqlite::{Connection, OpenFlags};
    use sha2::{Digest, Sha256};
    use std::{
        env,
        io::{BufRead, BufReader},
        os::unix::{fs::FileTypeExt, net::UnixStream},
        path::PathBuf,
        process::Output,
    };

    const ANCHOR: &str = "Proof.lean";
    const INTERVAL_MS: u64 = 3_000;

    fn success(output: Output, description: &str) -> Result<String> {
        ensure!(
            output.status.success(),
            "{description}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(String::from_utf8(output.stdout)?)
    }

    fn git(dir: &Path, args: &[&str]) -> Result<String> {
        let output = Command::new("/usr/bin/git")
            .current_dir(dir)
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
        Ok(success(output, &format!("Git {args:?}"))?.trim().into())
    }

    fn native_binary(name: &str) -> Result<PathBuf> {
        let path = PathBuf::from(env::var_os(name).with_context(|| format!("set {name}"))?);
        ensure!(
            path.is_absolute(),
            "{name} must name an absolute native executable"
        );
        path.canonicalize()
            .with_context(|| format!("resolve {name}"))
    }

    fn file_sha256(path: &Path) -> Result<String> {
        let mut digest = Sha256::new();
        std::io::copy(&mut fs::File::open(path)?, &mut digest)?;
        Ok(format!("{:x}", digest.finalize()))
    }

    fn utf8(path: &Path) -> &str {
        path.to_str().expect("UTF-8 fixture path")
    }

    struct Fixture {
        root: PathBuf,
        bare: PathBuf,
        cli: PathBuf,
        lean: PathBuf,
        review: PathBuf,
    }

    impl Fixture {
        fn new(root: PathBuf, original_cli: &Path, lean: PathBuf) -> Result<Self> {
            Self::new_with_spec(root, original_cli, lean, false)
        }

        fn new_with_spec(
            root: PathBuf,
            original_cli: &Path,
            lean: PathBuf,
            reviewed_spec: bool,
        ) -> Result<Self> {
            // Cargo binaries can be hard links. The daemon catalog requires a private,
            // ordinary executable, so use an independent copy of the actual CLI.
            let cli = root.join("ai4math");
            fs::copy(original_cli, &cli)?;
            let bare = root.join("repo.git");
            let review = root.join("review");
            let source = root.join("source");
            fs::create_dir(&source)?;
            git(&source, &["init", "--initial-branch=main", "--quiet"])?;
            fs::write(
                source.join("Defs.lean"),
                "def x : Nat := 2\ndef marker : Nat := 0\n",
            )?;
            fs::write(
                source.join(ANCHOR),
                "import Defs\ntheorem target : x = 2 := {{PROOF_BODY}}\n",
            )?;
            fs::write(
                source.join("ai4math-modules.txt"),
                if reviewed_spec {
                    "TrustedSpec.lean\nDefs.lean\nProof.lean\n"
                } else {
                    "Defs.lean\nProof.lean\n"
                },
            )?;
            if reviewed_spec {
                fs::write(
                    source.join("TrustedSpec.lean"),
                    "def approved : Prop := (2 : Nat) = 2\n",
                )?;
            }
            git(&source, &["add", "."])?;
            git(&source, &["commit", "-m", "freeze proof input"])?;
            git(&root, &["clone", "--bare", "source", "repo.git"])?;
            let f = Self {
                root,
                bare,
                cli,
                lean,
                review,
            };
            f.run("init", &["lean4"])?;
            f.declare("A", "Defs.lean", "x")?;
            f.declare("C", ANCHOR, "target")?;
            if reviewed_spec {
                f.run(
                    "bind-lean-spec",
                    &[
                        utf8(&f.bare),
                        "C",
                        "TrustedSpec.lean",
                        "approved",
                        &f.main()?,
                    ],
                )?;
            }
            let body = f.root.join("proof-body.txt");
            fs::write(&body, "by\n  rfl")?;
            f.run(
                "promote-managed",
                &[
                    utf8(&f.bare),
                    "C",
                    ANCHOR,
                    "target",
                    utf8(&f.lean),
                    utf8(&body),
                ],
            )?;
            git(&f.root, &["clone", "repo.git", "review"])?;
            Ok(f)
        }

        fn run(&self, command: &str, args: &[&str]) -> Result<String> {
            let output = Command::new(&self.cli)
                .arg(command)
                .arg(&self.root)
                .args(args)
                .output()?;
            success(output, command)
        }

        fn declare(&self, id: &str, module: &str, target: &str) -> Result<()> {
            self.run(
                "declare-claim",
                &[id, module, target, id, "Fixture statement"],
            )?;
            Ok(())
        }

        fn main(&self) -> Result<String> {
            git(&self.bare, &["rev-parse", "refs/heads/main"])
        }

        fn review_defs(&self, rhs: &str) -> Result<String> {
            fs::write(
                self.review.join("Defs.lean"),
                format!("def x : Nat := {rhs}\ndef marker : Nat := 0\n"),
            )?;
            git(&self.review, &["add", "Defs.lean"])?;
            git(
                &self.review,
                &["commit", "-m", "operator-reviewed definitional change"],
            )?;
            git(&self.review, &["push", "origin", "HEAD:refs/heads/main"])?;
            self.main()
        }

        fn preview(&self, base: &str, head: &str) -> Result<Value> {
            let preview = self.run("t3-preview", &[utf8(&self.bare), ANCHOR, base, head])?;
            Ok(serde_json::from_str(&preview)?)
        }

        fn apply(&self, base: &str, head: &str, hash: &str) -> Result<()> {
            self.run("t3-apply", &[utf8(&self.bare), ANCHOR, base, head, hash])?;
            Ok(())
        }

        fn config(&self, filename: &str, hash: &str) -> Result<PathBuf> {
            let path = self.root.join(filename);
            let mut body = catalog(&self.cli, &self.bare, &self.lean, hash, INTERVAL_MS);
            body["step_timeout_ms"] = json!(60_000);
            write_new(&path, 0o600, &serde_json::to_vec(&body)?)?;
            Ok(path)
        }

        fn stop(&self, mut daemon: Daemon) -> Result<()> {
            daemon.child.kill()?;
            daemon.child.wait()?;
            // SIGKILL cannot run socket cleanup. Remove only the known fixture socket
            // after reaping its daemon, and refuse to follow a replaced directory.
            let socket = self.root.join(SOCKET);
            let parent = socket.parent().context("fixture socket has no parent")?;
            ensure!(
                parent.canonicalize()? == parent,
                "fixture runtime path changed"
            );
            ensure!(
                fs::symlink_metadata(&socket)?.file_type().is_socket(),
                "fixture endpoint is not a Unix socket"
            );
            fs::remove_file(socket)?;
            Ok(())
        }

        fn database(&self) -> PathBuf {
            self.root.join(".runtime/state.sqlite")
        }

        fn state(&self) -> Result<StateStore> {
            StateStore::open_readonly(self.database())
        }

        fn claim(&self, id: &str) -> Result<ClaimState> {
            self.state()?
                .claim(id)?
                .with_context(|| format!("missing claim {id}"))
        }

        fn checkpoint(&self, hash: &str) -> Result<T3SweepStatus> {
            self.state()?.t3_sweep_status(hash)
        }

        fn receipt_count(&self) -> Result<i64> {
            let db =
                Connection::open_with_flags(self.database(), OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            db.busy_timeout(STARTUP)?;
            Ok(
                db.query_row("SELECT count(*) FROM verification_receipts", [], |row| {
                    row.get(0)
                })?,
            )
        }
    }

    fn digest(preview: &Value) -> Result<&str> {
        preview["plan_sha256"]
            .as_str()
            .context("preview did not return a digest")
    }

    fn request(root: &Path, method: &str, params: Value) -> Result<BufReader<UnixStream>> {
        let mut stream = UnixStream::connect(root.join(SOCKET))?;
        stream.set_read_timeout(Some(STARTUP))?;
        stream.set_write_timeout(Some(STARTUP))?;
        writeln!(
            stream,
            "{}",
            json!({"jsonrpc":"2.0","id":1,"method":method,"params":params})
        )?;
        Ok(BufReader::new(stream))
    }

    fn frame(stream: &mut BufReader<UnixStream>) -> Result<Value> {
        let mut line = String::new();
        ensure!(stream.read_line(&mut line)? > 0, "daemon closed RPC stream");
        Ok(serde_json::from_str(&line)?)
    }

    fn rpc(root: &Path, method: &str, params: Value) -> Result<Value> {
        let reply = frame(&mut request(root, method, params)?)?;
        assert_eq!(reply["id"], 1);
        Ok(reply)
    }

    fn status(root: &Path) -> Result<Value> {
        let reply = rpc(root, "status", json!({}))?;
        ensure!(
            reply["result"]["event_seq"].is_i64(),
            "unexpected status reply: {reply}"
        );
        Ok(reply["result"].clone())
    }

    fn ready(daemon: &mut Daemon, root: &Path) -> Result<()> {
        daemon.poll(STARTUP, "Unix socket", || {
            Ok(UnixStream::connect(root.join(SOCKET)).ok())
        })?;
        status(root)?;
        Ok(())
    }

    fn latched(daemon: &mut Daemon, reason: &str) -> Result<()> {
        let line = format!("ai4mathd t3 autosweep stopped: {reason}");
        let path = daemon.log.path().to_owned();
        daemon.poll(Duration::from_secs(70), &line, || {
            let log = fs::read_to_string(&path)?;
            if let Some(actual) = log
                .lines()
                .find(|line| line.starts_with("ai4mathd t3 autosweep stopped:"))
            {
                ensure!(actual == line, "expected {line:?}, got {actual:?}\n{log}");
                return Ok(Some(()));
            }
            Ok(None)
        })
    }

    #[test]
    #[ignore = "requires outer-unsandboxed macOS Seatbelt and absolute AI4MATH_TEST_LEAN and AI4MATH_TEST_CLI binaries"]
    fn native_resident_claim_discovery_tracks_t1_taint_and_sealed_t3() -> Result<()> {
        let tmp = short_tempdir()?;
        let f = Fixture::new_with_spec(
            tmp.path().canonicalize()?,
            &native_binary("AI4MATH_TEST_CLI")?,
            native_binary("AI4MATH_TEST_LEAN")?,
            true,
        )?;
        let promoted = f.main()?;
        let receipt = f.claim("C")?.receipt_id.context("no native T1 receipt")?;
        let baseline_seq = f.state()?.status()?.last_event_seq;
        let config = f.root.join("claim-discovery.json");
        write_new(
            &config,
            0o600,
            &serde_json::to_vec(&json!({"version":2,"cli_executable":f.cli,
            "bare_repo":f.bare,"claim_id":"C","interval_ms":100,"probe_timeout_ms":10_000}))?,
        )?;
        let mut daemon = Daemon::spawn_with_flag(&f.root, Some(&config), "--t3-discovery")?;
        ready(&mut daemon, &f.root)?;
        let first = daemon.poll(Duration::from_secs(15), "current T1 discovery", || {
            let sample = status(&f.root)?["t3_discovery"].clone();
            Ok((sample["status"] == "unchanged").then_some(sample))
        })?;
        assert_eq!(
            first,
            json!({"status":"unchanged","baseline_scope":"claim_t1","base_commit":promoted,
            "observed_head":promoted,"ledger_event_seq":baseline_seq,"module_order_changed":false,"changed_module_count":0,
            "plan_sha256":null,"advisory_only":true})
        );

        let reviewed = f.review_defs("Nat.succ 1")?;
        let preview = f.preview(&promoted, &reviewed)?;
        let hash = digest(&preview)?;
        let changed = daemon.poll(
            Duration::from_secs(15),
            "T1-scoped structural discovery",
            || {
                let sample = status(&f.root)?["t3_discovery"].clone();
                Ok((sample["status"] == "structural_change").then_some(sample))
            },
        )?;
        assert_eq!(
            changed,
            json!({"status":"structural_change","baseline_scope":"claim_t1","base_commit":promoted,
            "observed_head":reviewed,"ledger_event_seq":baseline_seq,"module_order_changed":false,"changed_module_count":1,
            "plan_sha256":hash,"advisory_only":true})
        );
        assert!(!changed.to_string().contains(&receipt));
        assert!(f.state()?.applied_t3_plan(hash)?.is_none());
        assert!(!f.claim("C")?.stale);
        assert_eq!(f.state()?.status()?.last_event_seq, baseline_seq);
        assert_eq!(f.receipt_count()?, 1);

        f.apply(&promoted, &reviewed, hash)?;
        let applied_seq = f.state()?.status()?.last_event_seq;
        let unavailable = daemon.poll(
            Duration::from_secs(15),
            "tainted Claim hides previous summary",
            || {
                let sample = status(&f.root)?["t3_discovery"].clone();
                Ok((sample["status"] == "unavailable").then_some(sample))
            },
        )?;
        assert_eq!(unavailable, json!({"status":"unavailable"}));
        assert!(f.claim("C")?.stale);
        assert_eq!(f.receipt_count()?, 1);
        assert_eq!(f.state()?.status()?.last_event_seq, applied_seq);
        assert_eq!(f.main()?, reviewed);
        // Explicitly perform a real comparator-backed T3; observation alone never does this.
        f.run("t3-recheck-one", &[utf8(&f.bare), hash, "C", utf8(&f.lean)])?;
        let state = f.state()?;
        let verified_seq = state.status()?.last_event_seq;
        let sealed = state
            .lean_spec_recheck("C", hash)?
            .context("no sealed T3 comparison")?;
        assert_eq!(f.receipt_count()?, 2);
        let restored = daemon.poll(Duration::from_secs(15), "sealed Claim T3 discovery", || {
            let sample = status(&f.root)?["t3_discovery"].clone();
            Ok((sample["baseline_scope"] == "claim_t3").then_some(sample))
        })?;
        assert_eq!(
            restored,
            json!({"status":"unchanged","baseline_scope":"claim_t3","base_commit":reviewed,
            "observed_head":reviewed,"ledger_event_seq":verified_seq,"module_order_changed":false,"changed_module_count":0,
            "plan_sha256":null,"advisory_only":true})
        );
        assert!(
            !restored
                .to_string()
                .contains(&sealed.candidate_receipt_sha256)
        );
        assert_eq!(state.status()?.last_event_seq, verified_seq);
        assert_eq!(f.main()?, reviewed);
        f.stop(daemon)?;
        Ok(())
    }

    #[test]
    #[ignore = "requires outer-unsandboxed macOS Seatbelt and absolute AI4MATH_TEST_LEAN and AI4MATH_TEST_CLI binaries"]
    fn native_resident_t2_audit_is_pinned_and_non_authoritative() -> Result<()> {
        let tmp = short_tempdir()?;
        let f = Fixture::new(
            tmp.path().canonicalize()?,
            &native_binary("AI4MATH_TEST_CLI")?,
            native_binary("AI4MATH_TEST_LEAN")?,
        )?;
        let reviewed = f.main()?;
        let change = Change {
            actor: "native-resident-t2-fixture",
            git_commit: Some(&reviewed),
        };
        let mut writer = StateStore::open(f.database())?;
        writer.create_claim(
            NewClaim {
                id: "unpublished-debt-id",
                title: "Fixture debt",
                informal_statement: "Unpublished fixture statement",
                engine: "lean4",
                formal_target: None,
                module_path: None,
                critical: false,
            },
            change,
        )?;
        writer.set_local_trust("unpublished-debt-id", Trust::L4a, change)?;
        let seq = writer.status()?.last_event_seq;
        let receipts = f.receipt_count()?;
        let wal = f.root.join(".runtime/state.sqlite-wal");
        let before = (fs::read(f.database())?, fs::read(&wal)?);
        let refs = git(&f.bare, &["show-ref"])?;
        let config = f.root.join("resident-t2.json");
        write_new(
            &config,
            0o600,
            &serde_json::to_vec(&json!({
                "version":1,"cli_executable":f.cli,"bare_repo":f.bare,"anchor_module":ANCHOR,"reviewed_main":reviewed,
                "native_executable":f.lean,"interval_ms":1000,"audit_timeout_ms":60000,"max_runs":2
            }))?,
        )?;
        let mut daemon = Daemon::spawn_with_flag(&f.root, Some(&config), "--t2-audit")?;
        ready(&mut daemon, &f.root)?;
        let sample = daemon.poll(
            Duration::from_secs(25),
            "both pinned native T2 samples",
            || {
                let live = status(&f.root)?;
                let audit = &live["t2_audit"];
                Ok((audit["exhausted"] == true).then_some(audit.clone()))
            },
        )?;
        assert_eq!(sample["status"], "observation", "{}", daemon.logs());
        assert_eq!(sample["attempts"], 2);
        let audit = &sample["observation"];
        assert_eq!(audit["schema"], "ai4math.t2.resident-sample.v1");
        assert_eq!(audit["advisory_only"], true);
        assert_eq!(audit["reviewed_commit"], reviewed);
        assert_eq!(audit["ledger_event_seq"], seq);
        assert_eq!(audit["module_count"], 2);
        assert_eq!(audit["debt_findings"], 2);
        assert!(audit["compile_ms"].as_u64().is_some_and(|ms| ms <= 120000));
        assert_eq!(
            audit.as_object().context("sanitized audit object")?.len(),
            7
        );
        assert!(!sample.to_string().contains("unpublished"));
        assert_eq!((fs::read(f.database())?, fs::read(&wal)?), before);
        assert_eq!(git(&f.bare, &["show-ref"])?, refs);
        assert_eq!(f.state()?.status()?.last_event_seq, seq);
        assert_eq!(f.receipt_count()?, receipts);
        assert_eq!(f.claim("C")?.local_trust, Trust::L5);
        f.stop(daemon)?;
        Ok(())
    }

    #[test]
    #[ignore = "requires outer-unsandboxed macOS Seatbelt, absolute AI4MATH_TEST_CLI, and absolute AI4MATH_TEST_LEAN with bundled leanexport and nanoda_bin"]
    fn native_resident_t2_independent_audit_is_pinned_and_non_authoritative() -> Result<()> {
        const POLICY: &str = "ai4math-t2-release.json";
        const MODULE: &str = "ResidentAudit.lean";
        const THEOREM: &str = "ResidentAuditFixture.identity";
        const CLAIM: &str = "resident-unverified-identity";
        let tmp = short_tempdir()?;
        let f = Fixture::new(
            tmp.path().canonicalize()?,
            &native_binary("AI4MATH_TEST_CLI")?,
            native_binary("AI4MATH_TEST_LEAN")?,
        )?;
        // Keep the exported theorem free of standard library dependencies. The
        // ordinary managed T1 proof still builds and its Claim must not change.
        fs::write(
            f.review.join(MODULE),
            "prelude\nnamespace ResidentAuditFixture\n\
             theorem identity {p : Prop} : p → p := fun privateProof => privateProof\n\
             end ResidentAuditFixture\n",
        )?;
        const MANIFEST: &str = "ai4math-modules.txt";
        fs::write(
            f.review.join(MANIFEST),
            format!("Defs.lean\n{ANCHOR}\n{MODULE}\n"),
        )?;
        let policy = f.review.join(POLICY);
        let mut bytes =
            serde_json::to_vec(&json!([{"target_module": MODULE, "declaration": THEOREM}]))?;
        bytes.push(b'\n');
        fs::write(&policy, &bytes)?;
        git(&f.review, &["add", "--", MODULE, MANIFEST, POLICY])?;
        git(
            &f.review,
            &["commit", "-m", "review resident T2 release policy"],
        )?;
        git(&f.review, &["push", "origin", "HEAD:refs/heads/main"])?;
        let reviewed = f.main()?;

        let bin = f.lean.parent().context("bundled Lean bin directory")?;
        let mut config = json!({
            "version": 2,
            "cli_executable": f.cli,
            "bare_repo": f.bare,
            "anchor_module": ANCHOR,
            "reviewed_main": reviewed,
            "native_executable": f.lean,
            "interval_ms": 1000,
            "audit_timeout_ms": 90_000,
            "max_runs": 1,
            "policy_sha256": file_sha256(&policy)?,
            "compiler_sha256": file_sha256(&f.lean)?,
            "exporter_sha256": file_sha256(&bin.join("leanexport"))?,
            "checker_sha256": file_sha256(&bin.join("nanoda_bin"))?,
        });
        // Keep a connection open so the WAL can be compared across daemon shutdown.
        let mut store = StateStore::open(f.database())?;
        store.create_claim(
            NewClaim {
                id: CLAIM,
                title: "Unverified resident fixture identity",
                informal_statement: "The independent audit is not a promotion.",
                engine: "lean4",
                formal_target: Some(THEOREM),
                module_path: Some(MODULE),
                critical: true,
            },
            Change {
                actor: "native-resident-release-fixture",
                git_commit: Some(&reviewed),
            },
        )?;
        let seq = store.status()?.last_event_seq;
        let eligible = f.claim(CLAIM)?;
        assert_eq!(eligible.local_trust, Trust::L0);
        assert!(eligible.receipt_id.is_none());
        let claims = [
            serde_json::to_value(eligible)?,
            serde_json::to_value(f.claim("C")?)?,
        ];
        let receipts = f.receipt_count()?;
        let wal = f.root.join(".runtime/state.sqlite-wal");
        let before = (fs::read(f.database())?, fs::read(&wal)?);
        let refs = git(&f.bare, &["show-ref"])?;
        let assert_unchanged = || -> Result<()> {
            assert_eq!(store.status()?.last_event_seq, seq);
            assert_eq!(f.receipt_count()?, receipts);
            assert_eq!(serde_json::to_value(f.claim(CLAIM)?)?, claims[0]);
            assert_eq!(serde_json::to_value(f.claim("C")?)?, claims[1]);
            assert_eq!((fs::read(f.database())?, fs::read(&wal)?), before);
            assert_eq!(git(&f.bare, &["show-ref"])?, refs);
            Ok(())
        };
        let run = |filename: &str, body: &Value| -> Result<(Daemon, Value)> {
            let path = f.root.join(filename);
            write_new(&path, 0o600, &serde_json::to_vec(body)?)?;
            let mut daemon = Daemon::spawn_with_flag(&f.root, Some(&path), "--t2-audit")?;
            ready(&mut daemon, &f.root)?;
            let live = daemon.poll(Duration::from_secs(95), "resident T2 attempt", || {
                let live = status(&f.root)?;
                Ok((live["t2_audit"]["exhausted"] == true).then_some(live))
            })?;
            assert_eq!(live["event_seq"], seq);
            Ok((daemon, live["t2_audit"].clone()))
        };

        let (daemon, sample) = run("resident-release.json", &config)?;
        let compile_ms = sample["observation"]["compile_ms"]
            .as_u64()
            .with_context(|| {
                format!(
                    "missing native independent observation: {sample}; {}",
                    daemon.logs()
                )
            })?;
        assert!(compile_ms <= 120_000);
        // Exact public allowlist: neither subprocess pin/digests nor target,
        // proof, or diagnostic strings may escape into daemon status.
        assert_eq!(
            sample,
            json!({
                "status": "observation", "attempts": 1, "exhausted": true,
                "observation": {
                    "schema": "ai4math.t2.resident-sample.v2", "advisory_only": true,
                    "reviewed_commit": reviewed, "ledger_event_seq": seq,
                    "compile_ms": compile_ms, "module_count": 3,
                    "debt_findings": 0, "independent_checks": 1,
                }
            })
        );
        let mut stream = request(
            &f.root,
            "subscribe",
            json!({"topics": ["vital_signs.tick"]}),
        )?;
        let reply = frame(&mut stream)?;
        assert_eq!(reply["id"], 1);
        assert_eq!(reply["result"]["topics"], json!(["vital_signs.tick"]));
        let tick = frame(&mut stream)?;
        assert_eq!(tick["method"], "vital_signs.tick");
        assert_eq!(tick["params"]["event_seq"], seq);
        assert_eq!(tick["params"]["t2_audit"], sample);
        drop(stream);
        f.stop(daemon)?;
        assert_unchanged()?;

        let pin = config["policy_sha256"]
            .as_str()
            .context("fixture policy pin")?;
        let replacement = if pin.starts_with('0') { '1' } else { '0' };
        config["policy_sha256"] = json!(format!("{replacement}{}", &pin[1..]));
        let (daemon, unavailable) = run("resident-release-wrong-pin.json", &config)?;
        assert_eq!(
            unavailable,
            json!({"status":"unavailable", "attempts":1, "exhausted":true})
        );
        f.stop(daemon)?;
        assert_unchanged()?;
        Ok(())
    }

    #[test]
    #[ignore = "requires outer-unsandboxed macOS Seatbelt and absolute AI4MATH_TEST_LEAN and AI4MATH_TEST_CLI binaries"]
    fn native_follow_autosweep_requires_approval_per_forward_main() -> Result<()> {
        let tmp = short_tempdir()?;
        let f = Fixture::new(
            tmp.path().canonicalize()?,
            &native_binary("AI4MATH_TEST_CLI")?,
            native_binary("AI4MATH_TEST_LEAN")?,
        )?;
        let base = f.main()?;
        let config = f.root.join("follow.json");
        write_new(
            &config,
            0o600,
            &serde_json::to_vec(&json!({
                "version":3,"selection":"applied_current_main","cli_executable":f.cli,"bare_repo":f.bare,
                "native_executable":f.lean,"page_limit":1,"interval_ms":200,"step_timeout_ms":60_000
            }))?,
        )?;
        let mut daemon = Daemon::spawn(&f.root, Some(&config))?;
        ready(&mut daemon, &f.root)?;
        let first = f.review_defs("Nat.succ 1")?;
        let preview1 = f.preview(&base, &first)?;
        let hash1 = digest(&preview1)?;
        assert!(f.state()?.applied_t3_plan(hash1)?.is_none());
        assert_eq!(f.receipt_count()?, 1);
        f.apply(&base, &first, hash1)?;
        let completed1 = daemon.poll(Duration::from_secs(35), "first reviewed T3 sweep", || {
            let progress = f.checkpoint(hash1)?;
            Ok((progress.status == T3SweepState::Complete).then_some(progress))
        })?;
        assert_eq!(completed1.reviewed_commit, first);
        assert_eq!(f.receipt_count()?, 2);
        assert_eq!(f.claim("C")?.local_trust, Trust::L5);

        let second = f.review_defs("1 + 1")?;
        let preview2 = f.preview(&first, &second)?;
        let hash2 = digest(&preview2)?;
        assert_ne!(hash2, hash1);
        assert!(f.state()?.applied_t3_plan(hash2)?.is_none());
        assert_eq!(f.receipt_count()?, 2);
        f.apply(&first, &second, hash2)?;
        let completed2 =
            daemon.poll(Duration::from_secs(35), "second reviewed T3 sweep", || {
                let progress = f.checkpoint(hash2)?;
                Ok((progress.status == T3SweepState::Complete).then_some(progress))
            })?;
        assert_eq!(completed2.reviewed_commit, second);
        assert_eq!(f.checkpoint(hash1)?, completed1);
        assert_eq!(f.receipt_count()?, 3);
        assert!(!f.claim("C")?.stale);
        let seq = f.state()?.status()?.last_event_seq;

        git(&f.bare, &["update-ref", "refs/heads/main", &first, &second])?;
        let log = daemon.log.path().to_path_buf();
        daemon.poll(Duration::from_secs(12), "rejected main rollback", || {
            Ok(fs::read_to_string(&log)?
                .contains("reviewed main did not advance by fast-forward")
                .then_some(()))
        })?;
        assert_eq!(f.checkpoint(hash2)?, completed2);
        assert_eq!(f.state()?.status()?.last_event_seq, seq);
        assert_eq!(f.receipt_count()?, 3);
        assert_eq!(status(&f.root)?["event_seq"], seq);
        f.stop(daemon)?;
        Ok(())
    }

    #[test]
    #[ignore = "requires outer-unsandboxed macOS Seatbelt and absolute AI4MATH_TEST_LEAN and AI4MATH_TEST_CLI binaries"]
    fn native_multi_autosweep_does_not_duplicate_proof_evidence() -> Result<()> {
        let tmp = short_tempdir()?;
        let f = Fixture::new(
            tmp.path().canonicalize()?,
            &native_binary("AI4MATH_TEST_CLI")?,
            native_binary("AI4MATH_TEST_LEAN")?,
        )?;
        let promoted = f.main()?;
        let reviewed = f.review_defs("Nat.succ 1")?;
        let before = f.claim("C")?;
        let main = f.preview(&promoted, &reviewed)?;
        let first = digest(&main)?;
        f.apply(&promoted, &reviewed, first)?;
        // The second approval uses a different validated anchor but the same
        // reviewed main. Both worklists include the original proof Claim.
        let other: Value = serde_json::from_str(&f.run(
            "t3-preview",
            &[utf8(&f.bare), "Defs.lean", &promoted, &reviewed],
        )?)?;
        let second = digest(&other)?;
        assert_ne!(first, second);
        f.run(
            "t3-apply",
            &[utf8(&f.bare), "Defs.lean", &promoted, &reviewed, second],
        )?;
        assert!(f.claim("C")?.stale);
        assert_eq!(f.receipt_count()?, 1);
        let seq = f.state()?.status()?.last_event_seq;
        let unapproved = "e".repeat(64);
        let config = f.root.join("multi.json");
        let mut body = catalog(&f.cli, &f.bare, &f.lean, first, 100);
        body.as_object_mut()
            .expect("catalog is an object")
            .remove("plan_sha256");
        body["version"] = json!(2);
        body["plan_sha256s"] = json!([first, unapproved, second]);
        body["step_timeout_ms"] = json!(60_000);
        write_new(&config, 0o600, &serde_json::to_vec(&body)?)?;
        let mut daemon = Daemon::spawn(&f.root, Some(&config))?;
        ready(&mut daemon, &f.root)?;
        let checkpoints = daemon.poll(
            Duration::from_secs(70),
            "both approved v2 checkpoints",
            || {
                let first = f.checkpoint(first)?;
                let second = f.checkpoint(second)?;
                Ok((first.status == T3SweepState::Complete
                    && second.status == T3SweepState::Complete)
                    .then_some([first, second]))
            },
        )?;
        assert!(
            checkpoints
                .iter()
                .all(|c| c.revision.is_some_and(|r| r >= 2))
        );
        let claim = f.claim("C")?;
        assert!(!claim.stale);
        assert_eq!(claim.effective_trust, Trust::L5);
        assert_ne!(before.receipt_id, claim.receipt_id);
        assert_eq!(f.receipt_count()?, 2);
        let state = f.state()?;
        assert_eq!(
            state
                .events_after(seq, 32)?
                .iter()
                .filter(|event| event.event_type == "RECEIPT_RECORDED")
                .count(),
            1
        );
        assert!(state.applied_t3_plan(&unapproved)?.is_none());
        assert_eq!(f.main()?, reviewed);
        f.stop(daemon)?;
        Ok(())
    }

    #[test]
    #[ignore = "requires outer-unsandboxed macOS Seatbelt and absolute AI4MATH_TEST_LEAN and AI4MATH_TEST_CLI binaries"]
    fn native_autosweep_is_opt_in_resumes_and_latches_only_approved_work() -> Result<()> {
        let tmp = short_tempdir()?;
        let f = Fixture::new(
            tmp.path().canonicalize()?,
            &native_binary("AI4MATH_TEST_CLI")?,
            native_binary("AI4MATH_TEST_LEAN")?,
        )?;
        let promoted = f.main()?;
        let original_source = git(&f.bare, &["show", "refs/heads/main:Proof.lean"])?;
        assert!(original_source.contains("theorem target : x = 2"));
        assert!(original_source.contains("rfl"));
        let original = f.claim("C")?;
        assert_eq!(original.local_trust, Trust::L5);
        assert!(original.receipt_id.is_some());
        assert_eq!(f.receipt_count()?, 1);
        let reviewed = f.review_defs("Nat.succ 1")?;
        let preview = f.preview(&promoted, &reviewed)?;
        let hash = digest(&preview)?;
        assert_eq!(preview["changed_modules"], json!(["Defs.lean"]));
        assert_eq!(preview["claims"][0]["claim_id"], "A");
        assert_eq!(preview["claims"][1]["claim_id"], "C");
        assert_eq!(
            preview["claims"][1]["old_proof"]["ready"]["promoted_commit"],
            promoted
        );
        let config = f.config("autosweep.json", hash)?;

        // A valid catalog and matching Git snapshot are not operator approval. Once
        // the worker stops, applying that very digest must not re-arm the old process.
        let mut unapproved = Daemon::spawn(&f.root, Some(&config))?;
        ready(&mut unapproved, &f.root)?;
        latched(&mut unapproved, "authorization or state unavailable")?;
        assert!(f.state()?.applied_t3_plan(hash)?.is_none());
        assert_eq!(f.receipt_count()?, 1);
        assert_eq!(f.claim("C")?.receipt_id, original.receipt_id);
        f.apply(&promoted, &reviewed, hash)?;
        thread::sleep(Duration::from_millis(2 * INTERVAL_MS));
        assert_eq!(f.checkpoint(hash)?.status, T3SweepState::NotStarted);
        assert!(f.claim("C")?.stale);
        status(&f.root)?;
        f.stop(unapproved)?;

        // A claim declared in the affected module after approval is not in the plan.
        f.declare("Z", "Defs.lean", "marker")?;
        let late_claim = serde_json::to_value(f.claim("Z")?)?;
        let seq = f.state()?.status()?.last_event_seq;
        let mut disabled = Daemon::spawn(&f.root, None)?;
        ready(&mut disabled, &f.root)?;
        assert_eq!(
            rpc(
                &f.root,
                "t3.sweep_step",
                json!({"plan_sha256":hash,"restart":true})
            )?["error"]["code"],
            -32601
        );
        thread::sleep(Duration::from_millis(400));
        assert_eq!(status(&f.root)?["event_seq"], seq);
        assert_eq!(f.checkpoint(hash)?.status, T3SweepState::NotStarted);
        assert_eq!(f.receipt_count()?, 1);
        f.stop(disabled)?;

        // Page one crosses only the non-L5 Claim A. Stop between pages at an
        // observed durable checkpoint, not while guessing where a child is running.
        let mut enabled = Daemon::spawn(&f.root, Some(&config))?;
        ready(&mut enabled, &f.root)?;
        let partial = enabled.poll(STARTUP, "first T3 page checkpoint", || {
            let progress = f.checkpoint(hash)?;
            Ok(progress.revision.is_some().then_some(progress))
        })?;
        // Let the finished page's CLI reap and release its locks, comfortably
        // before the 3-second inter-page delay expires.
        thread::sleep(Duration::from_millis(150));
        f.stop(enabled)?;
        assert_eq!(partial.revision, Some(1));
        assert_eq!(partial.status, T3SweepState::Running);
        assert_eq!(partial.next_after_claim.as_deref(), Some("A"));
        assert_eq!(partial.checkpoint_event_seq, Some(seq));
        assert_eq!(f.checkpoint(hash)?, partial);
        assert!(f.claim("C")?.stale);
        assert_eq!(f.receipt_count()?, 1);

        let mut resumed = Daemon::spawn(&f.root, Some(&config))?;
        ready(&mut resumed, &f.root)?;
        latched(&mut resumed, "complete")?;
        let complete = f.checkpoint(hash)?;
        assert_eq!(complete.revision, Some(2));
        assert_eq!(complete.status, T3SweepState::Complete);
        assert_eq!(complete.reviewed_commit, reviewed);
        assert!(complete.next_after_claim.is_none());
        let restored = f.claim("C")?;
        assert_eq!(restored.local_trust, Trust::L5);
        assert_eq!(restored.effective_trust, Trust::L5);
        assert!(!restored.stale);
        assert_ne!(restored.receipt_id, original.receipt_id);
        let after = status(&f.root)?;
        assert_eq!(after["trust"]["L5"], 1);
        assert_eq!(after["event_seq"].as_i64(), complete.checkpoint_event_seq);
        assert_eq!(f.receipt_count()?, 2);
        let events = f.state()?.events_after(seq, 32)?;
        let receipts: Vec<_> = events
            .iter()
            .filter(|event| event.event_type == "RECEIPT_RECORDED")
            .collect();
        assert_eq!(receipts.len(), 1);
        assert_eq!(
            Some(receipts[0].entity_id.as_str()),
            restored.receipt_id.as_deref()
        );
        assert_eq!(receipts[0].git_commit.as_deref(), Some(reviewed.as_str()));
        assert_eq!(
            receipts[0].payload,
            json!({"claim_id":"C","tier":"T3_TAINT_REBUILD"})
        );
        assert_eq!(serde_json::to_value(f.claim("Z")?)?, late_claim);
        assert_eq!(f.main()?, reviewed);
        let source = git(&f.bare, &["show", "refs/heads/main:Proof.lean"])?;
        assert_eq!(source, original_source);
        f.stop(resumed)?;

        let mut completed_restart = Daemon::spawn(&f.root, Some(&config))?;
        ready(&mut completed_restart, &f.root)?;
        latched(&mut completed_restart, "complete")?;
        assert_eq!(status(&f.root)?["event_seq"], after["event_seq"]);
        assert_eq!(f.checkpoint(hash)?, complete);
        assert_eq!(f.receipt_count()?, 2);
        f.stop(completed_restart)?;

        // The stale-input case uses a *pending* approval, so completion cannot
        // hide an accidental replay after main moves to an unapproved Git commit.
        let reviewed_two = f.review_defs("1 + 1")?;
        let second = f.preview(&reviewed, &reviewed_two)?;
        let old_hash = digest(&second)?;
        f.apply(&reviewed, &reviewed_two, old_hash)?;
        let moved = f.review_defs("Nat.succ (Nat.succ 0)")?;
        let stale_config = f.config("stale.json", old_hash)?;
        let seq = f.state()?.status()?.last_event_seq;
        let mut stale = Daemon::spawn(&f.root, Some(&stale_config))?;
        ready(&mut stale, &f.root)?;
        // This digest is applied, so the CLI rejects the obsolete main under the
        // Git/native gates rather than the worker rejecting it during SQLite readback.
        latched(&mut stale, "step failed")?;
        assert_eq!(status(&f.root)?["event_seq"], seq);
        assert_eq!(f.main()?, moved);
        assert_eq!(f.checkpoint(old_hash)?.status, T3SweepState::NotStarted);
        let claim = f.claim("C")?;
        assert!(claim.stale);
        assert_eq!(
            claim.taint_root_commit.as_deref(),
            Some(reviewed_two.as_str())
        );
        assert_eq!(f.receipt_count()?, 2);
        Ok(())
    }
}
