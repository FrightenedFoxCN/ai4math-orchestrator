use super::*;
const REPO: &str = "/managed/rfc.git";
const BASE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const MAIN2: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const MAIN3: &str = "cccccccccccccccccccccccccccccccccccccccc";
const MAIN4: &str = "dddddddddddddddddddddddddddddddddddddddd";
const SMOKE_RECEIPT: &str = "1111111111111111111111111111111111111111111111111111111111111111";
const SMOKE_RECEIPT2: &str = "2222222222222222222222222222222222222222222222222222222222222222";
const COMPLETE_RECEIPT: &str = "3333333333333333333333333333333333333333333333333333333333333333";
const COMPLETE_RECEIPT2: &str = "4444444444444444444444444444444444444444444444444444444444444444";
const MODULE: &str = "Basis.lean";

fn draft_for(id: &str, claim_id: &str, engine: Engine) -> InfraRFC {
    InfraRFC {
        rfc_id: id.into(),
        requested_by_claim: claim_id.into(),
        engine,
        motivation: "Need a reusable boundary API.".into(),
        proposed_definitions: vec!["Basis.bound".into()],
        required_api_lemmas: vec!["Basis.bound_spec".into()],
        consumer_usage_sketch: "exact Basis.bound_spec\n-- sketch, not executed".into(),
        smoke_test_criteria: vec!["Check an actual boundary case".into()],
        status: InfraStatus::Draft,
    }
}

fn draft(id: &str) -> InfraRFC {
    draft_for(id, "Consumer", Engine::Lean4)
}

fn enroll(store: &mut StateStore) -> Result<()> {
    let change = Change {
        actor: "fixture",
        git_commit: Some(BASE),
    };
    store.bind_task_repository(REPO, BASE)?;
    store.create_claim(
        NewClaim {
            id: "Consumer",
            title: "Consumer",
            informal_statement: "uses the new basis",
            engine: "lean4",
            formal_target: Some("Consumer.lean#N.theorem"),
            module_path: Some("Consumer.lean"),
            critical: false,
        },
        change,
    )?;
    Ok(())
}

fn enroll_claim(
    store: &mut StateStore,
    claim_id: &str,
    engine: &str,
    module_path: &str,
    formal_target: &str,
) -> Result<()> {
    store.create_claim(
        NewClaim {
            id: claim_id,
            title: claim_id,
            informal_statement: "consumer claim",
            engine,
            formal_target: Some(formal_target),
            module_path: Some(module_path),
            critical: false,
        },
        Change {
            actor: "fixture",
            git_commit: Some(BASE),
        },
    )?;
    Ok(())
}

#[test]
fn git_callback_failure_rolls_back_events_and_approval_and_preserves_immutable_draft() -> Result<()>
{
    let temp = tempfile::tempdir()?;
    let db = temp.path().join("ledger.sqlite");
    let mut store = StateStore::open(&db)?;
    enroll(&mut store)?;
    let before = store.status()?.last_event_seq;
    let d = draft("RFC-draft");
    assert!(
        store
            .submit_infra_rfc(REPO, BASE, MODULE, &d, || anyhow::bail!("moved Git"))
            .is_err()
    );
    assert_eq!(store.status()?.last_event_seq, before);
    assert!(store.infra_rfc(REPO, &d.rfc_id)?.is_none());
    let r = store.submit_infra_rfc(REPO, BASE, MODULE, &d, || Ok(()))?;
    assert_eq!(r.status, InfraRfcStatus::Draft);
    assert_eq!(r.submitted_event_seq, before + 1);
    // Bit-for-bit Round 71 JSON backward compatibility for DRAFT.
    assert_eq!(
        serde_json::to_value(&r)?,
        json!({
            "schema": "ai4math.infra-rfc.v1",
            "module_path": MODULE,
            "reviewed_commit": BASE,
            "draft_sha256": r.draft_sha256,
            "status": "DRAFT",
            "draft": d,
            "submitted_event_seq": before + 1,
        })
    );
    assert!(
        store
            .approve_infra_rfc(REPO, BASE, &d.rfc_id, &r.draft_sha256, || anyhow::bail!(
                "moved Git"
            ))
            .is_err()
    );
    assert_eq!(store.status()?.last_event_seq, before + 1);
    assert_eq!(store.infra_rfc(REPO, &d.rfc_id)?, Some(r.clone()));
    let mut changed = d.clone();
    changed.consumer_usage_sketch.push('!');
    assert!(
        store
            .submit_infra_rfc(REPO, BASE, MODULE, &changed, || Ok(()))
            .is_err()
    );
    for reserved in [
        SUBMITTED,
        APPROVED,
        SMOKE_STARTED,
        FROZEN,
        COMPLETED,
        SUPERSEDED,
        REJECTED,
    ] {
        assert!(
            store
                .append_event(
                    Change {
                        actor: "bad",
                        git_commit: Some(BASE)
                    },
                    reserved,
                    ENTITY,
                    &d.rfc_id,
                    json!({})
                )
                .is_err(),
            "append_event must reject reserved {reserved}"
        );
    }
    let a = store.approve_infra_rfc(REPO, BASE, &d.rfc_id, &r.draft_sha256, || Ok(()))?;
    assert_eq!(a.status, InfraRfcStatus::Approved);
    assert_eq!(a.draft, d);
    assert_eq!(a.approved_event_seq, Some(before + 2));
    // Bit-for-bit Round 71 JSON backward compatibility for APPROVED.
    assert_eq!(
        serde_json::to_value(&a)?,
        json!({
            "schema": "ai4math.infra-rfc.v1",
            "module_path": MODULE,
            "reviewed_commit": BASE,
            "draft_sha256": r.draft_sha256,
            "status": "APPROVED",
            "draft": d,
            "submitted_event_seq": before + 1,
            "approved_event_seq": before + 2,
        })
    );
    assert_eq!(
        store.submit_infra_rfc(REPO, BASE, MODULE, &d, || Ok(()))?,
        a
    );
    assert_eq!(
        store.approve_infra_rfc(REPO, BASE, &d.rfc_id, &r.draft_sha256, || Ok(()))?,
        a
    );
    assert!(
        store
            .db
            .execute("UPDATE infra_rfcs SET draft_json='{}'", [])
            .is_err()
    );
    assert!(store.db.execute("DELETE FROM infra_rfcs", []).is_err());
    drop(store);
    let reopened = StateStore::open_readonly(&db)?;
    assert_eq!(reopened.infra_rfc(REPO, &d.rfc_id)?, Some(a));
    assert_eq!(reopened.claim("Consumer")?.unwrap().local_trust, Trust::L0);
    assert_eq!(reopened.status()?.last_event_seq, before + 2);
    assert!(reopened.infra_rfc("/unbound", &d.rfc_id).is_err());
    Ok(())
}

#[test]
fn concurrent_approval_obtains_one_sequence_and_same_exact_record() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let db = tmp.path().join("state.sqlite");
    let mut store = StateStore::open(&db)?;
    enroll(&mut store)?;
    let d = draft("RFC-once");
    let r = store.submit_infra_rfc(REPO, BASE, MODULE, &d, || Ok(()))?;
    std::thread::scope(|scope| -> Result<()> {
        let apply = || {
            StateStore::open(&db)?
                .approve_infra_rfc(REPO, BASE, &d.rfc_id, &r.draft_sha256, || Ok(()))
        };
        let a = scope.spawn(apply);
        let b = scope.spawn(apply);
        assert_eq!(a.join().unwrap()?, b.join().unwrap()?);
        Ok(())
    })?;
    assert_eq!(store.status()?.last_event_seq, r.submitted_event_seq + 1);
    Ok(())
}

