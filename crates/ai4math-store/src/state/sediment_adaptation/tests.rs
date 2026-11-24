use super::*;
use crate::{NewClaim, PromotionIntent, ReviewedLeanSpec, VerifiedReceipt};

const REPO: &str = "/managed/project";
const ORIGIN: &str = "claim:origin";
const TARGET: &str = "claim:destination";
const ORIGIN_MODULE: &str = "Old.lean";
const TARGET_MODULE: &str = "New.lean";
const BASE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const PROMOTED: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const HEAD: &str = "cccccccccccccccccccccccccccccccccccccccc";
const CHANGE: Change<'_> = Change {
    actor: "test",
    git_commit: Some(HEAD),
};

fn binding(claim_id: &str) -> ReviewedLeanSpec {
    ReviewedLeanSpec {
        repo: REPO.into(),
        claim_id: claim_id.into(),
        approved_git_commit: BASE.into(),
        spec_file: "Spec.lean".into(),
        spec_decl: "reviewed".into(),
        spec_prefix_sha256: "1".repeat(64),
    }
}

fn register(store: &mut StateStore, claim_id: &str, file: &str, decl: &str) -> Result<()> {
    store.create_claim(
        NewClaim {
            id: claim_id,
            title: "Test Claim",
            informal_statement: "True",
            engine: "lean4",
            formal_target: Some(&format!("{file}#{decl}")),
            module_path: Some(file),
            critical: false,
        },
        CHANGE,
    )?;
    Ok(())
}

fn promotion(
    claim_id: &str,
    file: &str,
    decl: &str,
    commit: &str,
    digest: &str,
) -> (PromotionIntent, VerifiedReceipt) {
    let intent = PromotionIntent {
        repo: REPO.into(),
        claim_id: claim_id.into(),
        base_commit: BASE.into(),
        new_commit: commit.into(),
        file_path: file.into(),
        target_decl: decl.into(),
        proof_body: "by trivial".into(),
    };
    // Model the internal projection of a sealed broker result without invoking Lean.
    let receipt = VerifiedReceipt {
        id: digest.into(),
        issued_at: "2026-01-01T00:00:00Z".into(),
        tier: "T1_PROMOTION".into(),
        engine: "lean4".into(),
        base_commit: BASE.into(),
        patch_sha256: "2".repeat(64),
        target_decl: decl.into(),
        file_path: file.into(),
        allowlist_passed: true,
        axioms_used: vec![],
        external_checker_passed: None,
        compile_seconds: 0.1,
        max_heartbeats: None,
        receipt_sha256: digest.into(),
        raw: json!({"file_path": file}),
        regression_checked: true,
        lean_comparison: Some(reviewed_lean::ComparedSpec {
            spec_file: "Spec.lean".into(),
            spec_decl: "reviewed".into(),
            spec_prefix_sha256: "1".repeat(64),
            candidate_receipt_sha256: digest.into(),
            receipt_sha256: "3".repeat(64),
        }),
    };
    (intent, receipt)
}

fn install(store: &mut StateStore) -> Result<()> {
    register(store, ORIGIN, ORIGIN_MODULE, "origin")?;
    register(store, TARGET, TARGET_MODULE, "destination")?;
    store.bind_lean_spec(&binding(ORIGIN))?;
    let (intent, receipt) = promotion(ORIGIN, ORIGIN_MODULE, "origin", PROMOTED, &"4".repeat(64));
    store.prepare_promotion(&intent, &receipt)?;
    store.finish_promotion(&intent, &receipt)?;
    Ok(())
}

fn plan(seq: i64) -> SedimentAdaptationPlan {
    let spec = binding(TARGET);
    SedimentAdaptationPlan {
        archive_id: format!("SED-{}", "5".repeat(64)),
        origin_claim: ORIGIN.into(),
        promoted_commit: PROMOTED.into(),
        target_claim: TARGET.into(),
        reviewed_commit: HEAD.into(),
        repo: REPO.into(),
        target_module: TARGET_MODULE.into(),
        target_decl: "destination".into(),
        frozen_source_sha256: "6".repeat(64),
        spec_file: spec.spec_file,
        spec_decl: spec.spec_decl,
        spec_prefix_sha256: spec.spec_prefix_sha256,
        expected_event_seq: seq,
    }
}

fn assert_rejected(
    store: &mut StateStore,
    plan: &SedimentAdaptationPlan,
    message: &str,
) -> Result<()> {
    let before = last_seq(&store.db)?;
    let approval = plan.approval_sha256()?;
    let error = store
        .approve_sediment_adaptation(plan, &approval)
        .unwrap_err();
    assert!(error.to_string().contains(message), "{error:#}");
    assert_eq!(last_seq(&store.db)?, before);
    assert_eq!(store.sediment_adaptation(&approval)?, None);
    Ok(())
}

