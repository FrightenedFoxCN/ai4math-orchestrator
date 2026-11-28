#![cfg(unix)]
use ai4math_evidence::{OracleObservationEvidence, OracleOutcome};
use ai4math_store::{Change, NewClaim, NewTask, StateStore, TaskRunOutcome, TaskTrack};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::{fs::PermissionsExt, net::UnixStream},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const QUERY: &str = "1111111111111111111111111111111111111111111111111111111111111111";
const Z3: &str = "2222222222222222222222222222222222222222222222222222222222222222";
const CVC5: &str = "3333333333333333333333333333333333333333333333333333333333333333";

struct OracleEvidence {
    raw: Value,
    id: String,
}
impl OracleEvidence {
    fn new() -> Result<Self> {
        let mut raw = json!({"outcome":"no_counterexample_within_bounds"});
        let id = format!("{:x}", Sha256::digest(serde_json::to_vec(&raw)?));
        raw["observation_sha256"] = json!(id);
        Ok(Self { raw, id })
    }
}
unsafe impl OracleObservationEvidence for OracleEvidence {
    fn id(&self) -> &str {
        &self.id
    }
    fn query_sha256(&self) -> &str {
        QUERY
    }
    fn solver_sha256s(&self) -> [&str; 2] {
        [Z3, CVC5]
    }
    fn outcome(&self) -> OracleOutcome {
        OracleOutcome::NoCounterexampleWithinBounds
    }
    fn raw_json(&self) -> Result<Value> {
        Ok(self.raw.clone())
    }
}

struct Server(Child);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
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
    fn set_timeout(&self, timeout: Duration) -> Result<()> {
        self.stream.set_read_timeout(Some(timeout))?;
        self.reader.get_ref().set_read_timeout(Some(timeout))?;
        Ok(())
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
            let mut line = String::new();
            ensure!(
                self.reader.read_line(&mut line)? > 0,
                "daemon stream closed"
            );
            let message: Value = serde_json::from_str(&line)?;
            if message["id"] == id {
                return Ok(message);
            }
        }
        anyhow::bail!("no RPC response")
    }
    fn result(&mut self, method: &str, params: Value) -> Result<Value> {
        let mut response = self.call(method, params)?;
        ensure!(response.get("result").is_some(), "{method}: {response}");
        Ok(response["result"].take())
    }
    fn bind(&mut self, role: &str) -> Result<()> {
        self.result("session.bind", json!({"role":role}))?;
        Ok(())
    }
}

fn git(repo: &Path, args: &[&str], input: Option<&[u8]>) -> Result<Vec<u8>> {
    let mut child = None;
    for _ in 0..20 {
        let spawned = Command::new("/usr/bin/git")
            .arg("--git-dir")
            .arg(repo)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@localhost")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@localhost")
            .args(args)
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();
        match spawned {
            Ok(started) => {
                child = Some(started);
                break;
            }
            Err(error) if error.raw_os_error() == Some(35) => {
                thread::sleep(Duration::from_millis(250));
            }
            Err(error) => return Err(error).context("spawn Git fixture command"),
        }
    }
    let mut child = child.context("spawn Git fixture command after resource backoff")?;
    if let Some(bytes) = input {
        child
            .stdin
            .take()
            .context("piped Git stdin")?
            .write_all(bytes)?;
    }
    let output = child.wait_with_output()?;
    ensure!(
        output.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(output.stdout)
}

fn bare_with_files(path: &Path, files: &[(&str, &[u8])]) -> Result<String> {
    fs::create_dir_all(path)?;
    git(path, &["init", "--bare", "--initial-branch=main"], None)?;
    let mut entries = String::new();
    for (name, source) in files {
        let blob = git(path, &["hash-object", "-w", "--stdin"], Some(source))?;
        let blob = String::from_utf8(blob)?.trim().to_owned();
        entries.push_str(&format!("100644 blob {blob}\t{name}\n"));
    }
    let tree = git(path, &["mktree"], Some(entries.as_bytes()))?;
    let tree = String::from_utf8(tree)?.trim().to_owned();
    let commit = git(path, &["commit-tree", &tree, "-m", "initial"], None)?;
    let commit = String::from_utf8(commit)?.trim().to_owned();
    git(path, &["update-ref", "refs/heads/main", &commit], None)?;
    Ok(commit)
}

fn bare_with_source(path: &Path, source: &[u8]) -> Result<String> {
    bare_with_files(path, &[("Demo.lean", source)])
}

fn bare_with_module(path: &Path) -> Result<String> {
    bare_with_source(path, b"def demo : Nat := 0\n")
}

fn next_commit(repo: &Path, parent: &str) -> Result<String> {
    let tree = git(repo, &["rev-parse", &format!("{parent}^{{tree}}")], None)?;
    let tree = String::from_utf8(tree)?.trim().to_owned();
    let commit = git(
        repo,
        &["commit-tree", &tree, "-p", parent, "-m", "next"],
        None,
    )?;
    Ok(String::from_utf8(commit)?.trim().to_owned())
}