#[test]
fn full_happy_path_lifecycle_idempotency_and_rollbacks() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let db = tmp.path().join("lifecycle.sqlite");
    let mut store = StateStore::open(&db)?;
    enroll(&mut store)?;
    let d = draft("RFC-happy");
    let r = store.submit_infra_rfc(REPO, BASE, MODULE, &d, || Ok(()))?;
    let sha = &r.draft_sha256;

    // Cannot skip directly from DRAFT to BUILDING_SMOKE, FROZEN_PROVING, or COMPLETED.
    assert!(
        store
            .start_infra_rfc_smoke(REPO, BASE, &d.rfc_id, sha, || Ok(()))
            .is_err()
    );
    assert!(
        store
            .freeze_infra_rfc_verified(
                REPO,
                &d.rfc_id,
                sha,
                MAIN2,
                MODULE,
                &d.proposed_definitions,
                SMOKE_RECEIPT,
                || Ok(())
            )
            .is_err()
    );
    assert!(
        store
            .complete_infra_rfc_verified(
                REPO,
                &d.rfc_id,
                sha,
                MAIN3,
                MODULE,
                &d.proposed_definitions,
                &d.required_api_lemmas,
                COMPLETE_RECEIPT,
                || Ok(())
            )
            .is_err()
    );

    // DRAFT -> APPROVED
    let approved = store.approve_infra_rfc(REPO, BASE, &d.rfc_id, sha, || Ok(()))?;
    assert_eq!(approved.status, InfraRfcStatus::Approved);
    // Cannot skip from APPROVED directly to FROZEN_PROVING or COMPLETED.
    assert!(
        store
            .freeze_infra_rfc_verified(
                REPO,
                &d.rfc_id,
                sha,
                MAIN2,
                MODULE,
                &d.proposed_definitions,
                SMOKE_RECEIPT,
                || Ok(())
            )
            .is_err()
    );
    assert!(
        store
            .complete_infra_rfc_verified(
                REPO,
                &d.rfc_id,
                sha,
                MAIN3,
                MODULE,
                &d.proposed_definitions,
                &d.required_api_lemmas,
                COMPLETE_RECEIPT,
                || Ok(())
            )
            .is_err()
    );

    // Fresh smoke start must match exact reviewed_commit.
    assert!(
        store
            .start_infra_rfc_smoke(REPO, MAIN2, &d.rfc_id, sha, || Ok(()))
            .is_err()
    );

    // Rollback on confirm_git failure during start_infra_rfc_smoke.
    let seq_before_smoke = store.status()?.last_event_seq;
    assert!(
        store
            .start_infra_rfc_smoke(REPO, BASE, &d.rfc_id, sha, || anyhow::bail!("head moved"))
            .is_err()
    );
    assert_eq!(store.status()?.last_event_seq, seq_before_smoke);
    assert_eq!(store.infra_rfc(REPO, &d.rfc_id)?, Some(approved.clone()));

    // APPROVED -> BUILDING_SMOKE
    let building = store.start_infra_rfc_smoke(REPO, BASE, &d.rfc_id, sha, || Ok(()))?;
    assert_eq!(building.status, InfraRfcStatus::BuildingSmoke);
    assert_eq!(building.smoke_started_event_seq, Some(seq_before_smoke + 1));
    assert_eq!(building.smoke_commit, None);
    // Idempotent retry with same reviewed_main and draft_sha256.
    assert_eq!(
        store.start_infra_rfc_smoke(REPO, BASE, &d.rfc_id, sha, || Ok(()))?,
        building
    );
    assert_eq!(store.status()?.last_event_seq, seq_before_smoke + 1);
    // Retry with different reviewed_main or wrong digest fails.
    assert!(
        store
            .start_infra_rfc_smoke(REPO, MAIN3, &d.rfc_id, sha, || Ok(()))
            .is_err()
    );
    assert!(
        store
            .start_infra_rfc_smoke(REPO, BASE, &d.rfc_id, &"0".repeat(64), || Ok(()))
            .is_err()
    );
    // approve_infra_rfc fails once moved beyond APPROVED; submit remains idempotent without downgrading.
    assert!(
        store
            .approve_infra_rfc(REPO, BASE, &d.rfc_id, sha, || Ok(()))
            .is_err()
    );
    assert_eq!(
        store.submit_infra_rfc(REPO, BASE, MODULE, &d, || Ok(()))?,
        building
    );
    // Cannot complete before freezing.
    assert!(
        store
            .complete_infra_rfc_verified(
                REPO,
                &d.rfc_id,
                sha,
                MAIN3,
                MODULE,
                &d.proposed_definitions,
                &d.required_api_lemmas,
                COMPLETE_RECEIPT,
                || Ok(())
            )
            .is_err()
    );

    // Mismatched module or definitions rejected for freeze; confirm_git rollback works.
    assert!(
        store
            .freeze_infra_rfc_verified(
                REPO,
                &d.rfc_id,
                sha,
                MAIN3,
                "Other.lean",
                &d.proposed_definitions,
                SMOKE_RECEIPT,
                || Ok(())
            )
            .is_err()
    );
    assert!(
        store
            .freeze_infra_rfc_verified(
                REPO,
                &d.rfc_id,
                sha,
                MAIN3,
                MODULE,
                &["Basis.other".into()],
                SMOKE_RECEIPT,
                || Ok(())
            )
            .is_err()
    );
    let seq_before_freeze = store.status()?.last_event_seq;
    assert!(
        store
            .freeze_infra_rfc_verified(
                REPO,
                &d.rfc_id,
                sha,
                MAIN3,
                MODULE,
                &d.proposed_definitions,
                SMOKE_RECEIPT,
                || anyhow::bail!("head moved")
            )
            .is_err()
    );
    assert_eq!(store.status()?.last_event_seq, seq_before_freeze);
    assert_eq!(store.infra_rfc(REPO, &d.rfc_id)?, Some(building.clone()));

    // BUILDING_SMOKE -> FROZEN_PROVING
    let frozen = store.freeze_infra_rfc_verified(
        REPO,
        &d.rfc_id,
        sha,
        MAIN3,
        MODULE,
        &d.proposed_definitions,
        SMOKE_RECEIPT,
        || Ok(()),
    )?;
    assert_eq!(frozen.status, InfraRfcStatus::FrozenProving);
    assert_eq!(frozen.frozen_event_seq, Some(seq_before_freeze + 1));
    assert_eq!(frozen.smoke_commit.as_deref(), Some(MAIN3));
    assert_eq!(frozen.smoke_receipt_sha256.as_deref(), Some(SMOKE_RECEIPT));
    // Idempotent retry with same smoke commit + receipt.
    assert_eq!(
        store.freeze_infra_rfc_verified(
            REPO,
            &d.rfc_id,
            sha,
            MAIN3,
            MODULE,
            &d.proposed_definitions,
            SMOKE_RECEIPT,
            || Ok(())
        )?,
        frozen
    );
    assert_eq!(store.status()?.last_event_seq, seq_before_freeze + 1);
    // Different commit or receipt rejected on retry.
    assert!(
        store
            .freeze_infra_rfc_verified(
                REPO,
                &d.rfc_id,
                sha,
                MAIN4,
                MODULE,
                &d.proposed_definitions,
                SMOKE_RECEIPT,
                || Ok(())
            )
            .is_err()
    );
    assert!(
        store
            .freeze_infra_rfc_verified(
                REPO,
                &d.rfc_id,
                sha,
                MAIN3,
                MODULE,
                &d.proposed_definitions,
                SMOKE_RECEIPT2,
                || Ok(())
            )
            .is_err()
    );
    // Cannot start smoke again or reject once frozen.
    assert!(
        store
            .start_infra_rfc_smoke(REPO, BASE, &d.rfc_id, sha, || Ok(()))
            .is_err()
    );
    assert!(
        store
            .reject_infra_rfc(REPO, MAIN3, &d.rfc_id, sha, "too late to reject", || Ok(()))
            .is_err()
    );

    // Mismatched commit/module/defs/api_lemmas rejected for completion; confirm_git rollback works.
    assert!(
        store
            .complete_infra_rfc_verified(
                REPO,
                &d.rfc_id,
                sha,
                MAIN4,
                MODULE,
                &d.proposed_definitions,
                &d.required_api_lemmas,
                COMPLETE_RECEIPT,
                || Ok(())
            )
            .is_err()
    );
    assert!(
        store
            .complete_infra_rfc_verified(
                REPO,
                &d.rfc_id,
                sha,
                MAIN3,
                "Other.lean",
                &d.proposed_definitions,
                &d.required_api_lemmas,
                COMPLETE_RECEIPT,
                || Ok(())
            )
            .is_err()
    );
    assert!(
        store
            .complete_infra_rfc_verified(
                REPO,
                &d.rfc_id,
                sha,
                MAIN3,
                MODULE,
                &["Basis.wrong".into()],
                &d.required_api_lemmas,
                COMPLETE_RECEIPT,
                || Ok(())
            )
            .is_err()
    );
    assert!(
        store
            .complete_infra_rfc_verified(
                REPO,
                &d.rfc_id,
                sha,
                MAIN3,
                MODULE,
                &d.proposed_definitions,
                &["Basis.wrong_lemma".into()],
                COMPLETE_RECEIPT,
                || Ok(())
            )
            .is_err()
    );
    let seq_before_complete = store.status()?.last_event_seq;
    assert!(
        store
            .complete_infra_rfc_verified(
                REPO,
                &d.rfc_id,
                sha,
                MAIN3,
                MODULE,
                &d.proposed_definitions,
                &d.required_api_lemmas,
                COMPLETE_RECEIPT,
                || anyhow::bail!("head moved")
            )
            .is_err()
    );
    assert_eq!(store.status()?.last_event_seq, seq_before_complete);
    assert_eq!(store.infra_rfc(REPO, &d.rfc_id)?, Some(frozen.clone()));

    // FROZEN_PROVING -> COMPLETED (must match frozen smoke_commit MAIN3)
    let completed = store.complete_infra_rfc_verified(
        REPO,
        &d.rfc_id,
        sha,
        MAIN3,
        MODULE,
        &d.proposed_definitions,
        &d.required_api_lemmas,
        COMPLETE_RECEIPT,
        || Ok(()),
    )?;
    assert_eq!(completed.status, InfraRfcStatus::Completed);
    assert_eq!(completed.completed_event_seq, Some(seq_before_complete + 1));
    assert_eq!(completed.completion_commit.as_deref(), Some(MAIN3));
    assert_eq!(
        completed.completion_receipt_sha256.as_deref(),
        Some(COMPLETE_RECEIPT)
    );
    // Idempotent retry with exact completion evidence.
    assert_eq!(
        store.complete_infra_rfc_verified(
            REPO,
            &d.rfc_id,
            sha,
            MAIN3,
            MODULE,
            &d.proposed_definitions,
            &d.required_api_lemmas,
            COMPLETE_RECEIPT,
            || Ok(())
        )?,
        completed
    );
    assert_eq!(store.status()?.last_event_seq, seq_before_complete + 1);
    // Different completion evidence or commit fails.
    assert!(
        store
            .complete_infra_rfc_verified(
                REPO,
                &d.rfc_id,
                sha,
                MAIN3,
                MODULE,
                &d.proposed_definitions,
                &d.required_api_lemmas,
                COMPLETE_RECEIPT2,
                || Ok(())
            )
            .is_err()
    );
    assert!(
        store
            .complete_infra_rfc_verified(
                REPO,
                &d.rfc_id,
                sha,
                MAIN4,
                MODULE,
                &d.proposed_definitions,
                &d.required_api_lemmas,
                COMPLETE_RECEIPT,
                || Ok(())
            )
            .is_err()
    );
    // Submit retry still idempotent without downgrading.
    assert_eq!(
        store.submit_infra_rfc(REPO, BASE, MODULE, &d, || Ok(()))?,
        completed
    );
    // Terminal COMPLETED blocks further lifecycle mutations.
    assert!(
        store
            .approve_infra_rfc(REPO, BASE, &d.rfc_id, sha, || Ok(()))
            .is_err()
    );
    assert!(
        store
            .start_infra_rfc_smoke(REPO, BASE, &d.rfc_id, sha, || Ok(()))
            .is_err()
    );
    assert!(
        store
            .reject_infra_rfc(REPO, MAIN4, &d.rfc_id, sha, "cannot reject done", || Ok(()))
            .is_err()
    );

    // Immutability triggers on infra_rfc_transitions.
    assert!(
        store
            .db
            .execute("UPDATE infra_rfc_transitions SET reason='x'", [])
            .is_err()
    );
    assert!(
        store
            .db
            .execute("DELETE FROM infra_rfc_transitions", [])
            .is_err()
    );

    // Readback via open_readonly matches exactly.
    drop(store);
    let ro = StateStore::open_readonly(&db)?;
    assert_eq!(ro.infra_rfc(REPO, &d.rfc_id)?, Some(completed));
    Ok(())
}