#[test]
fn approval_hash_binds_plan_and_sequence() -> Result<()> {
    let plan = plan(42);
    let digest = plan.approval_sha256()?;
    let roundtrip: SedimentAdaptationPlan = serde_json::from_str(&serde_json::to_string(&plan)?)?;
    assert_eq!(roundtrip, plan);
    assert_eq!(roundtrip.approval_sha256()?, digest);
    assert!(valid_id(&digest));
    assert_ne!(
        digest,
        format!("{:x}", Sha256::digest(serde_json::to_vec(&plan)?))
    );
    for changed in [
        SedimentAdaptationPlan {
            expected_event_seq: 43,
            ..plan.clone()
        },
        SedimentAdaptationPlan {
            frozen_source_sha256: "7".repeat(64),
            ..plan.clone()
        },
        SedimentAdaptationPlan {
            target_decl: "different".into(),
            ..plan.clone()
        },
    ] {
        assert_ne!(changed.approval_sha256()?, digest);
    }
    for invalid in [
        SedimentAdaptationPlan {
            archive_id: format!("SED-{}", "A".repeat(64)),
            ..plan.clone()
        },
        SedimentAdaptationPlan {
            reviewed_commit: "abc123".into(),
            ..plan.clone()
        },
        SedimentAdaptationPlan {
            promoted_commit: "7".repeat(64),
            ..plan.clone()
        },
        SedimentAdaptationPlan {
            target_claim: ORIGIN.into(),
            ..plan.clone()
        },
        SedimentAdaptationPlan {
            target_claim: "x".repeat(257),
            ..plan.clone()
        },
        SedimentAdaptationPlan {
            target_module: "../New.lean".into(),
            ..plan.clone()
        },
        SedimentAdaptationPlan {
            target_decl: "New#unchecked".into(),
            ..plan.clone()
        },
        SedimentAdaptationPlan {
            spec_prefix_sha256: "G".repeat(64),
            ..plan.clone()
        },
        SedimentAdaptationPlan {
            expected_event_seq: -1,
            ..plan
        },
    ] {
        assert!(invalid.approval_sha256().is_err());
    }
    Ok(())
}

#[test]
fn approval_is_atomic_immutable_and_never_proof_or_task_authority() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let db = temp.path().join("state.sqlite");
    let mut store = StateStore::open(&db)?;
    install(&mut store)?;
    store.bind_lean_spec(&binding(TARGET))?;
    // A stale origin can supply an advisory input; only the destination must be current.
    store.mark_t3(&[ORIGIN], &[], CHANGE)?;
    assert!(store.claim(ORIGIN)?.unwrap().stale);
    let stale_plan = plan(last_seq(&store.db)?);
    let drift =
        StateStore::open(&db)?.append_event(CHANGE, "TEST_DRIFT", "claim", TARGET, json!({}))?;
    assert_rejected(&mut store, &stale_plan, "ledger event sequence changed")?;

    let plan = plan(drift);
    let digest = plan.approval_sha256()?;
    assert!(
        store
            .approve_sediment_adaptation(&plan, &"0".repeat(64))
            .is_err()
    );
    assert!(
        store
            .append_event(CHANGE, APPROVED, "claim", TARGET, json!({}))
            .is_err()
    );
    store.db.execute_batch(
        "CREATE TRIGGER fail_approval_row BEFORE INSERT ON sediment_adaptation_approvals
         BEGIN SELECT RAISE(ABORT, 'fixture row failure'); END;",
    )?;
    assert_rejected(&mut store, &plan, "fixture row failure")?;
    store.db.execute_batch("DROP TRIGGER fail_approval_row")?;

    let claim_before = (
        serde_json::to_value(store.claim(ORIGIN)?)?,
        serde_json::to_value(store.claim(TARGET)?)?,
    );
    let record = store.approve_sediment_adaptation(&plan, &digest)?;
    assert_eq!(record.approval_sha256, digest);
    assert_eq!(record.plan, plan);
    assert_eq!(store.approve_sediment_adaptation(&plan, &digest)?, record);
    assert_eq!(last_seq(&store.db)?, record.event_seq);
    let events = store.events_after(drift, 10)?;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event_type, APPROVED);
    assert_eq!(events[0].event_seq, record.event_seq);
    assert_eq!(events[0].entity_id, TARGET);
    assert_eq!(events[0].payload["approval_sha256"], digest);
    assert_eq!(events[0].git_commit.as_deref(), Some(HEAD));
    assert_eq!(
        (
            serde_json::to_value(store.claim(ORIGIN)?)?,
            serde_json::to_value(store.claim(TARGET)?)?
        ),
        claim_before
    );
    let counts = store.db.query_row(
        "SELECT (SELECT count(*) FROM verification_receipts),
                (SELECT count(*) FROM lean_spec_rechecks),
                (SELECT count(*) FROM scheduler_tasks),
                (SELECT count(*) FROM scheduler_task_candidates)",
        [],
        |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
            ))
        },
    )?;
    assert_eq!(counts, (1, 0, 0, 0));
    for sql in [
        "UPDATE sediment_adaptation_approvals SET repo='/other'",
        "DELETE FROM sediment_adaptation_approvals",
        "INSERT OR REPLACE INTO sediment_adaptation_approvals SELECT * FROM sediment_adaptation_approvals",
    ] {
        assert!(
            store
                .db
                .execute(sql, [])
                .unwrap_err()
                .to_string()
                .contains("immutable")
        );
    }
    assert_eq!(
        StateStore::open_readonly(db)?.sediment_adaptation(&digest)?,
        Some(record)
    );
    Ok(())
}

