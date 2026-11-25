#![cfg(unix)]
use ai4math_store::{StateStore, Trust};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdout, Command, Stdio},
    time::{Duration, Instant},
};

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let r = Command::new("/usr/bin/git")
        .current_dir(dir)
        .args([
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@localhost",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgSign=false",
        ])
        .args(args)
        .output()?;
    ensure!(
        r.status.success(),
        "Git: {}",
        String::from_utf8_lossy(&r.stderr)
    );
    Ok(String::from_utf8(r.stdout)?.trim().into())
}
struct Fixture {
    tmp: tempfile::TempDir,
    base: String,
    repo: PathBuf,
}
fn cli() -> Command {
    Command::new(env!("CARGO_BIN_EXE_ai4math"))
}
impl Fixture {
    fn new() -> Result<Self> {
        Self::with_statement("True")
    }
    fn with_statement(formal: &str) -> Result<Self> {
        let tmp = tempfile::tempdir()?;
        let root = tmp.path();
        let work = root.join("work");
        fs::create_dir(&work)?;
        git(&work, &["init", "--initial-branch=main"])?;
        fs::write(
            work.join("Proof.lean"),
            format!("-- author-only context\ntheorem target : {formal} := {{{{PROOF_BODY}}}}\n"),
        )?;
        git(&work, &["add", "."])?;
        git(&work, &["commit", "-m", "freeze target"])?;
        let base = git(&work, &["rev-parse", "HEAD"])?;
        git(root, &["clone", "--bare", "work", "repo.git"])?;
        let repo = root.join("repo.git");
        let f = Self { tmp, base, repo };
        for args in [
            vec!["init", f.root(), "lean4"],
            vec![
                "declare-claim",
                f.root(),
                "C",
                "Proof.lean",
                "target",
                "Title",
                "The statement is true.",
            ],
            vec!["task-bind", f.root(), f.bare(), "Proof.lean", &f.base],
            vec![
                "task-register",
                f.root(),
                f.bare(),
                "T",
                "infrastructure",
                "Proof.lean",
                "target",
            ],
        ] {
            let r = cli().args(&args).output()?;
            ensure!(
                r.status.success(),
                "{}: {}",
                args[0],
                String::from_utf8_lossy(&r.stderr)
            );
        }
        Ok(f)
    }
    fn root(&self) -> &str {
        self.tmp.path().to_str().unwrap()
    }
    fn bare(&self) -> &str {
        self.repo.to_str().unwrap()
    }
    fn db(&self) -> Result<StateStore> {
        StateStore::open_readonly(self.tmp.path().join(".runtime/state.sqlite"))
    }
    fn spawn(&self, seconds: &str) -> Result<Run> {
        self.spawn_as("prover_worker", seconds)
    }
    fn spawn_as(&self, role: &str, seconds: &str) -> Result<Run> {
        self.spawn_pipe(&[
            "task-run-model",
            self.root(),
            self.bare(),
            "T",
            "C",
            role,
            seconds,
        ])
    }
    fn approve(&self) -> Result<Value> {
        let r = cli()
            .args([
                "model-dispatch-approve",
                self.root(),
                self.bare(),
                "T",
                "C",
                "prover_worker",
                &self.base,
                "fixture-math-model",
                "8",
                "4",
            ])
            .output()?;
        ensure!(
            r.status.success(),
            "approval: {}",
            String::from_utf8_lossy(&r.stderr)
        );
        Ok(serde_json::from_slice(&r.stdout)?)
    }
    fn spawn_approved(&self, model: &str) -> Result<Run> {
        self.spawn_pipe(&[
            "task-run-approved-model",
            self.root(),
            self.bare(),
            "T",
            "C",
            "prover_worker",
            "8",
            model,
            "4",
        ])
    }
    fn spawn_pipe(&self, args: &[&str]) -> Result<Run> {
        let mut child = cli()
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let out = BufReader::new(child.stdout.take().unwrap());
        let mut run = Run {
            child,
            out: Some(out),
            packet: Value::Null,
        };
        let mut line = String::new();
        run.out.as_mut().unwrap().read_line(&mut line)?;
        run.packet =
            serde_json::from_str::<Value>(&line).context("expected one packet")?["packet"].clone();
        Ok(run)
    }
}
struct Run {
    child: Child,
    out: Option<BufReader<ChildStdout>>,
    packet: Value,
}
impl Drop for Run {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
impl Run {
    fn result(&self) -> Value {
        let p = &self.packet;
        json!({"task_id":p["task_id"],"claim_id":p["claim_id"],"role":p["role"],"base_commit":p["base_commit"],
         "module_path":p["module_path"],"target_decl":p["target_decl"],"proof_verified":false,
         "suggestion":{"kind":"proof_body","proof_body":"True.intro"}})
    }
    fn send(&mut self, result: &Value) -> Result<()> {
        writeln!(self.child.stdin.take().unwrap(), "{result}")?;
        Ok(())
    }
    fn wait(&mut self) -> Result<bool> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(s) = self.child.try_wait()? {
                return Ok(s.success());
            }
            ensure!(
                Instant::now() < deadline,
                "model coordinator failed to terminate"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
#[test]
fn approved_dispatch_is_one_shot_even_when_acknowledgement_is_lost() -> Result<()> {
    use ai4math_store::ModelDispatchState;
    let f = Fixture::new()?;
    assert_eq!(f.approve()?["state"], "approved");
    assert!(f.spawn("8").is_err(), "old pipe bypassed approval");
    assert!(f.spawn_approved("wrong-model").is_err());
    assert_eq!(
        f.db()?.get_model_dispatch("T")?.unwrap().state,
        ModelDispatchState::Approved
    );

    let mut run = f.spawn_approved("fixture-math-model")?;
    assert_eq!(run.packet["base_commit"], f.base);
    assert_eq!(
        f.db()?.get_model_dispatch("T")?.unwrap().state,
        ModelDispatchState::Dispatched
    );
    assert!(f.spawn_approved("fixture-math-model").is_err());
    drop(run.out.take()); // Corresponds to a host losing its acknowledgement channel.
    run.send(&run.result())?;
    assert!(!run.wait()?);
    assert!(f.db()?.task_candidate("T")?.is_some());
    assert!(f.spawn_approved("fixture-math-model").is_err());
    let status = cli()
        .args(["model-dispatch-reconcile", f.root(), "T"])
        .output()?;
    assert!(status.status.success());
    let report: Value = serde_json::from_slice(&status.stdout)?;
    assert_eq!(report["state"], "recorded");
    assert!(!String::from_utf8(status.stdout)?.contains("True.intro"));
    assert_eq!(f.db()?.claim("C")?.unwrap().local_trust, Trust::L0);
    Ok(())
}

#[test]
fn approved_dispatch_cancel_and_failed_worker_do_not_requeue() -> Result<()> {
    use ai4math_store::ModelDispatchState;
    let cancelled = Fixture::new()?;
    cancelled.approve()?;
    let r = cli()
        .args(["model-dispatch-cancel", cancelled.root(), "T"])
        .output()?;
    assert!(r.status.success());
    assert!(cancelled.spawn_approved("fixture-math-model").is_err());
    assert!(cancelled.spawn("8").is_err());
    let f = Fixture::new()?;
    f.approve()?;
    let mut run = f.spawn_approved("fixture-math-model")?;
    drop(run.child.stdin.take());
    assert!(!run.wait()?);
    assert!(f.db()?.task_candidate("T")?.is_none());
    assert_eq!(
        f.db()?.get_model_dispatch("T")?.unwrap().state,
        ModelDispatchState::Dispatched
    );
    assert!(f.spawn_approved("fixture-math-model").is_err());
    assert!(f.spawn("8").is_err());
    Ok(())
}

#[test]
fn wrong_identity_cannot_write_and_success_is_untrusted_and_one_shot() -> Result<()> {
    let f = Fixture::new()?;
    let seq = f.db()?.status()?.last_event_seq;
    let mut empty = f.spawn("8")?;
    drop(empty.child.stdin.take());
    assert!(!empty.wait()?);
    assert!(f.db()?.task_candidate("T")?.is_none());
    assert!(
        f.db()?
            .events_after(seq, 8)?
            .iter()
            .any(|e| e.event_type == "TASK_RUN_FAILED")
    );
    let mut run = f.spawn("8")?; // Re-eligible on explicit invocation, without waiting 8s.
    assert_eq!(run.packet["formal_header"], "theorem target : True");
    assert_eq!(run.packet.as_object().unwrap().len(), 11);
    assert!(!run.packet.to_string().contains("author-only context"));
    let mut invalid = run.result();
    invalid["base_commit"] = json!("b".repeat(40));
    run.send(&invalid)?;
    assert!(!run.wait()?);
    assert!(f.db()?.task_candidate("T")?.is_none());
    let mut run = f.spawn("8")?;
    run.send(&run.result())?;
    assert!(run.wait()?);
    let kept = serde_json::to_value(f.db()?.task_candidate("T")?.context("missing candidate")?)?;
    assert_eq!(kept["source"], "dsh_model");
    assert!(kept.get("executable_sha256").is_none());
    let db = f.db()?;
    assert_eq!(db.claim("C")?.unwrap().local_trust, Trust::L0);
    assert!(db.promotion_intent()?.is_none());
    assert_eq!(git(&f.repo, &["rev-parse", "refs/heads/main"])?, f.base);
    let events = db.events_after(seq, 16)?;
    assert_eq!(
        events
            .iter()
            .filter(|e| e.event_type == "TASK_COMPLETED")
            .count(),
        1
    );
    assert!(
        events
            .iter()
            .all(|e| !e.payload.to_string().contains("True.intro"))
    );
    assert!(f.spawn("5").is_err());
    Ok(())
}
#[test]
fn open_stdin_has_deadline_and_moved_main_cannot_record() -> Result<()> {
    let f = Fixture::new()?;
    let mut run = f.spawn("1")?;
    let result = run.result();
    writeln!(run.child.stdin.as_mut().unwrap(), "{result}")?;
    assert!(!run.wait()?); // A complete line on open stdin does not commit.
    assert!(f.db()?.task_candidate("T")?.is_none());
    let mut run = f.spawn("8")?;
    let work = f.tmp.path().join("work");
    git(&work, &["commit", "--allow-empty", "-m", "advance main"])?;
    git(&work, &["push", f.bare(), "HEAD:refs/heads/main"])?;
    run.send(&run.result())?;
    assert!(!run.wait()?);
    assert!(f.db()?.task_candidate("T")?.is_none());
    // A real module trip fences the live lease even without changing main.
    let mut run = f.spawn("8")?;
    let trip = cli()
        .args([
            "trip-breaker",
            f.root(),
            f.bare(),
            "during-model",
            "Proof.lean",
            "target",
            "pause the model fixture",
        ])
        .output()?;
    ensure!(
        trip.status.success(),
        "breaker: {}",
        String::from_utf8_lossy(&trip.stderr)
    );
    run.send(&run.result())?;
    assert!(!run.wait()?);
    assert!(f.db()?.task_candidate("T")?.is_none());
    assert!(f.spawn("5").is_err());
    Ok(())
}

#[test]
fn dsh_blind_review_is_not_promotion_authority() -> Result<()> {
    let f = Fixture::new()?;
    let mut host = f.spawn_as("independent_reviewer", "8")?;
    let mut reply = host.result();
    reply["suggestion"] = json!({"kind":"review", "verdict":"looks_consistent", "rationale":"Matches the informal statement."});
    host.send(&reply)?;
    assert!(host.wait()?);
    let result = cli()
        .args([
            "promote-task-result",
            f.root(),
            f.bare(),
            "T",
            "/not-a-native-engine",
        ])
        .output()?;
    assert!(!result.status.success());
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("only retained proof-body suggestions")
    );
    assert_eq!(git(&f.repo, &["rev-parse", "refs/heads/main"])?, f.base);
    assert_eq!(f.db()?.claim("C")?.unwrap().effective_trust, Trust::L0);
    assert!(f.db()?.promotion_intent()?.is_none());
    Ok(())
}

#[test]
fn retained_dsh_candidate_cannot_be_rebased_on_a_new_main() -> Result<()> {
    let f = Fixture::new()?;
    let mut host = f.spawn("8")?;
    host.send(&host.result())?;
    assert!(host.wait()?);
    let work = f.tmp.path().join("work");
    git(
        &work,
        &["commit", "--allow-empty", "-m", "new operator baseline"],
    )?;
    git(&work, &["push", f.bare(), "HEAD:refs/heads/main"])?;
    let new_head = git(&f.repo, &["rev-parse", "refs/heads/main"])?;
    assert_ne!(new_head, f.base);
    let promotion = cli()
        .args([
            "promote-task-result",
            f.root(),
            f.bare(),
            "T",
            "/not-a-native-engine",
        ])
        .output()?;
    assert!(!promotion.status.success());
    assert!(String::from_utf8_lossy(&promotion.stderr).contains("older Git snapshot"));
    assert_eq!(git(&f.repo, &["rev-parse", "refs/heads/main"])?, new_head);
    let db = f.db()?;
    assert_eq!(db.task_candidate("T")?.unwrap().base_commit, f.base);
    assert_eq!(db.claim("C")?.unwrap().effective_trust, Trust::L0);
    assert!(db.promotion_intent()?.is_none());
    Ok(())
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "requires native macOS Seatbelt and absolute AI4MATH_TEST_LEAN"]
fn native_dsh_model_candidate_requires_kernel_promotion() -> Result<()> {
    let lean =
        std::env::var("AI4MATH_TEST_LEAN").context("missing native Lean fixture executable")?;
    let promote = |f: &Fixture| {
        cli()
            .args(["promote-task-result", f.root(), f.bare(), "T", &lean])
            .output()
    };
    for (formal, succeeds) in [("False", false), ("True", true)] {
        let f = Fixture::with_statement(formal)?;
        let seq = f.db()?.status()?.last_event_seq;
        let mut host = f.spawn("8")?;
        assert_eq!(
            host.packet["formal_header"],
            format!("theorem target : {formal}")
        );
        host.send(&host.result())?; // Both get the same syntactically allowed True.intro candidate.
        assert!(host.wait()?);
        let db = f.db()?;
        assert_eq!(db.claim("C")?.unwrap().local_trust, Trust::L0);
        assert!(db.promotion_intent()?.is_none());
        assert_eq!(git(&f.repo, &["rev-parse", "refs/heads/main"])?, f.base);
        assert!(
            !db.events_after(seq, 64)?
                .iter()
                .any(|e| e.event_type == "RECEIPT_RECORDED")
        );
        let out = promote(&f)?;
        if succeeds {
            ensure!(
                out.status.success(),
                "native promotion: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        } else {
            assert!(!out.status.success());
            let log = String::from_utf8_lossy(&out.stderr);
            assert!(
                log.to_ascii_lowercase().contains("type mismatch"),
                "expected native Lean type rejection, got {log}"
            );
        }
        let head = git(&f.repo, &["rev-parse", "refs/heads/main"])?;
        assert_eq!(head != f.base, succeeds);
        let db = f.db()?;
        let events = db.events_after(seq, 64)?;
        for tag in ["RECEIPT_RECORDED", "PROMOTION_COMMITTED"] {
            assert_eq!(
                events.iter().filter(|e| e.event_type == tag).count(),
                usize::from(succeeds)
            );
        }
        assert_eq!(
            db.claim("C")?.unwrap().effective_trust,
            if succeeds { Trust::L5 } else { Trust::L0 }
        );
        assert!(db.promotion_intent()?.is_none());
        if succeeds {
            let replay = promote(&f)?;
            assert!(!replay.status.success());
            assert!(String::from_utf8_lossy(&replay.stderr).contains("older Git snapshot"));
            assert_eq!(git(&f.repo, &["rev-parse", "refs/heads/main"])?, head);
        }
    }
    Ok(())
}