#[test]
fn rejection_paths_and_non_lean_engine_guards() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let mut store = StateStore::open(tmp.path().join("reject.sqlite"))?;
    enroll(&mut store)?;
    enroll_claim(
        &mut store,
        "AgdaConsumer",
        "agda",
        "AgdaConsumer.agda",
        "AgdaConsumer.agda#thm",
    )?;

    // Reject from DRAFT.
    let d1 = draft("RFC-rej-draft");
    let r1 = store.submit_infra_rfc(REPO, BASE, MODULE, &d1, || Ok(()))?;
    // Invalid reasons rejected.
    for bad in ["", " leading", "trailing ", "line1\nline2", "a\u{202e}b"] {
        assert!(
            store
                .reject_infra_rfc(REPO, BASE, &d1.rfc_id, &r1.draft_sha256, bad, || Ok(()))
                .is_err(),
            "reason {bad:?} must fail"
        );
    }
    assert!(
        store
            .reject_infra_rfc(
                REPO,
                BASE,
                &d1.rfc_id,
                &r1.draft_sha256,
                &"x".repeat(513),
                || Ok(())
            )
            .is_err()
    );
    // confirm_git failure rolls back rejection.
    let seq = store.status()?.last_event_seq;
    assert!(
        store
            .reject_infra_rfc(
                REPO,
                BASE,
                &d1.rfc_id,
                &r1.draft_sha256,
                "Scope too broad",
                || anyhow::bail!("git changed")
            )
            .is_err()
    );
    assert_eq!(store.status()?.last_event_seq, seq);
    let rej1 = store.reject_infra_rfc(
        REPO,
        BASE,
        &d1.rfc_id,
        &r1.draft_sha256,
        "Scope too broad",
        || Ok(()),
    )?;
    assert_eq!(rej1.status, InfraRfcStatus::Rejected);
    assert_eq!(rej1.rejected_reason.as_deref(), Some("Scope too broad"));
    assert_eq!(rej1.rejected_event_seq, Some(seq + 1));
    assert_eq!(rej1.approved_event_seq, None);
    // Idempotent retry succeeds; different reason or commit fails.
    assert_eq!(
        store.reject_infra_rfc(
            REPO,
            BASE,
            &d1.rfc_id,
            &r1.draft_sha256,
            "Scope too broad",
            || Ok(())
        )?,
        rej1
    );
    assert!(
        store
            .reject_infra_rfc(
                REPO,
                BASE,
                &d1.rfc_id,
                &r1.draft_sha256,
                "Different reason",
                || Ok(())
            )
            .is_err()
    );
    assert!(
        store
            .reject_infra_rfc(
                REPO,
                MAIN2,
                &d1.rfc_id,
                &r1.draft_sha256,
                "Scope too broad",
                || Ok(())
            )
            .is_err()
    );
    // Cannot approve a REJECTED draft!
    assert!(
        store
            .approve_infra_rfc(REPO, BASE, &d1.rfc_id, &r1.draft_sha256, || Ok(()))
            .is_err()
    );

    // Reject from APPROVED.
    let d2 = draft("RFC-rej-approved");
    let r2 = store.submit_infra_rfc(REPO, BASE, MODULE, &d2, || Ok(()))?;
    store.approve_infra_rfc(REPO, BASE, &d2.rfc_id, &r2.draft_sha256, || Ok(()))?;
    let rej2 = store.reject_infra_rfc(
        REPO,
        MAIN2,
        &d2.rfc_id,
        &r2.draft_sha256,
        "API design withdrawn",
        || Ok(()),
    )?;
    assert_eq!(rej2.status, InfraRfcStatus::Rejected);
    assert_eq!(
        rej2.rejected_reason.as_deref(),
        Some("API design withdrawn")
    );
    assert!(
        store
            .start_infra_rfc_smoke(REPO, BASE, &d2.rfc_id, &r2.draft_sha256, || Ok(()))
            .is_err()
    );

    // Reject from BUILDING_SMOKE.
    let d3 = draft("RFC-rej-smoke");
    let r3 = store.submit_infra_rfc(REPO, BASE, MODULE, &d3, || Ok(()))?;
    store.approve_infra_rfc(REPO, BASE, &d3.rfc_id, &r3.draft_sha256, || Ok(()))?;
    store.start_infra_rfc_smoke(REPO, BASE, &d3.rfc_id, &r3.draft_sha256, || Ok(()))?;
    let rej3 = store.reject_infra_rfc(
        REPO,
        MAIN3,
        &d3.rfc_id,
        &r3.draft_sha256,
        "Smoke prototype exposed unsound abstraction",
        || Ok(()),
    )?;
    assert_eq!(rej3.status, InfraRfcStatus::Rejected);
    assert!(
        store
            .freeze_infra_rfc_verified(
                REPO,
                &d3.rfc_id,
                &r3.draft_sha256,
                MAIN3,
                MODULE,
                &d3.proposed_definitions,
                SMOKE_RECEIPT,
                || Ok(())
            )
            .is_err()
    );

    // Non-Lean4 RFC can start smoke, but cannot freeze or complete via Lean4 verifiers.
    let agda = draft_for("RFC-agda", "AgdaConsumer", Engine::Agda);
    let ra = store.submit_infra_rfc(REPO, BASE, "AgdaBasis.agda", &agda, || Ok(()))?;
    store.approve_infra_rfc(REPO, BASE, &agda.rfc_id, &ra.draft_sha256, || Ok(()))?;
    store.start_infra_rfc_smoke(REPO, BASE, &agda.rfc_id, &ra.draft_sha256, || Ok(()))?;
    assert!(
        store
            .freeze_infra_rfc_verified(
                REPO,
                &agda.rfc_id,
                &ra.draft_sha256,
                MAIN2,
                "AgdaBasis.agda",
                &agda.proposed_definitions,
                SMOKE_RECEIPT,
                || Ok(())
            )
            .is_err()
    );
    Ok(())
}

