use super::*;
use crate::{managed_native, worker_dispatch::private_state_path};
use ai4math_store::Change;
use serde_json::json;
use std::{fs, path::PathBuf, process::Command};

const MODULE: &str = "Audit.lean";
const SOURCE: &str = "theorem audit : True := True.intro\n";
const MAIN: &str = "refs/heads/main";

pub(super) struct Fixture {
    _temp: tempfile::TempDir,
    pub(super) root: PathBuf,
    pub(super) bare: PathBuf,
    pub(super) initial: String,
    pub(super) next: String,
}

pub(super) fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let run = Command::new("/usr/bin/git")
        .current_dir(dir)
        .args([
            "-c",
            "user.name=Audit Fixture",
            "-c",
            "user.email=audit@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()?;
    ensure!(
        run.status.success(),
        "Git fixture failed: {}",
        String::from_utf8_lossy(&run.stderr)
    );
    Ok(String::from_utf8(run.stdout)?.trim().to_owned())
}

impl Fixture {
    pub(super) fn new() -> Result<Self> {
        Self::with_release_policy(None)
    }

    pub(super) fn with_release_policy(policy: Option<&[u8]>) -> Result<Self> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().canonicalize()?;
        let source = root.join("fixture");
        let bare = root.join("repo.git");
        crate::init(&root, Engine::Lean4)?;
        fs::create_dir(&source)?;
        git(&source, &["init", "--initial-branch=main", "--quiet"])?;
        fs::write(source.join(MODULE), SOURCE)?;
        git(&source, &["add", MODULE])?;
        if let Some(policy) = policy {
            fs::write(source.join("ai4math-t2-release.json"), policy)?;
            git(&source, &["add", "ai4math-t2-release.json"])?;
        }
        git(&source, &["commit", "-m", "audit fixture", "--quiet"])?;
        let initial = git(&source, &["rev-parse", "HEAD"])?;
        git(
            &source,
            &[
                "commit",
                "--allow-empty",
                "-m",
                "concurrent review",
                "--quiet",
            ],
        )?;
        let next = git(&source, &["rev-parse", "HEAD"])?;
        git(
            &root,
            &["clone", "--bare", "--quiet", "fixture", "repo.git"],
        )?;
        git(&bare, &["update-ref", MAIN, &initial, &next])?;
        let _lock = PromotionRepo::try_lock(&bare)?;
        let repo = bare.to_str().context("fixture repository path encoding")?;
        StateStore::open(private_state_path(&root)?)?.bind_task_repository(repo, &initial)?;
        Ok(Self {
            _temp: temp,
            root,
            bare,
            initial,
            next,
        })
    }

    pub(super) fn append_from_other_connection(&self) -> Result<i64> {
        StateStore::open(private_state_path(&self.root)?)?.append_event(
            Change {
                actor: "audit-concurrent-fixture",
                git_commit: None,
            },
            "NOTE_CREATED",
            "note",
            "unrelated",
            json!({}),
        )
    }

    fn run<T>(
        &self,
        snapshot: impl FnOnce(&StateStore, i64) -> Result<T>,
        compile: impl FnOnce(Engine, &[(&str, &str)]) -> Result<LeanBuildAudit>,
    ) -> Result<(T2BuildReport, T)> {
        audit_inner(
            &self.root,
            &self.bare,
            MODULE,
            &self.initial,
            snapshot,
            compile,
        )
    }
}

fn scan(state: &StateStore, stamp: i64) -> Result<TrustDebtScan> {
    let debt = state.scan_trust_debt()?;
    assert_eq!(debt.ledger_event_seq, stamp);
    Ok(debt)
}

fn fake_success(engine: Engine, modules: &[(&str, &str)]) -> LeanBuildAudit {
    assert_eq!(engine, Engine::Lean4);
    assert_eq!(modules, &[(MODULE, SOURCE)]);
    // A coordinator-only fixture: intentionally no native proof or authority.
    LeanBuildAudit {
        source_set_sha256: "a".repeat(64),
        compiler_sha256: "b".repeat(64),
        compile_ms: 0,
        modules: vec![],
    }
}

#[test]
fn ledger_move_after_debt_scan_prevents_compiler_launch() -> Result<()> {
    let f = Fixture::new()?;
    let result = f.run(
        |state, seq| {
            let debt = scan(state, seq)?;
            // This writer succeeds while the project Git/native guards are held:
            // unrelated events may advance the ledger without taking those locks.
            assert!(f.append_from_other_connection()? > seq);
            Ok(debt)
        },
        |_, _| panic!("pre-build generation drift dispatched a compiler"),
    );
    let error = result
        .err()
        .context("audit returned a partial or outdated report")?;
    assert!(
        error.to_string().contains("changed before T2 build"),
        "{error:#}"
    );
    Ok(())
}

#[test]
fn git_or_ledger_move_during_build_suppresses_both_reports() -> Result<()> {
    for move_git in [false, true] {
        let f = Fixture::new()?;
        let result = at_snapshot_generation_then(
            &f.root,
            &f.bare,
            MODULE,
            &f.initial,
            scan,
            |engine, _, modules| {
                let native = fake_success(engine, modules);
                if move_git {
                    git(&f.bare, &["update-ref", MAIN, &f.next, &f.initial])?;
                } else {
                    f.append_from_other_connection()?;
                }
                Ok(native)
            },
            |_, _, _, _, _| -> Result<()> { panic!("generation drift reached the finalizer") },
        );
        let error = result
            .err()
            .context("audit returned a partial or outdated report")?;
        assert!(
            error.to_string().contains("changed during T2 build"),
            "{error:#}"
        );
    }
    Ok(())
}

#[test]
fn finalizer_holds_both_gates_after_a_fresh_observation() -> Result<()> {
    let f = Fixture::new()?;
    let (_, _, seq, _, tag) = at_snapshot_generation_then(
        &f.root,
        &f.bare,
        MODULE,
        &f.initial,
        scan,
        |engine, _, modules| Ok(fake_success(engine, modules)),
        |git, path, seq, _, debt| {
            assert_eq!(git.path(), f.bare);
            assert_eq!(git.main_head()?, f.initial);
            assert_eq!(path, private_state_path(&f.root)?);
            assert_eq!(seq, debt.ledger_event_seq);
            assert!(PromotionRepo::try_lock_existing(&f.bare).is_err());
            assert!(managed_native(&f.root).is_err());
            // No fake tag or receipt: this only proves the finalizer boundary.
            Ok("called at fresh generation")
        },
    )?;
    assert!(seq > 0);
    assert_eq!(tag, "called at fresh generation");
    Ok(())
}