fn boot_with_native_timeout(
    project: &Path,
    cli: &Path,
    lean: &Path,
    timeout_secs: &str,
) -> Result<Server> {
    let project = project.to_str().context("UTF8 fixture")?;
    let cli = cli.to_str().context("UTF8 CLI")?;
    let lean = lean.to_str().context("UTF8 Lean")?;
    let mut last = None;
    for _ in 0..20 {
        let spawned = Command::new(env!("CARGO_BIN_EXE_ai4mathd"))
            .args([
                "--project",
                project,
                "--tick-ms",
                "150",
                "--rfc-cli",
                cli,
                "--rfc-lean",
                lean,
                "--rfc-timeout-secs",
                timeout_secs,
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .spawn();
        match spawned {
            Ok(child) => return Ok(Server(child)),
            Err(error) if error.raw_os_error() == Some(35) => {
                last = Some(error);
                thread::sleep(Duration::from_millis(250));
            }
            Err(error) => return Err(error).context("spawn native ai4mathd"),
        }
    }
    Err(last.unwrap()).context("spawn native ai4mathd after resource backoff")
}

fn boot_with_native(project: &Path, cli: &Path, lean: &Path) -> Result<Server> {
    boot_with_native_timeout(project, cli, lean, "10")
}

fn decode_hex(text: &str) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() / 2);
    for pair in text.as_bytes().as_chunks::<2>().0 {
        let digit = |b: u8| -> Result<u8> {
            match b {
                b'0'..=b'9' => Ok(b - b'0'),
                b'a'..=b'f' => Ok(b - b'a' + 10),
                _ => anyhow::bail!("invalid hex"),
            }
        };
        out.push((digit(pair[0])? << 4) | digit(pair[1])?);
    }
    Ok(out)
}

fn sign_promotion(project: &Path, key: &Path, message_hex: &str) -> Result<Vec<u8>> {
    let message = project.join("owner-message");
    fs::write(&message, decode_hex(message_hex)?)?;
    let status = Command::new("/usr/bin/ssh-keygen")
        .args(["-Y", "sign", "-f"])
        .arg(key)
        .args(["-I", "ai4math", "-n", "ai4math-promotion"])
        .arg(&message)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    ensure!(status.success(), "sign Owner promotion fixture");
    Ok(fs::read(project.join("owner-message.sig"))?)
}