#[test]
fn supersession_rules_and_replacement_validation() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let mut store = StateStore::open(tmp.path().join("supersede.sqlite"))?;
    enroll(&mut store)?;
    enroll_claim(
        &mut store,
        "OtherConsumer",
        "lean4",
        "OtherConsumer.lean",
        "OtherConsumer.lean#N.other",
    )?;

    let rep_live = draft("RFC-rep-live");
    let _rep_live_rec = store.submit_infra_rfc(REPO, BASE, MODULE, &rep_live, || Ok(()))?;

    let rep_other_claim = draft_for("RFC-rep-other-claim", "OtherConsumer", Engine::Lean4);
    store.submit_infra_rfc(REPO, BASE, MODULE, &rep_other_claim, || Ok(()))?;

    let rep_rejected = draft("RFC-rep-rejected");
    let rr = store.submit_infra_rfc(REPO, BASE, MODULE, &rep_rejected, || Ok(()))?;
    store.reject_infra_rfc(
        REPO,
        BASE,
        &rep_rejected.rfc_id,
        &rr.draft_sha256,
        "obsolete",
        || Ok(()),
    )?;

    // DRAFT cannot be superseded.
    let d_draft = draft("RFC-sup-draft");
    let rd = store.submit_infra_rfc(REPO, BASE, MODULE, &d_draft, || Ok(()))?;
    assert!(
        store
            .supersede_infra_rfc(
                REPO,
                BASE,
                &d_draft.rfc_id,
                &rd.draft_sha256,
                &rep_live.rfc_id,
                "replaced",
                || Ok(())
            )
            .is_err()
    );

    // Supersede from APPROVED + validate replacement rejections.
    let d_app = draft("RFC-sup-approved");
    let ra = store.submit_infra_rfc(REPO, BASE, MODULE, &d_app, || Ok(()))?;
    store.approve_infra_rfc(REPO, BASE, &d_app.rfc_id, &ra.draft_sha256, || Ok(()))?;

    // Self-supersession rejected.
    assert!(
        store
            .supersede_infra_rfc(
                REPO,
                MAIN2,
                &d_app.rfc_id,
                &ra.draft_sha256,
                &d_app.rfc_id,
                "self",
                || Ok(())
            )
            .is_err()
    );
    // Nonexistent replacement rejected.
    assert!(
        store
            .supersede_infra_rfc(
                REPO,
                MAIN2,
                &d_app.rfc_id,
                &ra.draft_sha256,
                "RFC-missing",
                "missing",
                || Ok(())
            )
            .is_err()
    );
    // Wrong consumer claim replacement rejected.
    assert!(
        store
            .supersede_infra_rfc(
                REPO,
                MAIN2,
                &d_app.rfc_id,
                &ra.draft_sha256,
                &rep_other_claim.rfc_id,
                "wrong claim",
                || Ok(())
            )
            .is_err()
    );
    // Rejected replacement rejected.
    assert!(
        store
            .supersede_infra_rfc(
                REPO,
                MAIN2,
                &d_app.rfc_id,
                &ra.draft_sha256,
                &rep_rejected.rfc_id,
                "rejected rep",
                || Ok(())
            )
            .is_err()
    );
    // confirm_git failure rolls back supersession.
    let seq = store.status()?.last_event_seq;
    assert!(
        store
            .supersede_infra_rfc(
                REPO,
                MAIN2,
                &d_app.rfc_id,
                &ra.draft_sha256,
                &rep_live.rfc_id,
                "Better abstraction",
                || anyhow::bail!("head moved")
            )
            .is_err()
    );
    assert_eq!(store.status()?.last_event_seq, seq);

    let sup1 = store.supersede_infra_rfc(
        REPO,
        MAIN2,
        &d_app.rfc_id,
        &ra.draft_sha256,
        &rep_live.rfc_id,
        "Better abstraction",
        || Ok(()),
    )?;
    assert_eq!(sup1.status, InfraRfcStatus::Superseded);
    assert_eq!(
        sup1.superseded_by_rfc_id.as_deref(),
        Some(rep_live.rfc_id.as_str())
    );
    assert_eq!(sup1.superseded_event_seq, Some(seq + 1));
    // Idempotent retry succeeds; different replacement/reason/commit fails.
    assert_eq!(
        store.supersede_infra_rfc(
            REPO,
            MAIN2,
            &d_app.rfc_id,
            &ra.draft_sha256,
            &rep_live.rfc_id,
            "Better abstraction",
            || Ok(())
        )?,
        sup1
    );
    assert!(
        store
            .supersede_infra_rfc(
                REPO,
                MAIN2,
                &d_app.rfc_id,
                &ra.draft_sha256,
                &rep_live.rfc_id,
                "Other reason",
                || Ok(())
            )
            .is_err()
    );
    assert!(
        store
            .supersede_infra_rfc(
                REPO,
                MAIN3,
                &d_app.rfc_id,
                &ra.draft_sha256,
                &rep_live.rfc_id,
                "Better abstraction",
                || Ok(())
            )
            .is_err()
    );

    // A SUPERSEDED RFC cannot be used as a replacement for another RFC.
    let d_smoke = draft("RFC-sup-smoke");
    let rs = store.submit_infra_rfc(REPO, BASE, MODULE, &d_smoke, || Ok(()))?;
    store.approve_infra_rfc(REPO, BASE, &d_smoke.rfc_id, &rs.draft_sha256, || Ok(()))?;
    store.start_infra_rfc_smoke(REPO, BASE, &d_smoke.rfc_id, &rs.draft_sha256, || Ok(()))?;
    assert!(
        store
            .supersede_infra_rfc(
                REPO,
                MAIN3,
                &d_smoke.rfc_id,
                &rs.draft_sha256,
                &d_app.rfc_id,
                "superseded rep",
                || Ok(())
            )
            .is_err()
    );
    // Supersede from BUILDING_SMOKE succeeds with live replacement.
    let sup2 = store.supersede_infra_rfc(
        REPO,
        MAIN3,
        &d_smoke.rfc_id,
        &rs.draft_sha256,
        &rep_live.rfc_id,
        "Refactored in new RFC",
        || Ok(()),
    )?;
    assert_eq!(sup2.status, InfraRfcStatus::Superseded);

    // Supersede from FROZEN_PROVING succeeds with live replacement.
    let d_frozen = draft("RFC-sup-frozen");
    let rf = store.submit_infra_rfc(REPO, BASE, MODULE, &d_frozen, || Ok(()))?;
    store.approve_infra_rfc(REPO, BASE, &d_frozen.rfc_id, &rf.draft_sha256, || Ok(()))?;
    store.start_infra_rfc_smoke(REPO, BASE, &d_frozen.rfc_id, &rf.draft_sha256, || Ok(()))?;
    store.freeze_infra_rfc_verified(
        REPO,
        &d_frozen.rfc_id,
        &rf.draft_sha256,
        MAIN3,
        MODULE,
        &d_frozen.proposed_definitions,
        SMOKE_RECEIPT,
        || Ok(()),
    )?;
    let sup3 = store.supersede_infra_rfc(
        REPO,
        MAIN4,
        &d_frozen.rfc_id,
        &rf.draft_sha256,
        &rep_live.rfc_id,
        "Stronger definition needed during proving",
        || Ok(()),
    )?;
    assert_eq!(sup3.status, InfraRfcStatus::Superseded);
    assert_eq!(
        sup3.superseded_by_rfc_id.as_deref(),
        Some(rep_live.rfc_id.as_str())
    );
    // Cannot complete or reject a SUPERSEDED RFC.
    assert!(
        store
            .complete_infra_rfc_verified(
                REPO,
                &d_frozen.rfc_id,
                &rf.draft_sha256,
                MAIN4,
                MODULE,
                &d_frozen.proposed_definitions,
                &d_frozen.required_api_lemmas,
                COMPLETE_RECEIPT,
                || Ok(())
            )
            .is_err()
    );
    assert!(
        store
            .reject_infra_rfc(
                REPO,
                MAIN4,
                &d_frozen.rfc_id,
                &rf.draft_sha256,
                "cannot reject superseded",
                || Ok(())
            )
            .is_err()
    );
    Ok(())
}

