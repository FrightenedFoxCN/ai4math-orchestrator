use super::*;
use std::cell::Cell;

const REPO: &str = "/repo";
const BASE: &str = "1111111111111111111111111111111111111111";
const NEW: &str = "2222222222222222222222222222222222222222";
const OTHER: &str = "3333333333333333333333333333333333333333";

fn trip(store: &mut StateStore, name: &str, base: &str) -> Result<()> {
    store.register_tripped_breaker(
        REPO,
        base,
        name,
        "Defs.name",
        "Defs",
        "lean4",
        "incident",
        false,
    )?;
    Ok(())
}

fn approval(
    store: &mut StateStore,
    name: &str,
    action: PivotAction,
    new_commit: &str,
) -> Result<(OwnerArchiveRequest, PivotIntent)> {
    let facts = store.briefing_facts()?;
    let review = RetainedReview {
        event_from_exclusive: facts.event_from_exclusive,
        event_to_inclusive: facts.event_to_inclusive,
        to_git_commit: facts.to_git_commit,
        checkpoint: facts.checkpoint,
        presentation_truncated: false,
        breakers: facts.breakers.items,
        inbox: facts.inbox.items,
        frontier_edges: facts.frontier.plans.items,
    };
    let plan = store.preview_pivot(
        REPO,
        review.to_git_commit.as_deref().unwrap(),
        &"4".repeat(64),
        &review,
        name,
        "reviewed",
        action,
    )?;
    let intent = PivotIntent {
        adr_id: plan.adr_id()?,
        plan,
        new_commit: new_commit.into(),
    };
    let request = OwnerArchiveRequest {
        request_id: format!("approval-{name}"),
        message_sha256: "5".repeat(64),
        public_key_fingerprint: "6".repeat(64),
        repo: REPO.into(),
        base_commit: intent.plan.base_commit.clone(),
        adr_id: intent.adr_id.clone(),
    };
    Ok((request, intent))
}

fn assert_request(
    store: &StateStore,
    request: &OwnerArchiveRequest,
    intent: &PivotIntent,
    status: OwnerArchiveRequestStatus,
    result: Option<PivotResult>,
) -> Result<()> {
    assert_eq!(
        store.owner_archive_request(&request.request_id)?,
        Some(OwnerArchiveRequestRecord {
            request: request.clone(),
            intent: intent.clone(),
            status,
            result
        })
    );
    Ok(())
}

#[test]
fn durable_status_replay_and_adr_collision() -> Result<()> {
    use OwnerArchiveRequestStatus::*;
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("state.sqlite");
    let mut store = StateStore::open(&path)?;
    trip(&mut store, "first", BASE)?;
    trip(&mut store, "second", BASE)?;
    let (request, intent) = approval(&mut store, "first", PivotAction::ArchiveBreaker, NEW)?;
    let (mut second, second_intent) =
        approval(&mut store, "second", PivotAction::ArchiveBreaker, OTHER)?;
    let start_seq = last_seq(&store.db)?;
    assert!(store.owner_archive_request(&request.request_id)?.is_none());
    store.prepare_owner_archive_pivot(&request, &intent)?;
    assert_eq!(last_seq(&store.db)?, start_seq);
    assert!(
        store
            .prepare_owner_archive_pivot(&request, &intent)
            .is_err()
    );
    let reader = StateStore::open_readonly(&path)?;
    assert_request(&reader, &request, &intent, Prepared, None)?;
    store.abandon_unadopted_pivot(&intent)?;
    assert!(store.pivot_intent()?.is_none());
    assert_eq!(last_seq(&store.db)?, start_seq);
    assert_request(&reader, &request, &intent, Abandoned, None)?;
    drop(store);
    let mut store = StateStore::open(&path)?;
    assert!(
        store
            .prepare_owner_archive_pivot(&request, &intent)
            .is_err()
    );
    assert!(store.prepare_pivot(&intent).is_err());
    let mut another_id = request.clone();
    another_id.request_id = "different-request-id".into();
    assert!(
        store
            .prepare_owner_archive_pivot(&another_id, &intent)
            .is_err()
    );
    assert!(
        store
            .owner_archive_request(&another_id.request_id)?
            .is_none()
    );
    let second_id = second.request_id.clone();
    second.request_id.clone_from(&request.request_id);
    assert!(
        store
            .prepare_owner_archive_pivot(&second, &second_intent)
            .is_err()
    );
    assert_request(&reader, &request, &intent, Abandoned, None)?;
    second.request_id = second_id;
    store.prepare_owner_archive_pivot(&second, &second_intent)?;
    let result = store.land_pivot(&second_intent, false, || Ok(()))?;
    assert_request(
        &reader,
        &second,
        &second_intent,
        Applied,
        Some(result.clone()),
    )?;
    assert!(store.pivot_intent()?.is_none());
    assert_eq!(
        store.recorded_pivot(&second.adr_id)?,
        Some((second_intent.plan.clone(), result))
    );
    assert!(
        store
            .prepare_owner_archive_pivot(&second, &second_intent)
            .is_err()
    );
    assert_request(&reader, &request, &intent, Abandoned, None)?;
    Ok(())
}