fn connect(project: &Path, child: &mut Server) -> Result<Rpc> {
    let socket = project.join(".runtime/ai4mathd.sock");
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(stream) = UnixStream::connect(&socket) {
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

#[test]
#[ignore = "requires native macOS Seatbelt, real Lean 4.35.0 and a built ai4math CLI"]
fn native_rfc_freeze_and_complete_with_real_lean() -> Result<()> {
    let lean = PathBuf::from(
        std::env::var_os("AI4MATH_TEST_LEAN")
            .context("set AI4MATH_TEST_LEAN to the real Lean binary")?,
    );
    let version = Command::new(&lean).arg("--version").output()?;
    ensure!(version.status.success(), "Lean version query failed");
    ensure!(
        String::from_utf8_lossy(&version.stdout).contains("version 4.35.0,"),
        "native RFC acceptance requires Lean 4.35.0"
    );
    let cli = PathBuf::from(env!("CARGO_BIN_EXE_ai4mathd"))
        .parent()
        .context("daemon parent")?
        .join("ai4math");
    ensure!(
        cli.is_file(),
        "build ai4math before running native RFC acceptance"
    );

    let dir = tempfile::Builder::new()
        .prefix("a4rfc-native")
        .rand_bytes(4)
        .tempdir()?;
    let project = dir.path();
    let runtime = project.join(".runtime");
    fs::create_dir_all(&runtime)?;
    fs::write(runtime.join("primary_engine"), "lean4\n")?;
    let repo_path = project.join("repo.git");
    let head = bare_with_source(
        &repo_path,
        b"def demo : Nat := 0\ntheorem demo_eq : demo = 0 := rfl\n",
    )?;
    fs::write(repo_path.join("ai4math-promotion.lock"), "")?;
    fs::set_permissions(
        repo_path.join("ai4math-promotion.lock"),
        fs::Permissions::from_mode(0o600),
    )?;
    let repo = repo_path.canonicalize()?;
    let mut store = StateStore::open(runtime.join("state.sqlite"))?;
    store.bind_task_repository(repo.to_str().context("repo path")?, &head)?;
    store.create_claim(
        NewClaim {
            id: "C1",
            title: "Claim",
            informal_statement: "demo = 0",
            engine: "lean4",
            formal_target: Some("Demo.lean#demo"),
            module_path: Some("Demo.lean"),
            critical: false,
        },
        Change {
            actor: "test",
            git_commit: None,
        },
    )?;
    drop(store);

    let mut child =
        boot_with_native_timeout(project, &cli, &lean, "600").context("boot native rfc daemon")?;
    let socket = runtime.join("ai4mathd.sock");
    let mut chief = connect(project, &mut child)?;
    chief.bind("chief")?;
    let mut explorer = Rpc::new(UnixStream::connect(&socket)?)?;
    explorer.bind("explorer")?;
    let mut builder = Rpc::new(UnixStream::connect(&socket)?)?;
    builder.bind("builder")?;

    let draft = json!({
        "rfc_id":"RFC-NATIVE","requested_by_claim":"C1","engine":"lean4",
        "motivation":"Native acceptance fixture",
        "proposed_definitions":["demo"],"required_api_lemmas":["demo_eq"],
        "consumer_usage_sketch":"example : demo = 0 := demo_eq","smoke_test_criteria":["nonempty"],"status":"DRAFT"
    });
    let submitted = explorer.result(
        "rfc.submit",
        json!({"module_path":"Demo.lean","reviewed_commit":head,"draft":draft}),
    )?;
    let digest = submitted["rfc"]["draft_sha256"]
        .as_str()
        .context("digest")?;
    chief.result(
        "rfc.approve",
        json!({"rfc_id":"RFC-NATIVE","reviewed_commit":head,"draft_sha256":digest}),
    )?;
    builder.result(
        "rfc.smoke_start",
        json!({"rfc_id":"RFC-NATIVE","reviewed_commit":head,"draft_sha256":digest}),
    )?;
    let frozen = builder.result("rfc.freeze", json!({"rfc_id":"RFC-NATIVE","reviewed_commit":head,"draft_sha256":digest,"smoke_lemmas":["demo_eq"]}))?;
    assert_eq!(frozen["rfc"]["status"], "FROZEN_PROVING");
    assert!(frozen["rfc"]["smoke_receipt_sha256"].is_string());
    let completed = builder.result(
        "rfc.complete",
        json!({"rfc_id":"RFC-NATIVE","reviewed_commit":head,"draft_sha256":digest}),
    )?;
    assert_eq!(completed["rfc"]["status"], "COMPLETED");
    assert!(completed["rfc"]["completion_receipt_sha256"].is_string());
    Ok(())
}

#[test]
#[ignore = "requires native macOS Seatbelt, real Lean 4.35.0 and a built ai4math CLI"]
fn native_owner_promotion_execute_with_real_lean() -> Result<()> {
    let lean = PathBuf::from(
        std::env::var_os("AI4MATH_TEST_LEAN")
            .context("set AI4MATH_TEST_LEAN to the real Lean binary")?,
    );
    let version = Command::new(&lean).arg("--version").output()?;
    ensure!(version.status.success(), "Lean version query failed");
    ensure!(
        String::from_utf8_lossy(&version.stdout).contains("version 4.35.0,"),
        "native promotion acceptance requires Lean 4.35.0"
    );
    let cli = PathBuf::from(env!("CARGO_BIN_EXE_ai4mathd"))
        .parent()
        .context("daemon parent")?
        .join("ai4math");
    ensure!(
        cli.is_file(),
        "build ai4math before running native promotion acceptance"
    );

    let dir = tempfile::Builder::new()
        .prefix("a4promote-native")
        .rand_bytes(4)
        .tempdir()?;
    let project = dir.path();
    let runtime = project.join(".runtime");
    fs::create_dir_all(&runtime)?;
    fs::write(runtime.join("primary_engine"), "lean4\n")?;
    let repo_path = project.join("repo.git");
    let head = bare_with_files(
        &repo_path,
        &[
            (
                "Demo.lean",
                b"theorem demo : (2 : Nat) = 2 := {{PROOF_BODY}}\n",
            ),
            ("ai4math-modules.txt", b"Demo.lean\n"),
        ],
    )?;
    fs::write(repo_path.join("ai4math-promotion.lock"), "")?;
    fs::set_permissions(
        repo_path.join("ai4math-promotion.lock"),
        fs::Permissions::from_mode(0o600),
    )?;
    let repo = repo_path.canonicalize()?;
    let mut store = StateStore::open(runtime.join("state.sqlite"))?;
    store.bind_task_repository(repo.to_str().context("repo path")?, &head)?;
    store.create_claim(
        NewClaim {
            id: "C1",
            title: "Claim",
            informal_statement: "2 = 2",
            engine: "lean4",
            formal_target: Some("Demo.lean#demo"),
            module_path: Some("Demo.lean"),
            critical: false,
        },
        Change {
            actor: "test",
            git_commit: None,
        },
    )?;
    store.register_task(
        NewTask {
            id: "T1",
            track: TaskTrack::Infrastructure,
            engine: "lean4",
            module_path: "Demo.lean",
            target: "demo",
            dependencies: &[],
        },
        Change {
            actor: "test",
            git_commit: None,
        },
    )?;
    let lease = store
        .claim_task(
            Some(TaskTrack::Infrastructure),
            Duration::from_secs(60),
            Change {
                actor: "test",
                git_commit: None,
            },
        )?
        .context("lease")?;
    store.finish_task_run(
        &lease,
        TaskRunOutcome::ModelCandidate {
            base_commit: &head,
            claim_id: "C1",
            role: "prover_worker",
            result_json: r#"{"kind":"proof_body","proof_body":"by\n  rfl"}"#,
        },
        Change {
            actor: "test",
            git_commit: None,
        },
    )?;
    drop(store);

    let owner_key = project.join("owner-ed25519");
    let keygen = Command::new("/usr/bin/ssh-keygen")
        .args(["-t", "ed25519", "-N", "", "-f"])
        .arg(&owner_key)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    ensure!(keygen.success(), "generate Owner Ed25519 fixture key");
    fs::copy(
        project.join("owner-ed25519.pub"),
        runtime.join("owner-promotion.pub"),
    )?;
    fs::set_permissions(
        runtime.join("owner-promotion.pub"),
        fs::Permissions::from_mode(0o600),
    )?;

    let mut child =
        boot_with_native_timeout(project, &cli, &lean, "600").context("boot native rfc daemon")?;
    let mut owner = connect(project, &mut child)?;
    owner.set_timeout(Duration::from_secs(620))?;
    owner.bind("owner")?;
    let mut chief = Rpc::new(UnixStream::connect(runtime.join("ai4mathd.sock"))?)?;
    chief.bind("chief")?;
    let expires_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs()
        + 300;
    let approval = owner.result(
        "promotion.approval",
        json!({"task_id":"T1","request_id":"REQ-NATIVE","expires_unix":expires_unix}),
    )?;
    assert!(!approval.to_string().contains("rfl"));
    let signature = sign_promotion(
        project,
        &owner_key,
        approval["message_hex"].as_str().context("message hex")?,
    )?;
    let signature_hex: String = signature.iter().map(|b| format!("{b:02x}")).collect();
    let args = json!({"task_id":"T1","request_id":"REQ-NATIVE","expires_unix":expires_unix,"signature_hex":signature_hex});
    let executed = owner.result("promotion.execute", args.clone())?;
    assert_eq!(executed["accepted"], true);
    let commit = executed["promotion"]["commit"]
        .as_str()
        .context("promoted commit")?;
    assert_ne!(commit, head);
    assert_eq!(
        String::from_utf8(git(&repo_path, &["rev-parse", "refs/heads/main"], None)?)?.trim(),
        commit
    );
    let status = chief.result("promotion.status", json!({}))?;
    assert_eq!(status["status"]["state"], "recorded");
    assert_eq!(
        status["status"]["latest_recorded"]["promoted_commit"],
        commit
    );
    assert_eq!(owner.result("promotion.execute", args)?["replayed"], true);
    assert_eq!(
        String::from_utf8(git(&repo_path, &["rev-parse", "refs/heads/main"], None)?)?.trim(),
        commit
    );
    Ok(())
}

#[test]
#[ignore = "requires native macOS Seatbelt, real Lean 4.35.0 and a built ai4math CLI"]
fn native_formal_sketch_applies_l3_without_publishing() -> Result<()> {
    let lean = PathBuf::from(
        std::env::var_os("AI4MATH_TEST_LEAN")
            .context("set AI4MATH_TEST_LEAN to the real Lean binary")?,
    );
    let version = Command::new(&lean).arg("--version").output()?;
    ensure!(version.status.success(), "Lean version query failed");
    ensure!(
        String::from_utf8_lossy(&version.stdout).contains("version 4.35.0,"),
        "formal sketch acceptance requires Lean 4.35.0"
    );
    let cli = PathBuf::from(env!("CARGO_BIN_EXE_ai4mathd"))
        .parent()
        .context("daemon parent")?
        .join("ai4math");
    ensure!(
        cli.is_file(),
        "build ai4math before running formal sketch acceptance"
    );

    let dir = tempfile::Builder::new()
        .prefix("a4sketch-native")
        .rand_bytes(4)
        .tempdir()?;
    let project = dir.path();
    let runtime = project.join(".runtime");
    fs::create_dir_all(&runtime)?;
    fs::write(runtime.join("primary_engine"), "lean4\n")?;
    let repo_path = project.join("repo.git");
    let head = bare_with_files(
        &repo_path,
        &[
            ("ASpec.lean", b"def spec : Prop := True\n"),
            ("ZTarget.lean", b"theorem demo : True := {{PROOF_BODY}}\n"),
            ("ai4math-modules.txt", b"ASpec.lean\nZTarget.lean\n"),
        ],
    )?;
    fs::write(repo_path.join("ai4math-promotion.lock"), "")?;
    fs::set_permissions(
        repo_path.join("ai4math-promotion.lock"),
        fs::Permissions::from_mode(0o600),
    )?;
    let repo = repo_path.canonicalize()?;
    let mut store = StateStore::open(runtime.join("state.sqlite"))?;
    store.bind_task_repository(repo.to_str().context("repo path")?, &head)?;
    store.create_claim(
        NewClaim {
            id: "C1",
            title: "Claim",
            informal_statement: "True",
            engine: "lean4",
            formal_target: Some("ZTarget.lean#demo"),
            module_path: Some("ZTarget.lean"),
            critical: false,
        },
        Change {
            actor: "test",
            git_commit: None,
        },
    )?;
    let spec_prefix =
        ai4math_broker::lean_breaker_sources_sha256(&[("ASpec.lean", "def spec : Prop := True\n")]);
    store.bind_lean_spec(&ai4math_store::ReviewedLeanSpec {
        repo: repo.to_str().context("repo path")?.into(),
        claim_id: "C1".into(),
        approved_git_commit: head.clone(),
        spec_file: "ASpec.lean".into(),
        spec_decl: "spec".into(),
        spec_prefix_sha256: spec_prefix,
    })?;
    let approval = store.oracle_approval(
        repo.to_str().context("repo path")?,
        "C1",
        &head,
        QUERY,
        [Z3, CVC5],
    )?;
    let observation_id = store
        .record_oracle_observation(&approval, &OracleEvidence::new()?)?
        .id;
    store.register_task(
        NewTask {
            id: "T1",
            track: TaskTrack::Infrastructure,
            engine: "lean4",
            module_path: "ZTarget.lean",
            target: "demo",
            dependencies: &[],
        },
        Change {
            actor: "test",
            git_commit: None,
        },
    )?;
    let lease = store
        .claim_task(
            Some(TaskTrack::Infrastructure),
            Duration::from_secs(60),
            Change {
                actor: "test",
                git_commit: None,
            },
        )?
        .context("lease")?;
    store.finish_task_run(
        &lease,
        TaskRunOutcome::ModelCandidate {
            base_commit: &head,
            claim_id: "C1",
            role: "prover_worker",
            result_json: r#"{"kind":"proof_body","proof_body":"by\n  sorry"}"#,
        },
        Change {
            actor: "test",
            git_commit: None,
        },
    )?;
    drop(store);

    let mut child =
        boot_with_native_timeout(project, &cli, &lean, "600").context("boot native rfc daemon")?;
    let mut builder = connect(project, &mut child)?;
    builder.bind("builder")?;
    builder.set_timeout(Duration::from_secs(620))?;
    let result = builder.result(
        "claim.apply_formal_sketch",
        json!({"task_id":"T1","observation_id":observation_id}),
    )?;
    assert_eq!(result["claim"]["local_trust"], "L3");
    assert_eq!(result["proof_verified"], false);
    assert_eq!(
        String::from_utf8(git(&repo_path, &["rev-parse", "refs/heads/main"], None)?)?.trim(),
        head
    );
    Ok(())
}

#[test]
#[ignore = "requires native macOS Seatbelt, real Lean 4.35.0 and a built ai4math CLI"]
fn native_oracle_backed_applies_l4a_debt_without_publishing() -> Result<()> {
    let lean = PathBuf::from(
        std::env::var_os("AI4MATH_TEST_LEAN")
            .context("set AI4MATH_TEST_LEAN to the real Lean binary")?,
    );
    let version = Command::new(&lean).arg("--version").output()?;
    ensure!(version.status.success(), "Lean version query failed");
    ensure!(
        String::from_utf8_lossy(&version.stdout).contains("version 4.35.0,"),
        "Oracle-backed acceptance requires Lean 4.35.0"
    );
    let cli = PathBuf::from(env!("CARGO_BIN_EXE_ai4mathd"))
        .parent()
        .context("daemon parent")?
        .join("ai4math");
    ensure!(
        cli.is_file(),
        "build ai4math before running Oracle-backed acceptance"
    );

    let dir = tempfile::Builder::new()
        .prefix("a4oracle-native")
        .rand_bytes(4)
        .tempdir()?;
    let project = dir.path();
    let runtime = project.join(".runtime");
    fs::create_dir_all(&runtime)?;
    fs::write(runtime.join("primary_engine"), "lean4\n")?;
    let repo_path = project.join("repo.git");
    let head = bare_with_files(
        &repo_path,
        &[
            ("ASpec.lean", b"def spec : Prop := (2 : Nat) = 2\n"),
            (
                "ZTarget.lean",
                b"theorem demo : (2 : Nat) = 2 := {{PROOF_BODY}}\n",
            ),
            ("ai4math-modules.txt", b"ASpec.lean\nZTarget.lean\n"),
        ],
    )?;
    fs::write(repo_path.join("ai4math-promotion.lock"), "")?;
    fs::set_permissions(
        repo_path.join("ai4math-promotion.lock"),
        fs::Permissions::from_mode(0o600),
    )?;
    let repo = repo_path.canonicalize()?;
    let mut store = StateStore::open(runtime.join("state.sqlite"))?;
    store.bind_task_repository(repo.to_str().context("repo path")?, &head)?;
    store.create_claim(
        NewClaim {
            id: "C1",
            title: "Claim",
            informal_statement: "2 = 2",
            engine: "lean4",
            formal_target: Some("ZTarget.lean#demo"),
            module_path: Some("ZTarget.lean"),
            critical: false,
        },
        Change {
            actor: "test",
            git_commit: None,
        },
    )?;
    let spec_prefix = ai4math_broker::lean_breaker_sources_sha256(&[(
        "ASpec.lean",
        "def spec : Prop := (2 : Nat) = 2\n",
    )]);
    store.bind_lean_spec(&ai4math_store::ReviewedLeanSpec {
        repo: repo.to_str().context("repo path")?.into(),
        claim_id: "C1".into(),
        approved_git_commit: head.clone(),
        spec_file: "ASpec.lean".into(),
        spec_decl: "spec".into(),
        spec_prefix_sha256: spec_prefix,
    })?;
    let approval = store.oracle_approval(
        repo.to_str().context("repo path")?,
        "C1",
        &head,
        QUERY,
        [Z3, CVC5],
    )?;
    let observation_id = store
        .record_oracle_observation(&approval, &OracleEvidence::new()?)?
        .id;
    store.register_task(
        NewTask {
            id: "T1",
            track: TaskTrack::Infrastructure,
            engine: "lean4",
            module_path: "ZTarget.lean",
            target: "demo",
            dependencies: &[],
        },
        Change {
            actor: "test",
            git_commit: None,
        },
    )?;
    let lease = store
        .claim_task(
            Some(TaskTrack::Infrastructure),
            Duration::from_secs(60),
            Change {
                actor: "test",
                git_commit: None,
            },
        )?
        .context("lease")?;
    store.finish_task_run(
        &lease,
        TaskRunOutcome::ModelCandidate {
            base_commit: &head,
            claim_id: "C1",
            role: "prover_worker",
            result_json: r#"{"kind":"proof_body","proof_body":"by\n  native_decide"}"#,
        },
        Change {
            actor: "test",
            git_commit: None,
        },
    )?;
    drop(store);

    let mut child =
        boot_with_native_timeout(project, &cli, &lean, "600").context("boot native rfc daemon")?;
    let mut builder = connect(project, &mut child)?;
    builder.bind("builder")?;
    builder.set_timeout(Duration::from_secs(620))?;
    let result = builder.result(
        "claim.apply_oracle_backed",
        json!({"task_id":"T1","observation_id":observation_id}),
    )?;
    assert_eq!(result["claim"]["local_trust"], "L4a");
    assert_eq!(result["proof_verified"], false);
    assert_eq!(result["oracle_debt"], true);
    let db = rusqlite::Connection::open(runtime.join("state.sqlite"))?;
    let debt: bool = db.query_row(
        "SELECT oracle_debt FROM claims WHERE claim_id='C1'",
        [],
        |r| r.get(0),
    )?;
    assert!(debt);
    assert_eq!(
        String::from_utf8(git(&repo_path, &["rev-parse", "refs/heads/main"], None)?)?.trim(),
        head
    );
    Ok(())
}

#[test]
fn rfc_submit_and_approval_use_bound_git_and_principal_actors() -> Result<()> {
    let dir = tempfile::Builder::new()
        .prefix("a4rfc")
        .rand_bytes(4)
        .tempdir()?;
    let project = dir.path();
    let runtime = project.join(".runtime");
    fs::create_dir_all(&runtime)?;
    fs::write(runtime.join("primary_engine"), "lean4\n")?;
    let repo_path = project.join("repo.git");
    let head = bare_with_module(&repo_path)?;
    let next_head = next_commit(&repo_path, &head)?;
    fs::write(repo_path.join("ai4math-promotion.lock"), "")?;
    {
        fs::set_permissions(
            repo_path.join("ai4math-promotion.lock"),
            fs::Permissions::from_mode(0o600),
        )?;
    }
    let repo = repo_path.canonicalize()?;
    let mut store = StateStore::open(runtime.join("state.sqlite"))?;
    store.bind_task_repository(repo.to_str().context("repo path")?, &head)?;
    store.create_claim(
        NewClaim {
            id: "C1",
            title: "Claim",
            informal_statement: "Statement",
            engine: "lean4",
            formal_target: Some("Demo.lean#demo"),
            module_path: Some("Demo.lean"),
            critical: false,
        },
        Change {
            actor: "test",
            git_commit: None,
        },
    )?;
    let approval = store.oracle_approval(
        repo.to_str().context("repo path")?,
        "C1",
        &head,
        QUERY,
        [Z3, CVC5],
    )?;
    let oracle_id = store
        .record_oracle_observation(&approval, &OracleEvidence::new()?)?
        .id;
    drop(store);

    let owner_key = project.join("owner-ed25519");
    let keygen = Command::new("/usr/bin/ssh-keygen")
        .args(["-t", "ed25519", "-N", "", "-f"])
        .arg(&owner_key)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    ensure!(keygen.success(), "generate Owner Ed25519 fixture key");
    fs::copy(
        project.join("owner-ed25519.pub"),
        runtime.join("owner-promotion.pub"),
    )?;
    fs::set_permissions(
        runtime.join("owner-promotion.pub"),
        fs::Permissions::from_mode(0o600),
    )?;

    let fake_cli = project.join("fake-ai4math");
    let fake_lean = project.join("fake-lean");
    let argv_log = project.join("rfc-argv.txt");
    fs::write(
        &fake_cli,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nprintf '%s\\n' '{{\"rfc_id\":\"RFC-2\",\"status\":\"FROZEN_PROVING\",\"fixture\":true}}'\n",
            argv_log.display()
        ),
    )?;
    fs::write(&fake_lean, "#!/bin/sh\nexit 0\n")?;
    fs::set_permissions(&fake_cli, fs::Permissions::from_mode(0o700))?;
    fs::set_permissions(&fake_lean, fs::Permissions::from_mode(0o700))?;
    let mut child = boot_with_native(project, &fake_cli, &fake_lean)?;
    let socket = runtime.join("ai4mathd.sock");
    let mut chief = connect(project, &mut child)?;
    chief.bind("chief")?;
    let mut explorer = Rpc::new(UnixStream::connect(&socket)?)?;
    explorer.bind("explorer")?;
    let trust_args = json!({"claim_id":"C1","observation_id":oracle_id});
    let trusted = chief.result("claim.apply_oracle", trust_args.clone())?;
    assert_eq!(trusted["claim"]["local_trust"], "L1");
    assert_eq!(trusted["proof_verified"], false);
    assert_eq!(
        chief.result("claim.apply_oracle", trust_args)?["claim"]["local_trust"],
        "L1"
    );
    assert_eq!(
        explorer.call(
            "claim.apply_oracle",
            json!({"claim_id":"C1","observation_id":oracle_id})
        )?["error"]["code"],
        -32003
    );
    let mut builder = Rpc::new(UnixStream::connect(&socket)?)?;
    builder.bind("builder")?;
    let mut owner = Rpc::new(UnixStream::connect(&socket)?)?;
    owner.bind("owner")?;
    assert_eq!(chief.call("rfc.submit", json!({"module_path":"Demo.lean","reviewed_commit":head,"draft":{
        "rfc_id":"RFC-1","requested_by_claim":"C1","engine":"lean4","motivation":"Need a definition",
        "proposed_definitions":["def demo : Nat := 0"],"required_api_lemmas":[],
        "consumer_usage_sketch":"example : demo = 0 := rfl","smoke_test_criteria":["nonempty"],"status":"DRAFT"
    }}))?["error"]["code"], -32003);

    let submit = json!({"module_path":"Demo.lean","reviewed_commit":head,"draft":{
        "rfc_id":"RFC-1","requested_by_claim":"C1","engine":"lean4","motivation":"Need a definition",
        "proposed_definitions":["def demo : Nat := 0"],"required_api_lemmas":[],
        "consumer_usage_sketch":"example : demo = 0 := rfl","smoke_test_criteria":["nonempty"],"status":"DRAFT"
    }});
    let submitted = explorer.result("rfc.submit", submit.clone())?;
    assert_eq!(submitted["rfc"]["status"], "DRAFT");
    assert_eq!(
        explorer.result("rfc.submit", submit.clone())?["rfc"],
        submitted["rfc"]
    );
    let digest = submitted["rfc"]["draft_sha256"]
        .as_str()
        .context("draft digest")?;
    let approve = json!({"rfc_id":"RFC-1","reviewed_commit":head,"draft_sha256":digest});
    let approved = chief.result("rfc.approve", approve.clone())?;
    assert_eq!(approved["rfc"]["status"], "APPROVED");
    assert_eq!(
        chief.result("rfc.approve", approve)?["rfc"],
        approved["rfc"]
    );
    assert_eq!(
        chief.result("rfc.inspect", json!({"rfc_id":"RFC-1"}))?["rfc"],
        approved["rfc"]
    );

    let mut replacement_submit = submit;
    replacement_submit["draft"]["rfc_id"] = json!("RFC-2");
    let replacement = explorer.result("rfc.submit", replacement_submit)?;
    let replacement_digest = replacement["rfc"]["draft_sha256"]
        .as_str()
        .context("replacement digest")?;
    chief.result(
        "rfc.approve",
        json!({"rfc_id":"RFC-2","reviewed_commit":head,"draft_sha256":replacement_digest}),
    )?;
    let smoke = builder.result(
        "rfc.smoke_start",
        json!({"rfc_id":"RFC-2","reviewed_commit":head,"draft_sha256":replacement_digest}),
    )?;
    assert_eq!(smoke["rfc"]["status"], "BUILDING_SMOKE");
    let frozen = builder.result(
        "rfc.freeze",
        json!({"rfc_id":"RFC-2","reviewed_commit":head,"draft_sha256":replacement_digest,"smoke_lemmas":["demo_eq"]}),
    )?;
    assert_eq!(frozen["rfc"]["fixture"], true);
    let argv = fs::read_to_string(&argv_log)?;
    let argv_lines: Vec<&str> = argv.lines().collect();
    assert_eq!(
        argv_lines,
        [
            "infra-rfc-freeze",
            project.canonicalize()?.to_str().context("project path")?,
            repo.to_str().context("repo path")?,
            "RFC-2",
            head.as_str(),
            replacement_digest,
            fake_lean.to_str().context("lean path")?,
            "demo_eq",
        ]
    );
    assert_eq!(
        builder.call(
            "rfc.complete",
            json!({"rfc_id":"RFC-2","reviewed_commit":head,"draft_sha256":replacement_digest})
        )?["error"]["code"],
        -32002
    );
    let supersede = json!({"rfc_id":"RFC-1","reviewed_commit":head,"draft_sha256":digest,
        "replacement_rfc_id":"RFC-2","reason":"replacement reviewed"});
    let superseded = chief.result("rfc.supersede", supersede.clone())?;
    assert_eq!(superseded["rfc"]["status"], "SUPERSEDED");
    assert_eq!(
        chief.result("rfc.supersede", supersede)?["rfc"],
        superseded["rfc"]
    );

    let reject_submit = json!({"module_path":"Demo.lean","reviewed_commit":head,"draft":{
        "rfc_id":"RFC-3","requested_by_claim":"C1","engine":"lean4","motivation":"Need a definition",
        "proposed_definitions":["def demo : Nat := 0"],"required_api_lemmas":[],
        "consumer_usage_sketch":"example : demo = 0 := rfl","smoke_test_criteria":["nonempty"],"status":"DRAFT"
    }});
    let rejected = explorer.result("rfc.submit", reject_submit)?;
    let rejected_digest = rejected["rfc"]["draft_sha256"]
        .as_str()
        .context("rejected digest")?;
    let reject = json!({"rfc_id":"RFC-3","reviewed_commit":head,"draft_sha256":rejected_digest,"reason":"not needed"});
    let rejected_record = chief.result("rfc.reject", reject.clone())?;
    assert_eq!(rejected_record["rfc"]["status"], "REJECTED");
    assert_eq!(
        chief.result("rfc.reject", reject)?["rfc"],
        rejected_record["rfc"]
    );

    let task = json!({"task_id":"T1","track":"infrastructure","module_path":"Demo.lean","target_decl":"demo"});
    let registered = chief.result("task.register", task.clone())?;
    assert_eq!(registered["dispatched"], false);
    assert_eq!(chief.result("task.register", task)?, registered);
    assert_eq!(
        chief.result("task.status", json!({}))?["infrastructure"]["ready"],
        1
    );

    let mut prover = Rpc::new(UnixStream::connect(&socket)?)?;
    prover.bind("prover")?;
    let mut prover_two = Rpc::new(UnixStream::connect(&socket)?)?;
    prover_two.result(
        "session.bind",
        json!({"role":"prover","binding_id":"prover-2"}),
    )?;
    let mut reviewer = Rpc::new(UnixStream::connect(&socket)?)?;
    reviewer.bind("reviewer")?;
    let acquire = json!({"request_id":"WREQ-1","wall_seconds":60});
    let acquired = prover.result("worker.acquire", acquire.clone())?;
    let attempt = &acquired["attempt"];
    assert_eq!(attempt["task_id"], "T1");
    assert_eq!(attempt["status"], "RUNNING");
    assert!(attempt.get("fence").is_none());
    assert_eq!(
        prover.result("worker.acquire", acquire)?["attempt"],
        *attempt
    );
    let submit = json!({"attempt_id":attempt["attempt_id"],"suggestion":{"kind":"proof_body","proof_body":"by rfl"}});
    let submitted = prover.result("worker.submit", submit.clone())?;
    assert_eq!(submitted["attempt"]["status"], "SUBMITTED");
    assert_eq!(
        prover.result("worker.submit", submit)?["attempt"],
        submitted["attempt"]
    );
    let mut conflicting_submit = json!({"attempt_id":attempt["attempt_id"],"suggestion":{"kind":"proof_body","proof_body":"by simp"}});
    assert_eq!(
        prover.call("worker.submit", conflicting_submit.take())?["error"]["code"],
        -32005
    );
    assert_eq!(reviewer.call("worker.submit", json!({"attempt_id":attempt["attempt_id"],"suggestion":{"kind":"review","verdict":"needs_changes","rationale":"missing"}}))?["error"]["code"], -32003);
    assert_eq!(prover_two.call("worker.submit", json!({"attempt_id":attempt["attempt_id"],"suggestion":{"kind":"proof_body","proof_body":"by simp"}}))?["error"]["code"], -32003);

    chief.result("task.register", json!({"task_id":"T2","track":"infrastructure","module_path":"Demo.lean","target_decl":"demo"}))?;
    let review_attempt = reviewer.result(
        "worker.acquire",
        json!({"request_id":"WREQ-2","wall_seconds":60}),
    )?;
    assert_eq!(review_attempt["attempt"]["task_id"], "T2");
    let cancelled = reviewer.result(
        "worker.cancel",
        json!({"attempt_id":review_attempt["attempt"]["attempt_id"]}),
    )?;
    assert_eq!(cancelled["attempt"]["status"], "CANCELLED");
    assert_eq!(
        chief.result("task.status", json!({}))?["infrastructure"]["ready"],
        1
    );

    assert_eq!(
        chief.call(
            "claim.apply_formal_sketch",
            json!({"task_id":"T1","observation_id":oracle_id})
        )?["error"]["code"],
        -32003
    );
    let sketch = builder.result(
        "claim.apply_formal_sketch",
        json!({"task_id":"T1","observation_id":oracle_id}),
    )?;
    assert_eq!(sketch["fixture"], true);
    let sketch_argv = fs::read_to_string(&argv_log)?;
    assert_eq!(
        sketch_argv.lines().collect::<Vec<_>>(),
        [
            "formal-sketch-task",
            project.canonicalize()?.to_str().context("project path")?,
            repo.to_str().context("repo path")?,
            "T1",
            fake_lean.to_str().context("lean path")?,
            oracle_id.as_str(),
        ]
    );

    assert_eq!(
        chief.call(
            "claim.apply_oracle_backed",
            json!({"task_id":"T1","observation_id":oracle_id})
        )?["error"]["code"],
        -32003
    );
    let oracle_backed = builder.result(
        "claim.apply_oracle_backed",
        json!({"task_id":"T1","observation_id":oracle_id}),
    )?;
    assert_eq!(oracle_backed["fixture"], true);
    let oracle_argv = fs::read_to_string(&argv_log)?;
    assert_eq!(
        oracle_argv.lines().collect::<Vec<_>>(),
        [
            "oracle-backed-task",
            project.canonicalize()?.to_str().context("project path")?,
            repo.to_str().context("repo path")?,
            "T1",
            fake_lean.to_str().context("lean path")?,
            oracle_id.as_str(),
        ]
    );

    let preview = chief.result("promotion.preview", json!({"task_id":"T1"}))?["preview"].clone();
    assert_eq!(preview["proof_verified"], false);
    assert_eq!(preview["head_matches_base"], true);
    assert_eq!(preview["proof_body_bytes"], 6);
    assert!(preview.get("proof_body").is_none());
    assert!(!preview.to_string().contains("by rfl"));
    assert!(chief.result("promotion.status", json!({}))?["status"]["pending_intent"].is_null());

    let expires_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs()
        + 300;
    let approval_args =
        json!({"task_id":"T1","request_id":"REQ-EXEC-1","expires_unix":expires_unix});
    let approval_packet = owner.result("promotion.approval", approval_args.clone())?;
    assert!(!approval_packet.to_string().contains("by rfl"));
    let signature = sign_promotion(
        project,
        &owner_key,
        approval_packet["message_hex"]
            .as_str()
            .context("message hex")?,
    )?;
    let signature_hex: String = signature.iter().map(|b| format!("{b:02x}")).collect();
    let execute_args = json!({
        "task_id":"T1","request_id":"REQ-EXEC-1","expires_unix":expires_unix,"signature_hex":signature_hex,
    });
    assert_eq!(
        chief.call("promotion.execute", execute_args.clone())?["error"]["code"],
        -32003
    );
    let executed = owner.result("promotion.execute", execute_args.clone())?;
    assert_eq!(executed["accepted"], true);
    assert_eq!(executed["replayed"], false);
    let execute_argv = fs::read_to_string(&argv_log)?;
    assert_eq!(
        execute_argv.lines().collect::<Vec<_>>(),
        [
            "promote-task-result",
            project.canonicalize()?.to_str().context("project path")?,
            repo.to_str().context("repo path")?,
            "T1",
            fake_lean.to_str().context("lean path")?,
        ]
    );
    let replay = owner.result("promotion.execute", execute_args)?;
    assert_eq!(replay["accepted"], false);
    assert_eq!(replay["replayed"], true);
    assert_eq!(fs::read_to_string(&argv_log)?, execute_argv);

    let db = rusqlite::Connection::open(runtime.join("state.sqlite"))?;
    db.execute(
        "INSERT INTO promotion_intent(singleton,repo,claim_id,base_commit,new_commit,file_path,target_decl,proof_body)
         VALUES(1,?1,'C1',?2,?3,'Demo.lean','demo','TOP-SECRET')",
        rusqlite::params![repo.to_str().context("repo path")?, head, next_head],
    )?;
    let promotion_status = chief.result("promotion.status", json!({}))?;
    assert!(!promotion_status.to_string().contains("TOP-SECRET"));
    assert!(
        !promotion_status
            .to_string()
            .contains(repo.to_str().context("repo path")?)
    );
    assert_eq!(
        promotion_status["status"]["pending_intent"]["claim_id"],
        "C1"
    );
    assert_eq!(promotion_status["status"]["state"], "pending");
    assert_eq!(
        chief.call("promotion.recover", json!({}))?["error"]["code"],
        -32003
    );
    let recovered = owner.result("promotion.recover", json!({}))?;
    assert_eq!(recovered["recovery"]["fixture"], true);
    let recover_argv = fs::read_to_string(&argv_log)?;
    let recover_lines: Vec<&str> = recover_argv.lines().collect();
    assert_eq!(
        recover_lines,
        [
            "recover-promotion",
            project.canonicalize()?.to_str().context("project path")?,
            repo.to_str().context("repo path")?,
            fake_lean.to_str().context("lean path")?,
        ]
    );
    assert_eq!(promotion_status["status"]["recovery"]["kind"], "prepared");
    git(
        &repo_path,
        &["update-ref", "refs/heads/main", &next_head],
        None,
    )?;
    let published = chief.result("promotion.status", json!({}))?;
    assert_eq!(
        published["status"]["recovery"]["kind"],
        "published_unrecorded"
    );
    assert_eq!(published["status"]["recovery"]["automatic"], false);
    let actors: Vec<String> = {
        let mut stmt = db.prepare("SELECT DISTINCT actor FROM events WHERE event_type IN ('INFRA_RFC_SUBMITTED','INFRA_RFC_APPROVED','INFRA_RFC_SMOKE_STARTED','INFRA_RFC_REJECTED','INFRA_RFC_SUPERSEDED','TASK_REGISTERED','WORKER_ATTEMPT_ACQUIRED','WORKER_ATTEMPT_SUBMITTED','WORKER_ATTEMPT_CANCELLED') ORDER BY actor")?;
        stmt.query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?
    };
    assert_eq!(
        actors,
        [
            "agent:builder",
            "agent:chief",
            "agent:explorer",
            "agent:prover",
            "agent:reviewer"
        ]
    );
    let submissions: i64 = db.query_row(
        "SELECT count(*) FROM events WHERE event_type='INFRA_RFC_SUBMITTED' AND entity_id='RFC-1'",
        [],
        |r| r.get(0),
    )?;
    let approvals: i64 = db.query_row(
        "SELECT count(*) FROM events WHERE event_type='INFRA_RFC_APPROVED' AND entity_id='RFC-1'",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(submissions, 1);
    assert_eq!(approvals, 1);
    let tasks: i64 = db.query_row(
        "SELECT count(*) FROM events WHERE event_type='TASK_REGISTERED' AND entity_id='T1'",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(tasks, 1);
    Ok(())
}