#[test]
fn tamper_detection_on_transitions_and_legacy_db_compatibility() -> Result<()> {
    let insert_raw_event = |db: &Connection,
                            event_type: &str,
                            rfc_id: &str,
                            git_commit: &str,
                            actor: &str,
                            payload: &Value|
     -> Result<i64> {
        let seq: i64 = db.query_row(
            "INSERT INTO events(occurred_at,event_type,entity_type,entity_id,git_commit,actor,payload_json)
             VALUES(strftime('%Y-%m-%dT%H:%M:%fZ','now'),?1,?2,?3,?4,?5,?6) RETURNING event_seq",
            params![
                event_type,
                ENTITY,
                rfc_id,
                git_commit,
                actor,
                serde_json::to_string(payload)?
            ],
            |r| r.get(0),
        )?;
        Ok(seq)
    };

    // Case 1: chain mismatch (APPROVED -> BUILDING_SMOKE on a DRAFT RFC).
    {
        let tmp = tempfile::tempdir()?;
        let mut store = StateStore::open(tmp.path().join("t1.sqlite"))?;
        enroll(&mut store)?;
        let d = draft("RFC-tamper-1");
        let r = store.submit_infra_rfc(REPO, BASE, MODULE, &d, || Ok(()))?;
        let payload = transition_event_payload(
            MODULE,
            Engine::Lean4,
            &r.draft_sha256,
            InfraRfcStatus::Approved,
            InfraRfcStatus::BuildingSmoke,
            None,
            None,
            None,
        );
        let seq = insert_raw_event(&store.db, SMOKE_STARTED, &d.rfc_id, MAIN2, ACTOR, &payload)?;
        store.db.execute(
            "INSERT INTO infra_rfc_transitions(event_seq,rfc_id,from_status,to_status,git_commit,evidence_sha256,superseded_by_rfc_id,reason)
             VALUES(?1,?2,'APPROVED','BUILDING_SMOKE',?3,NULL,NULL,NULL)",
            params![seq, &d.rfc_id, MAIN2],
        )?;
        assert!(store.infra_rfc(REPO, &d.rfc_id).is_err());
    }

    // Case 2: skipped state (APPROVED -> FROZEN_PROVING).
    {
        let tmp = tempfile::tempdir()?;
        let mut store = StateStore::open(tmp.path().join("t2.sqlite"))?;
        enroll(&mut store)?;
        let d = draft("RFC-tamper-2");
        let r = store.submit_infra_rfc(REPO, BASE, MODULE, &d, || Ok(()))?;
        store.approve_infra_rfc(REPO, BASE, &d.rfc_id, &r.draft_sha256, || Ok(()))?;
        let payload = transition_event_payload(
            MODULE,
            Engine::Lean4,
            &r.draft_sha256,
            InfraRfcStatus::Approved,
            InfraRfcStatus::FrozenProving,
            Some(SMOKE_RECEIPT),
            None,
            None,
        );
        let seq = insert_raw_event(&store.db, FROZEN, &d.rfc_id, MAIN2, ACTOR, &payload)?;
        store.db.execute(
            "INSERT INTO infra_rfc_transitions(event_seq,rfc_id,from_status,to_status,git_commit,evidence_sha256,superseded_by_rfc_id,reason)
             VALUES(?1,?2,'APPROVED','FROZEN_PROVING',?3,?4,NULL,NULL)",
            params![seq, &d.rfc_id, MAIN2, SMOKE_RECEIPT],
        )?;
        assert!(store.infra_rfc(REPO, &d.rfc_id).is_err());
    }

    // Case 3: mismatched event actor or payload.
    {
        let tmp = tempfile::tempdir()?;
        let mut store = StateStore::open(tmp.path().join("t3.sqlite"))?;
        enroll(&mut store)?;
        let d = draft("RFC-tamper-3");
        let r = store.submit_infra_rfc(REPO, BASE, MODULE, &d, || Ok(()))?;
        store.approve_infra_rfc(REPO, BASE, &d.rfc_id, &r.draft_sha256, || Ok(()))?;
        let payload = transition_event_payload(
            MODULE,
            Engine::Lean4,
            &r.draft_sha256,
            InfraRfcStatus::Approved,
            InfraRfcStatus::BuildingSmoke,
            None,
            None,
            None,
        );
        let seq = insert_raw_event(
            &store.db,
            SMOKE_STARTED,
            &d.rfc_id,
            MAIN2,
            "forged:actor",
            &payload,
        )?;
        store.db.execute(
            "INSERT INTO infra_rfc_transitions(event_seq,rfc_id,from_status,to_status,git_commit,evidence_sha256,superseded_by_rfc_id,reason)
             VALUES(?1,?2,'APPROVED','BUILDING_SMOKE',?3,NULL,NULL,NULL)",
            params![seq, &d.rfc_id, MAIN2],
        )?;
        assert!(store.infra_rfc(REPO, &d.rfc_id).is_err());
    }

    // Case 3b: tampered BUILDING_SMOKE with git_commit != reviewed_commit.
    {
        let tmp = tempfile::tempdir()?;
        let mut store = StateStore::open(tmp.path().join("t3b.sqlite"))?;
        enroll(&mut store)?;
        let d = draft("RFC-tamper-3b");
        let r = store.submit_infra_rfc(REPO, BASE, MODULE, &d, || Ok(()))?;
        store.approve_infra_rfc(REPO, BASE, &d.rfc_id, &r.draft_sha256, || Ok(()))?;
        let payload = transition_event_payload(
            MODULE,
            Engine::Lean4,
            &r.draft_sha256,
            InfraRfcStatus::Approved,
            InfraRfcStatus::BuildingSmoke,
            None,
            None,
            None,
        );
        let seq = insert_raw_event(&store.db, SMOKE_STARTED, &d.rfc_id, MAIN2, ACTOR, &payload)?;
        store.db.execute(
            "INSERT INTO infra_rfc_transitions(event_seq,rfc_id,from_status,to_status,git_commit,evidence_sha256,superseded_by_rfc_id,reason)
             VALUES(?1,?2,'APPROVED','BUILDING_SMOKE',?3,NULL,NULL,NULL)",
            params![seq, &d.rfc_id, MAIN2],
        )?;
        assert!(store.infra_rfc(REPO, &d.rfc_id).is_err());
    }

    // Case 4: missing evidence_sha256 on FROZEN_PROVING.
    {
        let tmp = tempfile::tempdir()?;
        let mut store = StateStore::open(tmp.path().join("t4.sqlite"))?;
        enroll(&mut store)?;
        let d = draft("RFC-tamper-4");
        let r = store.submit_infra_rfc(REPO, BASE, MODULE, &d, || Ok(()))?;
        store.approve_infra_rfc(REPO, BASE, &d.rfc_id, &r.draft_sha256, || Ok(()))?;
        store.start_infra_rfc_smoke(REPO, BASE, &d.rfc_id, &r.draft_sha256, || Ok(()))?;
        let payload = transition_event_payload(
            MODULE,
            Engine::Lean4,
            &r.draft_sha256,
            InfraRfcStatus::BuildingSmoke,
            InfraRfcStatus::FrozenProving,
            None,
            None,
            None,
        );
        let seq = insert_raw_event(&store.db, FROZEN, &d.rfc_id, MAIN3, ACTOR, &payload)?;
        store.db.execute(
            "INSERT INTO infra_rfc_transitions(event_seq,rfc_id,from_status,to_status,git_commit,evidence_sha256,superseded_by_rfc_id,reason)
             VALUES(?1,?2,'BUILDING_SMOKE','FROZEN_PROVING',?3,NULL,NULL,NULL)",
            params![seq, &d.rfc_id, MAIN3],
        )?;
        assert!(store.infra_rfc(REPO, &d.rfc_id).is_err());
    }

    // Case 5: second terminal transition after COMPLETED (terminal uniqueness).
    {
        let tmp = tempfile::tempdir()?;
        let mut store = StateStore::open(tmp.path().join("t5.sqlite"))?;
        enroll(&mut store)?;
        let rep = draft("RFC-rep-t5");
        store.submit_infra_rfc(REPO, BASE, MODULE, &rep, || Ok(()))?;
        let d = draft("RFC-tamper-5");
        let r = store.submit_infra_rfc(REPO, BASE, MODULE, &d, || Ok(()))?;
        store.approve_infra_rfc(REPO, BASE, &d.rfc_id, &r.draft_sha256, || Ok(()))?;
        store.start_infra_rfc_smoke(REPO, BASE, &d.rfc_id, &r.draft_sha256, || Ok(()))?;
        store.freeze_infra_rfc_verified(
            REPO,
            &d.rfc_id,
            &r.draft_sha256,
            MAIN3,
            MODULE,
            &d.proposed_definitions,
            SMOKE_RECEIPT,
            || Ok(()),
        )?;
        store.complete_infra_rfc_verified(
            REPO,
            &d.rfc_id,
            &r.draft_sha256,
            MAIN3,
            MODULE,
            &d.proposed_definitions,
            &d.required_api_lemmas,
            COMPLETE_RECEIPT,
            || Ok(()),
        )?;
        let payload = transition_event_payload(
            MODULE,
            Engine::Lean4,
            &r.draft_sha256,
            InfraRfcStatus::FrozenProving,
            InfraRfcStatus::Superseded,
            None,
            Some(&rep.rfc_id),
            Some("tampered post-terminal"),
        );
        let seq = insert_raw_event(&store.db, SUPERSEDED, &d.rfc_id, MAIN4, ACTOR, &payload)?;
        store.db.execute(
            "INSERT INTO infra_rfc_transitions(event_seq,rfc_id,from_status,to_status,git_commit,evidence_sha256,superseded_by_rfc_id,reason)
             VALUES(?1,?2,'FROZEN_PROVING','SUPERSEDED',?3,NULL,?4,'tampered post-terminal')",
            params![seq, &d.rfc_id, MAIN4, &rep.rfc_id],
        )?;
        assert!(store.infra_rfc(REPO, &d.rfc_id).is_err());
    }

    // Case 5b: tampered COMPLETED transition with git_commit != smoke_commit.
    {
        let tmp = tempfile::tempdir()?;
        let mut store = StateStore::open(tmp.path().join("t5b.sqlite"))?;
        enroll(&mut store)?;
        let d = draft("RFC-tamper-5b");
        let r = store.submit_infra_rfc(REPO, BASE, MODULE, &d, || Ok(()))?;
        store.approve_infra_rfc(REPO, BASE, &d.rfc_id, &r.draft_sha256, || Ok(()))?;
        store.start_infra_rfc_smoke(REPO, BASE, &d.rfc_id, &r.draft_sha256, || Ok(()))?;
        store.freeze_infra_rfc_verified(
            REPO,
            &d.rfc_id,
            &r.draft_sha256,
            MAIN3,
            MODULE,
            &d.proposed_definitions,
            SMOKE_RECEIPT,
            || Ok(()),
        )?;
        let payload = transition_event_payload(
            MODULE,
            Engine::Lean4,
            &r.draft_sha256,
            InfraRfcStatus::FrozenProving,
            InfraRfcStatus::Completed,
            Some(COMPLETE_RECEIPT),
            None,
            None,
        );
        let seq = insert_raw_event(&store.db, COMPLETED, &d.rfc_id, MAIN4, ACTOR, &payload)?;
        store.db.execute(
            "INSERT INTO infra_rfc_transitions(event_seq,rfc_id,from_status,to_status,git_commit,evidence_sha256,superseded_by_rfc_id,reason)
             VALUES(?1,?2,'FROZEN_PROVING','COMPLETED',?3,?4,NULL,NULL)",
            params![seq, &d.rfc_id, MAIN4, COMPLETE_RECEIPT],
        )?;
        assert!(store.infra_rfc(REPO, &d.rfc_id).is_err());
    }

    // A missing projection cannot look like a legacy ledger if an event remains.
    {
        let tmp = tempfile::tempdir()?;
        let db_path = tmp.path().join("orphan.sqlite");
        let mut store = StateStore::open(&db_path)?;
        enroll(&mut store)?;
        let draft = draft("RFC-orphan-transition");
        let submitted = store.submit_infra_rfc(REPO, BASE, MODULE, &draft, || Ok(()))?;
        store.reject_infra_rfc(
            REPO,
            BASE,
            &draft.rfc_id,
            &submitted.draft_sha256,
            "Declined",
            || Ok(()),
        )?;
        store.db.execute("DROP TABLE infra_rfc_transitions", [])?;
        drop(store);
        let ro = StateStore::open_readonly(&db_path)?;
        let error = ro.infra_rfc(REPO, &draft.rfc_id).unwrap_err();
        assert!(error.to_string().contains("missing projection rows"));
    }

    // Case 6: legacy Round 71 DB without infra_rfc_transitions table opened readonly.
    {
        let tmp = tempfile::tempdir()?;
        let db_path = tmp.path().join("legacy71.sqlite");
        let mut store = StateStore::open(&db_path)?;
        enroll(&mut store)?;
        let d_draft = draft("RFC-leg-draft");
        let r_draft = store.submit_infra_rfc(REPO, BASE, MODULE, &d_draft, || Ok(()))?;
        let d_app = draft("RFC-leg-approved");
        let r_app = store.submit_infra_rfc(REPO, BASE, MODULE, &d_app, || Ok(()))?;
        let a_app =
            store.approve_infra_rfc(REPO, BASE, &d_app.rfc_id, &r_app.draft_sha256, || Ok(()))?;
        store.db.execute("DROP TABLE infra_rfc_transitions", [])?;
        drop(store);

        let mut ro = StateStore::open_readonly(&db_path)?;
        assert_eq!(ro.infra_rfc(REPO, &d_draft.rfc_id)?, Some(r_draft));
        assert_eq!(ro.infra_rfc(REPO, &d_app.rfc_id)?, Some(a_app));
        let snap = ro.live_telemetry()?;
        assert_eq!(snap.infra_rfc_drafts, 1);
        assert_eq!(snap.infra_rfc_approved, 1);
        assert_eq!(snap.infra_rfc_building_smoke, 0);
        assert_eq!(snap.infra_rfc_frozen_proving, 0);
        assert_eq!(snap.infra_rfc_completed, 0);
    }

    Ok(())
}

