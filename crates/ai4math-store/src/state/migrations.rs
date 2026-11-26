//! State schema upgrades that preserve existing evidence rows and event sequences.
use anyhow::{Result, ensure};
use rusqlite::{Connection, TransactionBehavior};

pub(super) fn initialize_model_dispatch(db: &mut Connection) -> Result<()> {
    const PENDING: &str = "SELECT NOT EXISTS(SELECT 1 FROM state_migrations WHERE version=4)";
    const RESIDENT_PENDING: &str =
        "SELECT NOT EXISTS(SELECT 1 FROM state_migrations WHERE version=5)";
    if db.query_row(RESIDENT_PENDING, [], |r| r.get::<_, bool>(0))? {
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if tx.query_row(PENDING, [], |r| r.get::<_, bool>(0))? {
            tx.execute_batch(include_str!("../../../../schemas/model-dispatch.sql"))?;
            tx.execute("INSERT INTO state_migrations(version) VALUES(4)", [])?;
        }
        if tx.query_row(RESIDENT_PENDING, [], |r| r.get::<_, bool>(0))? {
            tx.execute_batch(include_str!(
                "../../../../schemas/model-dispatch-resident.sql"
            ))?;
            tx.execute("INSERT INTO state_migrations(version) VALUES(5)", [])?;
        }
        tx.commit()?;
    }
    Ok(())
}

pub(super) fn initialize_owner_archive_requests(db: &mut Connection) -> Result<()> {
    const PENDING: &str = "SELECT NOT EXISTS(SELECT 1 FROM state_migrations WHERE version=6)";
    if db.query_row(PENDING, [], |r| r.get::<_, bool>(0))? {
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if tx.query_row(PENDING, [], |r| r.get::<_, bool>(0))? {
            tx.execute_batch(include_str!(
                "../../../../schemas/owner-archive-requests.sql"
            ))?;
            tx.execute("INSERT INTO state_migrations(version) VALUES(6)", [])?;
        }
        tx.commit()?;
    }
    Ok(())
}

pub(super) fn initialize_protocol_verification(db: &mut Connection) -> Result<()> {
    const PENDING: &str = "SELECT NOT EXISTS(SELECT 1 FROM state_migrations WHERE version=7)";
    if db.query_row(PENDING, [], |r| r.get::<_, bool>(0))? {
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if tx.query_row(PENDING, [], |r| r.get::<_, bool>(0))? {
            tx.execute_batch(include_str!(
                "../../../../schemas/protocol-verification.sql"
            ))?;
            tx.execute("INSERT INTO state_migrations(version) VALUES(7)", [])?;
        }
        tx.commit()?;
    }
    Ok(())
}

pub(super) fn initialize_compute_jobs(db: &mut Connection) -> Result<()> {
    const PENDING: &str = "SELECT NOT EXISTS(SELECT 1 FROM state_migrations WHERE version=8)";
    if db.query_row(PENDING, [], |r| r.get::<_, bool>(0))? {
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if tx.query_row(PENDING, [], |r| r.get::<_, bool>(0))? {
            tx.execute_batch(include_str!("../../../../schemas/compute-jobs.sql"))?;
            tx.execute("INSERT INTO state_migrations(version) VALUES(8)", [])?;
        }
        tx.commit()?;
    }
    Ok(())
}

pub(super) fn pending(db: &Connection) -> rusqlite::Result<bool> {
    db.query_row(
        "SELECT NOT EXISTS(SELECT 1 FROM state_migrations WHERE version=2)",
        [],
        |r| r.get(0),
    )
}

/// Remove the implicit UNIQUE(reviewed_commit) constraint introduced before
/// plan digests and approved scopes could be scheduled independently. Keep
/// the table name and plan_sha256 foreign keys stable across the atomic copy.
/// A fresh DB already uses the new layout; marking this version is also safe.
pub(super) fn permit_distinct_same_head_t3_plans(db: &mut Connection) -> Result<()> {
    if !pending(db)? {
        return Ok(());
    }
    // SQLite cannot disable FK actions inside a transaction. This changes only
    // the private connection being initialized, before any caller can use it.
    db.pragma_update(None, "foreign_keys", false)?;
    let migrated = (|| -> Result<()> {
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if pending(&tx)? {
            tx.execute_batch("CREATE TABLE applied_t3_plans_v2 (
                plan_sha256 TEXT PRIMARY KEY,
                base_commit TEXT NOT NULL,
                reviewed_commit TEXT NOT NULL,
                anchor_module TEXT NOT NULL,
                preview_event_seq INTEGER NOT NULL
            );
            INSERT INTO applied_t3_plans_v2 SELECT plan_sha256,base_commit,reviewed_commit,anchor_module,preview_event_seq FROM applied_t3_plans;
            DROP TABLE applied_t3_plans;
            ALTER TABLE applied_t3_plans_v2 RENAME TO applied_t3_plans;
            CREATE INDEX idx_applied_t3_reviewed_commit ON applied_t3_plans(reviewed_commit);
            INSERT INTO state_migrations(version) VALUES(2);")?;
            for table in ["t3_sweep_progress", "lean_spec_rechecks"] {
                let mut check = tx.prepare(&format!("PRAGMA foreign_key_check({table})"))?;
                ensure!(
                    !check.exists([])?,
                    "T3 migration found a broken foreign-key reference in {table}"
                );
            }
        }
        tx.commit()?;
        Ok(())
    })();
    db.pragma_update(None, "foreign_keys", true)?;
    migrated
}