#[test]
fn rejects_mismatches_stale_context_and_metadata_overwrites() -> Result<()> {
    let mut store = StateStore::open(":memory:")?;
    trip(&mut store, "breaker", BASE)?;
    let (request, intent) = approval(&mut store, "breaker", PivotAction::ArchiveBreaker, NEW)?;
    for wrong in [
        OwnerArchiveRequest {
            repo: "/other".into(),
            ..request.clone()
        },
        OwnerArchiveRequest {
            repo: "/repo/".into(),
            ..request.clone()
        },
        OwnerArchiveRequest {
            base_commit: OTHER.into(),
            ..request.clone()
        },
        OwnerArchiveRequest {
            adr_id: "ADR-wrong".into(),
            ..request.clone()
        },
        OwnerArchiveRequest {
            public_key_fingerprint: "E".repeat(64),
            ..request.clone()
        },
        OwnerArchiveRequest {
            message_sha256: "not-a-digest".into(),
            ..request.clone()
        },
        OwnerArchiveRequest {
            request_id: "noncanonical.id".into(),
            ..request.clone()
        },
    ] {
        assert!(store.prepare_owner_archive_pivot(&wrong, &intent).is_err());
        assert!(store.owner_archive_request(&wrong.request_id)?.is_none());
        assert!(store.pivot_intent()?.is_none());
    }
    let (resume, resume_intent) = approval(&mut store, "breaker", PivotAction::ResumeAsIs, NEW)?;
    assert!(
        store
            .prepare_owner_archive_pivot(&resume, &resume_intent)
            .is_err()
    );
    let invalid_intent = PivotIntent {
        new_commit: BASE.into(),
        ..intent.clone()
    };
    assert!(
        store
            .prepare_owner_archive_pivot(&request, &invalid_intent)
            .is_err()
    );
    let mut unattested = intent.clone();
    unattested.plan.base_commit = OTHER.into();
    unattested.adr_id = unattested.plan.adr_id()?;
    let wrong_base = OwnerArchiveRequest {
        base_commit: OTHER.into(),
        adr_id: unattested.adr_id.clone(),
        ..request.clone()
    };
    assert!(
        store
            .prepare_owner_archive_pivot(&wrong_base, &unattested)
            .is_err()
    );
    assert!(store.owner_archive_request(&request.request_id)?.is_none());
    assert!(store.pivot_intent()?.is_none());
    trip(&mut store, "later", BASE)?;
    assert!(
        store
            .prepare_owner_archive_pivot(&request, &intent)
            .is_err()
    );
    assert!(store.owner_archive_request(&request.request_id)?.is_none());
    assert!(store.pivot_intent()?.is_none());
    let (request, intent) = approval(&mut store, "breaker", PivotAction::ArchiveBreaker, NEW)?;
    store.prepare_owner_archive_pivot(&request, &intent)?;
    let mut changed = request.clone();
    changed.message_sha256 = "7".repeat(64);
    assert!(
        store
            .prepare_owner_archive_pivot(&changed, &intent)
            .is_err()
    );
    changed = request.clone();
    changed.public_key_fingerprint = "8".repeat(64);
    assert!(
        store
            .prepare_owner_archive_pivot(&changed, &intent)
            .is_err()
    );
    for sql in [
        "UPDATE owner_archive_requests SET message_sha256=public_key_fingerprint",
        "INSERT OR REPLACE INTO owner_archive_requests SELECT * FROM owner_archive_requests",
        "DELETE FROM owner_archive_requests",
    ] {
        assert!(store.db.execute(sql, []).is_err());
    }
    let wrong_commit = PivotIntent {
        new_commit: OTHER.into(),
        ..intent.clone()
    };
    assert!(
        store
            .land_pivot(&wrong_commit, false, || panic!(
                "must not adopt a different commit"
            ))
            .is_err()
    );
    assert!(store.abandon_unadopted_pivot(&wrong_commit).is_err());
    assert_request(
        &store,
        &request,
        &intent,
        OwnerArchiveRequestStatus::Prepared,
        None,
    )
}