#[test]
fn approved_exact_replay_ignores_consumer_drift_but_new_approval_rechecks_it() -> Result<()> {
    let mut store = StateStore::open(":memory:")?;
    enroll(&mut store)?;
    let approved_draft = draft("RFC-replay-approved");
    let submitted = store.submit_infra_rfc(REPO, BASE, MODULE, &approved_draft, || Ok(()))?;
    let approved = store.approve_infra_rfc(
        REPO,
        BASE,
        &approved_draft.rfc_id,
        &submitted.draft_sha256,
        || Ok(()),
    )?;
    let pending_draft = draft("RFC-replay-pending");
    let pending = store.submit_infra_rfc(REPO, BASE, MODULE, &pending_draft, || Ok(()))?;

    store.append_event(
        Change {
            actor: "fixture",
            git_commit: Some(BASE),
        },
        "CLAIM_NOTE",
        "claim",
        "Consumer",
        json!({"note": "consumer changed after approval"}),
    )?;
    let seq = store.status()?.last_event_seq;
    let confirmed = std::cell::Cell::new(false);
    assert_eq!(
        store.approve_infra_rfc(
            REPO,
            BASE,
            &approved_draft.rfc_id,
            &submitted.draft_sha256,
            || {
                confirmed.set(true);
                Ok(())
            },
        )?,
        approved
    );
    assert!(confirmed.get());
    assert_eq!(store.status()?.last_event_seq, seq);

    // Exact replay still rejects a different review scope or approval digest.
    assert!(
        store
            .approve_infra_rfc(
                REPO,
                MAIN2,
                &approved_draft.rfc_id,
                &submitted.draft_sha256,
                || Ok(()),
            )
            .is_err()
    );
    assert!(
        store
            .approve_infra_rfc(REPO, BASE, &approved_draft.rfc_id, &"0".repeat(64), || Ok(
                ()
            ),)
            .is_err()
    );
    let error = store
        .approve_infra_rfc(
            REPO,
            BASE,
            &pending_draft.rfc_id,
            &pending.draft_sha256,
            || Ok(()),
        )
        .unwrap_err();
    assert!(error.to_string().contains("consumer changed"));
    assert_eq!(store.status()?.last_event_seq, seq);
    Ok(())
}

