use super::*;
use crate::state::{T3PlanApproval, T3SweepState};
use rusqlite::Connection;

const HEAD: &str = "cccccccccccccccccccccccccccccccccccccccc";
const OTHER_HEAD: &str = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";
const MODULE: &str = "Spec.lean";

fn approve(store: &mut StateStore, reviewed: &str, n: usize) -> Result<String> {
    let preview = store.preview_t3(&[], &[MODULE])?;
    let approval = T3PlanApproval {
        plan_sha256: format!("{n:064x}"),
        base_commit: "a".repeat(reviewed.len()),
        reviewed_commit: reviewed.into(),
        anchor_module: MODULE.into(),
        preview_event_seq: preview.event_seq,
    };
    assert_eq!(
        store
            .apply_t3_plan(&approval, &[MODULE])?
            .expect("new approval")
            .modules,
        [MODULE]
    );
    Ok(approval.plan_sha256)
}

#[test]
fn lists_sorted_exact_head_including_completed_without_mutation() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("state.sqlite");
    let mut store = StateStore::open(&path)?;
    let high = approve(&mut store, HEAD, 16)?;
    let other = approve(&mut store, OTHER_HEAD, 1)?;
    let low = approve(&mut store, HEAD, 2)?;
    let complete = store
        .t3_sweep_step(&high, 1, false, "fixture:follow", |_| {
            panic!("empty approval scope must not dispatch a claim")
        })?
        .progress;
    assert_eq!(complete.status, T3SweepState::Complete);
    let unstarted = store.t3_sweep_status(&low)?;
    assert_eq!(unstarted.status, T3SweepState::NotStarted);
    let seq = store.status()?.last_event_seq;
    let changes = store.db.total_changes();
    let expected = vec![low.clone(), high.clone()];
    assert_eq!(store.applied_t3_hashes_for_head(HEAD)?, expected);
    assert_eq!(store.db.total_changes(), changes);
    drop(store);

    let readonly = StateStore::open_readonly(&path)?;
    assert_eq!(readonly.applied_t3_hashes_for_head(HEAD)?, expected);
    assert_eq!(readonly.applied_t3_hashes_for_head(OTHER_HEAD)?, [other]);
    assert!(
        readonly
            .applied_t3_hashes_for_head(&"e".repeat(40))?
            .is_empty()
    );
    assert_eq!(readonly.status()?.last_event_seq, seq);
    assert_eq!(readonly.t3_sweep_status(&high)?, complete);
    assert_eq!(readonly.t3_sweep_status(&low)?, unstarted);
    Ok(())
}

#[test]
fn rejects_ninth_same_head_match_without_returning_a_partial_list() -> Result<()> {
    let mut store = StateStore::open(":memory:")?;
    let mut expected = Vec::new();
    for n in (1..=8).rev() {
        expected.push(approve(&mut store, HEAD, n)?);
    }
    expected.reverse();
    let other = approve(&mut store, OTHER_HEAD, 99)?;
    assert_eq!(store.applied_t3_hashes_for_head(HEAD)?, expected);
    approve(&mut store, HEAD, 9)?;
    let error = store.applied_t3_hashes_for_head(HEAD).unwrap_err();
    assert!(error.to_string().contains("more than 8 applied T3 plans"));
    assert_eq!(store.applied_t3_hashes_for_head(OTHER_HEAD)?, [other]);
    Ok(())
}

#[test]
fn rejects_invalid_oid_and_matching_digest() -> Result<()> {
    let mut store = StateStore::open(":memory:")?;
    for invalid in [
        "HEAD".to_owned(),
        "C".repeat(40),
        "g".repeat(40),
        "0".repeat(63),
        "0".repeat(40),
        "0".repeat(64),
    ] {
        assert!(store.applied_t3_hashes_for_head(&invalid).is_err());
    }
    let mut hash = approve(&mut store, HEAD, 1)?;
    approve(&mut store, HEAD, 2)?;
    for invalid in ["A".repeat(64), "a".repeat(63), "g".repeat(64)] {
        assert_eq!(
            store.db.execute(
                "UPDATE applied_t3_plans SET plan_sha256=?1 WHERE plan_sha256=?2",
                params![invalid, hash],
            )?,
            1
        );
        assert!(store.applied_t3_hashes_for_head(HEAD).is_err());
        hash = invalid;
    }
    store.db.execute(
        "UPDATE applied_t3_plans SET plan_sha256=CAST(plan_sha256 AS BLOB) WHERE reviewed_commit=?1",
        [HEAD],
    )?;
    assert!(store.applied_t3_hashes_for_head(HEAD).is_err());
    Ok(())
}

#[test]
fn readonly_legacy_absence_is_empty_but_malformed_table_is_an_error() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("legacy.sqlite");
    let writer = Connection::open(&path)?;
    writer.execute_batch("CREATE TABLE legacy_data (value TEXT)")?;
    let readonly = StateStore::open_readonly(&path)?;
    assert!(readonly.applied_t3_hashes_for_head(HEAD)?.is_empty());
    drop(readonly);

    writer.execute_batch("CREATE TABLE applied_t3_plans (reviewed_commit TEXT)")?;
    let readonly = StateStore::open_readonly(&path)?;
    assert!(readonly.applied_t3_hashes_for_head(HEAD).is_err());
    Ok(())
}
