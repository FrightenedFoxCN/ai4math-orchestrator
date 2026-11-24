use ai4math_store::StateStore;
use anyhow::{Result, ensure};
use rusqlite::{Connection, ErrorCode, TransactionBehavior, config::DbConfig};
use std::{fs, path::Path, process::Command, time::Duration};

const REPO: &str = "/fixture/repo";
const BASE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn rejected(path: &Path, repo: &str, seq: i64) -> anyhow::Error {
    StateStore::with_managed_generation_reservation::<()>(path, repo, seq, || {
        panic!("a refused reservation must not run the operation")
    })
    .unwrap_err()
}

#[test]
fn missing_or_uninitialized_ledger_is_not_created_or_migrated() -> Result<()> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("state.sqlite");
    assert!(
        rejected(&path, REPO, 0)
            .to_string()
            .contains("open existing")
    );
    assert_eq!(fs::read_dir(root.path())?.count(), 0);

    let db = Connection::open(&path)?;
    db.execute_batch("CREATE TABLE legacy(value TEXT)")?;
    drop(db);
    let before = fs::read(&path)?;
    assert!(
        rejected(&path, REPO, 0)
            .to_string()
            .contains("no such table")
    );
    assert_eq!(fs::read(&path)?, before);
    Ok(())
}

#[test]
fn writer_is_reserved_during_operation_and_released_on_return() -> Result<()> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("state.sqlite");
    let mut store = StateStore::open(&path)?;
    store.bind_task_repository(REPO, BASE)?;
    let seq = store.status()?.last_event_seq;
    let mut probe = Connection::open(&path)?;
    probe.busy_timeout(Duration::ZERO)?;

    let value = StateStore::with_managed_generation_reservation(&path, REPO, seq, || {
        assert_eq!(
            StateStore::open_readonly(&path)?.status()?.last_event_seq,
            seq
        );
        let err = probe
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap_err();
        assert_eq!(err.sqlite_error_code(), Some(ErrorCode::DatabaseBusy));
        Ok("operation result")
    })?;
    assert_eq!(value, "operation result");

    let lock = probe.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let err = rejected(&path, REPO, seq);
    assert_eq!(
        err.downcast_ref::<rusqlite::Error>()
            .and_then(rusqlite::Error::sqlite_error_code),
        Some(ErrorCode::DatabaseBusy)
    );
    drop(lock);

    let err = StateStore::with_managed_generation_reservation(&path, REPO, seq, || {
        Err::<(), _>(anyhow::anyhow!("operation failed"))
    })
    .unwrap_err();
    assert_eq!(err.to_string(), "operation failed");
    probe
        .transaction_with_behavior(TransactionBehavior::Immediate)?
        .rollback()?;
    Ok(())
}

fn git(repo: &Path, args: &[&str]) -> Result<()> {
    let output = Command::new("git")
        .arg("--git-dir")
        .arg(repo)
        .args(args)
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_WORK_TREE")
        .output()?;
    ensure!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

#[test]
fn binding_drift_and_pending_journals_leave_git_untouched() -> Result<()> {
    let root = tempfile::tempdir()?;
    let repo = root.path().canonicalize()?.join("repo.git");
    let repo_name = repo.to_str().unwrap();
    git(
        &repo,
        &["init", "--bare", "--template=", "--initial-branch=main"],
    )?;
    let head = repo.join("HEAD");
    let before = fs::read(&head)?;
    let path = root.path().join("state.sqlite");
    let mut store = StateStore::open(&path)?;
    let reject_git = |expected_repo, seq, reason| -> Result<()> {
        let mut ran = false;
        let err =
            StateStore::with_managed_generation_reservation(&path, expected_repo, seq, || {
                ran = true;
                git(&repo, &["symbolic-ref", "HEAD", "refs/heads/not-main"])
            })
            .unwrap_err();
        assert!(!ran);
        assert!(err.to_string().contains(reason), "{err:#}");
        assert_eq!(fs::read(&head)?, before);
        Ok(())
    };

    let old_seq = store.status()?.last_event_seq;
    reject_git(repo_name, old_seq, "no repository binding")?;
    store.bind_task_repository(repo_name, BASE)?;
    let seq = store.status()?.last_event_seq;
    reject_git(repo_name, old_seq, "generation changed")?;
    reject_git(REPO, seq, "generation changed")?;

    let fixture = Connection::open(&path)?;
    fixture.pragma_update(None, "foreign_keys", false)?;
    fixture.execute(
        "INSERT INTO promotion_intent VALUES (1,?1,'fixture','base','new','Spec.lean','foo','proof')",
        [repo_name],
    )?;
    assert_eq!(store.status()?.last_event_seq, seq);
    reject_git(repo_name, seq, "pending Git promotion/Pivot")?;
    fixture.execute("DELETE FROM promotion_intent", [])?;
    fixture.execute(
        "INSERT INTO pivot_resume_intent VALUES (1,'fixture','{}','new')",
        [],
    )?;
    assert_eq!(store.status()?.last_event_seq, seq);
    reject_git(repo_name, seq, "pending Git promotion/Pivot")?;
    Ok(())
}

#[test]
fn last_reservation_does_not_checkpoint_or_append_to_wal() -> Result<()> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("state.sqlite");
    let wal = root.path().join("state.sqlite-wal");
    let mut store = StateStore::open(&path)?;
    store.bind_task_repository(REPO, BASE)?;
    let stamp = (REPO.to_owned(), store.status()?.last_event_seq);

    // Keep a nonempty committed WAL on disk with no active SQLite connection.
    let keeper = Connection::open(&path)?;
    keeper.set_db_config(DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE, true)?;
    let seq = keeper.query_row("SELECT max(event_seq) FROM events", [], |row| {
        row.get::<_, i64>(0)
    })?;
    assert_eq!(seq, stamp.1);
    drop(store);
    drop(keeper);
    let before_db = fs::read(&path)?;
    let before_wal = fs::read(&wal)?;
    assert!(!before_wal.is_empty());

    StateStore::with_managed_generation_reservation(&path, REPO, stamp.1, || Ok(()))?;
    assert_eq!(fs::read(&path)?, before_db);
    assert_eq!(fs::read(&wal)?, before_wal);
    assert_eq!(
        StateStore::open_readonly(&path)?.managed_wiki_stamp()?,
        Some(stamp)
    );
    Ok(())
}