#[test]
fn infra_rfc_read_projection_uses_one_snapshot_across_committed_transition() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let db = tmp.path().join("snapshot.sqlite");
    let mut store = StateStore::open(&db)?;
    enroll(&mut store)?;
    let d = draft("RFC-snapshot");
    let submitted = store.submit_infra_rfc(REPO, BASE, MODULE, &d, || Ok(()))?;
    let approved =
        store.approve_infra_rfc(REPO, BASE, &d.rfc_id, &submitted.draft_sha256, || Ok(()))?;
    let mut writer = StateStore::open(&db)?;
    let readonly = StateStore::open_readonly(&db)?;

    let tx = readonly.db.unchecked_transaction()?;
    assert_eq!(
        read_infra_rfc(&tx, REPO, &d.rfc_id)?,
        Some(approved.clone())
    );
    let building =
        writer.start_infra_rfc_smoke(REPO, BASE, &d.rfc_id, &submitted.draft_sha256, || Ok(()))?;
    // The already-open read snapshot cannot mix the old approval projection
    // with the newly committed BUILDING_SMOKE transition.
    assert_eq!(read_infra_rfc(&tx, REPO, &d.rfc_id)?, Some(approved));
    tx.commit()?;
    assert_eq!(readonly.infra_rfc(REPO, &d.rfc_id)?, Some(building));
    Ok(())
}

#[test]
fn fresh_smoke_start_rechecks_consumer_fingerprint_while_retry_stays_idempotent() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let mut store = StateStore::open(tmp.path().join("consumer_drift.sqlite"))?;
    enroll(&mut store)?;

    // Two approved RFCs sharing the same consumer Claim.
    let d_started = draft("RFC-drift-started");
    let r_started = store.submit_infra_rfc(REPO, BASE, MODULE, &d_started, || Ok(()))?;
    store.approve_infra_rfc(
        REPO,
        BASE,
        &d_started.rfc_id,
        &r_started.draft_sha256,
        || Ok(()),
    )?;
    let started = store.start_infra_rfc_smoke(
        REPO,
        BASE,
        &d_started.rfc_id,
        &r_started.draft_sha256,
        || Ok(()),
    )?;

    let d_pending = draft("RFC-drift-pending");
    let r_pending = store.submit_infra_rfc(REPO, BASE, MODULE, &d_pending, || Ok(()))?;
    store.approve_infra_rfc(
        REPO,
        BASE,
        &d_pending.rfc_id,
        &r_pending.draft_sha256,
        || Ok(()),
    )?;

    // Consumer Claim drifts after approval (a new claim event changes its consumer fingerprint).
    store.append_event(
        Change {
            actor: "fixture",
            git_commit: Some(BASE),
        },
        "CLAIM_NOTE",
        "claim",
        "Consumer",
        json!({"note": "consumer updated"}),
    )?;

    // Fresh smoke-start must reject the drifted consumer.
    assert!(
        store
            .start_infra_rfc_smoke(
                REPO,
                BASE,
                &d_pending.rfc_id,
                &r_pending.draft_sha256,
                || Ok(())
            )
            .is_err()
    );
    assert_eq!(
        store.infra_rfc(REPO, &d_pending.rfc_id)?.unwrap().status,
        InfraRfcStatus::Approved
    );

    // Already-started BUILDING_SMOKE remains idempotent on exact retry.
    assert_eq!(
        store.start_infra_rfc_smoke(
            REPO,
            BASE,
            &d_started.rfc_id,
            &r_started.draft_sha256,
            || Ok(())
        )?,
        started
    );
    Ok(())
}