#[test]
fn append_requires_exact_historical_and_destination_identities() -> Result<()> {
    let mut store = StateStore::open(":memory:")?;
    install(&mut store)?;
    let unbound = plan(last_seq(&store.db)?);
    assert_rejected(&mut store, &unbound, "no reviewed Lean spec binding")?;
    store.bind_lean_spec(&binding(TARGET))?;
    let plan = plan(last_seq(&store.db)?);
    for (invalid, error) in [
        (
            SedimentAdaptationPlan {
                repo: "/other".into(),
                ..plan.clone()
            },
            "bound managed repository",
        ),
        (
            SedimentAdaptationPlan {
                promoted_commit: HEAD.into(),
                ..plan.clone()
            },
            "exact historical",
        ),
        (
            SedimentAdaptationPlan {
                spec_decl: "different".into(),
                ..plan.clone()
            },
            "historical Lean comparison",
        ),
        (
            SedimentAdaptationPlan {
                target_decl: "different".into(),
                ..plan.clone()
            },
            "registered Claim module and declaration",
        ),
        (
            SedimentAdaptationPlan {
                target_module: "Wrong.lean".into(),
                ..plan.clone()
            },
            "registered Claim module and declaration",
        ),
    ] {
        assert_rejected(&mut store, &invalid, error)?;
    }
    // Mutate only test fixture rows to model edge conditions independently of CAS.
    store.db.execute("UPDATE claims SET module_path='OLD.lean',formal_target='OLD.lean#origin' WHERE claim_id=?1", [TARGET])?;
    let alias = SedimentAdaptationPlan {
        target_module: "OLD.lean".into(),
        target_decl: "origin".into(),
        ..plan.clone()
    };
    assert_rejected(&mut store, &alias, "distinct physical Lean target")?;
    store.db.execute("UPDATE claims SET module_path='New.lean',formal_target='New.lean#destination' WHERE claim_id=?1", [TARGET])?;
    for setting in [
        "stale=1",
        "effective_stale=1",
        "local_trust_level='L2b'",
        "effective_trust_level='L2b'",
        "effective_trust_level='L3'",
    ] {
        store.db.execute(
            &format!("UPDATE claims SET {setting} WHERE claim_id=?1"),
            [TARGET],
        )?;
        assert_rejected(&mut store, &plan, "stale, refuted or inactive")?;
        store.db.execute("UPDATE claims SET stale=0,effective_stale=0,local_trust_level='L0',effective_trust_level='L0' WHERE claim_id=?1", [TARGET])?;
    }
    store.db.execute(
        "UPDATE claims SET effective_trust_level='L2b' WHERE claim_id=?1",
        [ORIGIN],
    )?;
    assert_rejected(&mut store, &plan, "origin Claim is refuted")?;
    store.db.execute(
        "UPDATE claims SET effective_trust_level='L5' WHERE claim_id=?1",
        [ORIGIN],
    )?;

    // A pending promotion does not itself advance the sequence.
    let (intent, receipt) = promotion(TARGET, TARGET_MODULE, "destination", HEAD, &"8".repeat(64));
    store.prepare_promotion(&intent, &receipt)?;
    assert_rejected(&mut store, &plan, "pending promotion or Pivot")?;
    store.finish_promotion(&intent, &receipt)?;
    let plan = SedimentAdaptationPlan {
        expected_event_seq: last_seq(&store.db)?,
        ..plan
    };
    assert_rejected(&mut store, &plan, "already has a managed proof recipe")?;

    store.db.execute(
        "UPDATE verification_receipts SET raw_receipt_json=?1 WHERE receipt_id=?2",
        params![
            json!({"file_path":"Wrong.lean"}).to_string(),
            "4".repeat(64)
        ],
    )?;
    assert_rejected(
        &mut store,
        &plan,
        "does not identify the historical managed T1 proof receipt",
    )?;
    Ok(())
}

#[test]
fn absent_target_spec_identity_and_legacy_readback_fail_closed() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let db = temp.path().join("state.sqlite");
    Connection::open(&db)?;
    let legacy = StateStore::open_readonly(&db)?;
    let id = "1".repeat(64);
    assert_eq!(legacy.sediment_adaptation(&id)?, None);
    assert!(legacy.sediment_adaptation("not-a-token").is_err());
    assert!(!legacy.db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name=?1)",
        [TABLE],
        |r| r.get::<_, bool>(0),
    )?);
    drop(legacy);

    let mut store = StateStore::open(db)?;
    install(&mut store)?;
    let mut other = binding(TARGET);
    other.spec_prefix_sha256 = "9".repeat(64);
    store.bind_lean_spec(&other)?;
    let plan = plan(last_seq(&store.db)?);
    assert_rejected(
        &mut store,
        &plan,
        "reviewed Lean spec binding for claim:destination does not match",
    )?;
    Ok(())
}