#[test]
fn prepare_land_and_abandon_rollback_together() -> Result<()> {
    use OwnerArchiveRequestStatus::*;
    let mut store = StateStore::open(":memory:")?;
    trip(&mut store, "abandon", BASE)?;
    trip(&mut store, "land", BASE)?;
    let (request, intent) = approval(&mut store, "abandon", PivotAction::ArchiveBreaker, NEW)?;
    let (land_request, land_intent) =
        approval(&mut store, "land", PivotAction::ArchiveBreaker, OTHER)?;
    let seq = last_seq(&store.db)?;
    assert!(
        store
            .prepare_owner_archive_pivot_with_check(&request, &intent, || {
                anyhow::bail!("expired under writer lock")
            })
            .unwrap_err()
            .to_string()
            .contains("expired under writer lock")
    );
    assert!(store.owner_archive_request(&request.request_id)?.is_none());
    assert!(store.pivot_intent()?.is_none());

    store.prepare_owner_archive_pivot(&request, &intent)?;
    store.db.execute_batch(
        "CREATE TRIGGER fail_remove BEFORE DELETE ON pivot_resume_intent
                             BEGIN SELECT RAISE(ABORT, 'injected intent removal failure'); END;",
    )?;
    assert!(store.abandon_unadopted_pivot(&intent).is_err());
    assert_request(&store, &request, &intent, Prepared, None)?;
    assert_eq!(store.pivot_intent()?, Some(intent.clone()));
    store.db.execute_batch("DROP TRIGGER fail_remove")?;
    store.abandon_unadopted_pivot(&intent)?;
    assert_request(&store, &request, &intent, Abandoned, None)?;

    store.prepare_owner_archive_pivot(&land_request, &land_intent)?;
    let err = store
        .land_pivot(&land_intent, false, || anyhow::bail!("CAS failed"))
        .unwrap_err();
    assert_eq!(err.to_string(), "CAS failed");
    assert_request(&store, &land_request, &land_intent, Prepared, None)?;
    store.db.execute_batch(
        "CREATE TRIGGER fail_remove BEFORE DELETE ON pivot_resume_intent
                             BEGIN SELECT RAISE(ABORT, 'injected intent removal failure'); END;",
    )?;
    let adopted = Cell::new(false);
    let error = store
        .land_pivot(&land_intent, false, || {
            adopted.set(true);
            Ok(())
        })
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("injected intent removal failure")
    );
    assert!(adopted.get());
    assert_request(&store, &land_request, &land_intent, Prepared, None)?;
    assert_eq!(store.pivot_intent()?, Some(land_intent.clone()));
    assert!(store.recorded_pivot(&land_intent.adr_id)?.is_none());
    assert_eq!(last_seq(&store.db)?, seq);
    assert_eq!(current_checkpoint(&store.db)?, land_intent.plan.checkpoint);
    assert_eq!(read_target(&store.db, "land")?.0, land_intent.plan.breaker);
    store.db.execute_batch("DROP TRIGGER fail_remove")?;
    let result = store.land_pivot(&land_intent, true, || Ok(()))?;
    assert_request(&store, &land_request, &land_intent, Applied, Some(result))
}

#[test]
fn migration_preserves_existing_pivot_receipt_and_pending_intent() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("state.sqlite");
    let mut store = StateStore::open(&path)?;
    trip(&mut store, "landed", BASE)?;
    let (old_request, old_intent) =
        approval(&mut store, "landed", PivotAction::ArchiveBreaker, NEW)?;
    store.prepare_pivot(&old_intent)?;
    let old_result = store.land_pivot(&old_intent, false, || Ok(()))?;
    trip(&mut store, "pending", NEW)?;
    let (pending_request, pending_intent) =
        approval(&mut store, "pending", PivotAction::ArchiveBreaker, OTHER)?;
    store.prepare_pivot(&pending_intent)?;
    let seq = last_seq(&store.db)?;
    // Recreate the version-5 schema with a regular historical receipt and regular pending intent.
    store.db.execute_batch(
        "DROP TABLE owner_archive_requests; DELETE FROM state_migrations WHERE version=6;",
    )?;
    drop(store);
    let reader = StateStore::open_readonly(&path)?;
    assert!(
        reader
            .owner_archive_request(&old_request.request_id)?
            .is_none()
    );
    assert!(
        reader
            .owner_archive_request(&pending_request.request_id)?
            .is_none()
    );
    assert!(!reader.db.query_row(
        "SELECT EXISTS(SELECT 1 FROM state_migrations WHERE version=6)",
        [],
        |r| r.get::<_, bool>(0)
    )?);
    drop(reader);

    let mut store = StateStore::open(&path)?;
    assert_eq!(last_seq(&store.db)?, seq);
    assert_eq!(store.pivot_intent()?, Some(pending_intent.clone()));
    assert_eq!(
        store.recorded_pivot(&old_intent.adr_id)?,
        Some((old_intent.plan.clone(), old_result))
    );
    assert!(
        store
            .owner_archive_request(&old_request.request_id)?
            .is_none()
    );
    assert!(
        store
            .owner_archive_request(&pending_request.request_id)?
            .is_none()
    );
    assert!(
        store
            .prepare_owner_archive_pivot(&pending_request, &pending_intent)
            .is_err()
    );
    store.abandon_unadopted_pivot(&pending_intent)?;
    assert!(
        store
            .prepare_owner_archive_pivot(&old_request, &old_intent)
            .is_err()
    );
    store.prepare_owner_archive_pivot(&pending_request, &pending_intent)?;
    drop(store);
    let store = StateStore::open(&path)?;
    assert_eq!(last_seq(&store.db)?, seq);
    assert_request(
        &store,
        &pending_request,
        &pending_intent,
        OwnerArchiveRequestStatus::Prepared,
        None,
    )
}