#[test]
fn live_telemetry_counts_all_seven_states_accurately() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let mut store = StateStore::open(tmp.path().join("telemetry.sqlite"))?;
    enroll(&mut store)?;

    let check = |store: &mut StateStore,
                 drafts: i64,
                 approved: i64,
                 building: i64,
                 frozen: i64,
                 completed: i64|
     -> Result<()> {
        let snap = store.live_telemetry()?;
        assert_eq!(snap.infra_rfc_drafts, drafts, "drafts");
        assert_eq!(snap.infra_rfc_approved, approved, "approved");
        assert_eq!(snap.infra_rfc_building_smoke, building, "building_smoke");
        assert_eq!(snap.infra_rfc_frozen_proving, frozen, "frozen_proving");
        assert_eq!(snap.infra_rfc_completed, completed, "completed");
        Ok(())
    };

    check(&mut store, 0, 0, 0, 0, 0)?;

    // 1. Stays DRAFT
    let d_draft = draft("RFC-t-draft");
    store.submit_infra_rfc(REPO, BASE, MODULE, &d_draft, || Ok(()))?;
    check(&mut store, 1, 0, 0, 0, 0)?;

    // 2. Stays APPROVED
    let d_app = draft("RFC-t-approved");
    let r_app = store.submit_infra_rfc(REPO, BASE, MODULE, &d_app, || Ok(()))?;
    check(&mut store, 2, 0, 0, 0, 0)?;
    store.approve_infra_rfc(REPO, BASE, &d_app.rfc_id, &r_app.draft_sha256, || Ok(()))?;
    check(&mut store, 1, 1, 0, 0, 0)?;

    // 3. Stays BUILDING_SMOKE
    let d_smoke = draft("RFC-t-smoke");
    let r_smoke = store.submit_infra_rfc(REPO, BASE, MODULE, &d_smoke, || Ok(()))?;
    store.approve_infra_rfc(
        REPO,
        BASE,
        &d_smoke.rfc_id,
        &r_smoke.draft_sha256,
        || Ok(()),
    )?;
    check(&mut store, 1, 2, 0, 0, 0)?;
    store.start_infra_rfc_smoke(
        REPO,
        BASE,
        &d_smoke.rfc_id,
        &r_smoke.draft_sha256,
        || Ok(()),
    )?;
    check(&mut store, 1, 1, 1, 0, 0)?;

    // 4. Stays FROZEN_PROVING
    let d_frozen = draft("RFC-t-frozen");
    let r_frozen = store.submit_infra_rfc(REPO, BASE, MODULE, &d_frozen, || Ok(()))?;
    store.approve_infra_rfc(REPO, BASE, &d_frozen.rfc_id, &r_frozen.draft_sha256, || {
        Ok(())
    })?;
    store.start_infra_rfc_smoke(REPO, BASE, &d_frozen.rfc_id, &r_frozen.draft_sha256, || {
        Ok(())
    })?;
    store.freeze_infra_rfc_verified(
        REPO,
        &d_frozen.rfc_id,
        &r_frozen.draft_sha256,
        MAIN3,
        MODULE,
        &d_frozen.proposed_definitions,
        SMOKE_RECEIPT,
        || Ok(()),
    )?;
    check(&mut store, 1, 1, 1, 1, 0)?;

    // 5. Reaches COMPLETED
    let d_done = draft("RFC-t-done");
    let r_done = store.submit_infra_rfc(REPO, BASE, MODULE, &d_done, || Ok(()))?;
    store.approve_infra_rfc(REPO, BASE, &d_done.rfc_id, &r_done.draft_sha256, || Ok(()))?;
    store.start_infra_rfc_smoke(REPO, BASE, &d_done.rfc_id, &r_done.draft_sha256, || Ok(()))?;
    store.freeze_infra_rfc_verified(
        REPO,
        &d_done.rfc_id,
        &r_done.draft_sha256,
        MAIN3,
        MODULE,
        &d_done.proposed_definitions,
        SMOKE_RECEIPT,
        || Ok(()),
    )?;
    store.complete_infra_rfc_verified(
        REPO,
        &d_done.rfc_id,
        &r_done.draft_sha256,
        MAIN3,
        MODULE,
        &d_done.proposed_definitions,
        &d_done.required_api_lemmas,
        COMPLETE_RECEIPT,
        || Ok(()),
    )?;
    check(&mut store, 1, 1, 1, 1, 1)?;

    // 6. REJECTED from DRAFT, APPROVED, and BUILDING_SMOKE drop out of active counts.
    let d_rej_d = draft("RFC-t-rej-draft");
    let r_rej_d = store.submit_infra_rfc(REPO, BASE, MODULE, &d_rej_d, || Ok(()))?;
    check(&mut store, 2, 1, 1, 1, 1)?;
    store.reject_infra_rfc(
        REPO,
        BASE,
        &d_rej_d.rfc_id,
        &r_rej_d.draft_sha256,
        "no",
        || Ok(()),
    )?;
    check(&mut store, 1, 1, 1, 1, 1)?;

    let d_rej_a = draft("RFC-t-rej-app");
    let r_rej_a = store.submit_infra_rfc(REPO, BASE, MODULE, &d_rej_a, || Ok(()))?;
    store.approve_infra_rfc(
        REPO,
        BASE,
        &d_rej_a.rfc_id,
        &r_rej_a.draft_sha256,
        || Ok(()),
    )?;
    check(&mut store, 1, 2, 1, 1, 1)?;
    store.reject_infra_rfc(
        REPO,
        MAIN2,
        &d_rej_a.rfc_id,
        &r_rej_a.draft_sha256,
        "no",
        || Ok(()),
    )?;
    check(&mut store, 1, 1, 1, 1, 1)?;

    // 7. SUPERSEDED from FROZEN_PROVING drops out of frozen_proving count.
    let d_sup = draft("RFC-t-sup");
    let r_sup = store.submit_infra_rfc(REPO, BASE, MODULE, &d_sup, || Ok(()))?;
    store.approve_infra_rfc(REPO, BASE, &d_sup.rfc_id, &r_sup.draft_sha256, || Ok(()))?;
    store.start_infra_rfc_smoke(REPO, BASE, &d_sup.rfc_id, &r_sup.draft_sha256, || Ok(()))?;
    store.freeze_infra_rfc_verified(
        REPO,
        &d_sup.rfc_id,
        &r_sup.draft_sha256,
        MAIN3,
        MODULE,
        &d_sup.proposed_definitions,
        SMOKE_RECEIPT,
        || Ok(()),
    )?;
    check(&mut store, 1, 1, 1, 2, 1)?;
    store.supersede_infra_rfc(
        REPO,
        MAIN4,
        &d_sup.rfc_id,
        &r_sup.draft_sha256,
        &d_app.rfc_id,
        "replaced by RFC-t-approved",
        || Ok(()),
    )?;
    check(&mut store, 1, 1, 1, 1, 1)?;
    Ok(())
}
