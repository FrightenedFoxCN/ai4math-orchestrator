use super::*;
use crate::{NewClaim, PromotionIntent, VerifiedReceipt};
use ai4math_core::{FrozenTarget, ProofBodySubmission, render_proof_body};

const REPO: &str = "/managed/project";
const CLAIM: &str = "claim:target";
const FILE: &str = "Proof.lean";
const BASE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const PROMOTED: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const HEAD: &str = "cccccccccccccccccccccccccccccccccccccccc";
const ARCHIVE: &str = "SED-1111111111111111111111111111111111111111111111111111111111111111";
const T1: &str = "2222222222222222222222222222222222222222222222222222222222222222";
const T3: &str = "3333333333333333333333333333333333333333333333333333333333333333";
const COMPARISON: &str = "4444444444444444444444444444444444444444444444444444444444444444";
const EMPTY_DEPENDENCIES: &str = "53d6c8644575b13a8814f4cd42c27f935ea28d030d4da4cebbb9eb80d4b060c0";
fn target_and_proof() -> (FrozenTarget, ProofBodySubmission) {
    (
        FrozenTarget {
            engine: Engine::Lean4,
            prefix: "theorem target : True := ".into(),
            suffix: "\n".into(),
            target_id: "target".into(),
            file_path: FILE.into(),
        },
        ProofBodySubmission {
            target_id: "target".into(),
            proof_body: "by trivial".into(),
        },
    )
}

fn scope(expected_event_seq: i64) -> SedimentReplayScope<'static> {
    SedimentReplayScope {
        repo: REPO,
        archive_id: ARCHIVE,
        claim_id: CLAIM,
        promoted_commit: PROMOTED,
        reviewed_commit: HEAD,
        expected_event_seq,
    }
}

fn install_historical_promotion(store: &mut StateStore) -> Result<()> {
    store.create_claim(
        NewClaim {
            id: CLAIM,
            title: "Target",
            informal_statement: "True",
            engine: "lean4",
            formal_target: Some("Proof.lean#target"),
            module_path: Some(FILE),
            critical: false,
        },
        Change {
            actor: "fixture",
            git_commit: Some(BASE),
        },
    )?;
    let intent = PromotionIntent {
        repo: REPO.into(),
        claim_id: CLAIM.into(),
        base_commit: BASE.into(),
        new_commit: PROMOTED.into(),
        file_path: FILE.into(),
        target_decl: "target".into(),
        proof_body: "by trivial".into(),
    };
    let (target, proof) = target_and_proof();
    let source = render_proof_body(&target, &proof)?;
    let receipt = VerifiedReceipt {
        id: T1.into(),
        issued_at: "2026-01-01T00:00:00Z".into(),
        tier: "T1_PROMOTION".into(),
        engine: "lean4".into(),
        base_commit: BASE.into(),
        patch_sha256: format!("{:x}", Sha256::digest(source.as_bytes())),
        target_decl: "target".into(),
        file_path: FILE.into(),
        allowlist_passed: true,
        axioms_used: vec![],
        external_checker_passed: None,
        compile_seconds: 0.1,
        max_heartbeats: None,
        receipt_sha256: T1.into(),
        raw: json!({"file_path": FILE,"dependencies_sha256": EMPTY_DEPENDENCIES}),
        regression_checked: true,
        broker_assessed_t3: false,
        lean_comparison: None,
    };
    store.prepare_promotion(&intent, &receipt)?;
    store.finish_promotion(&intent, &receipt)?;
    Ok(())
}

#[test]
fn retention_is_atomic_immutable_idempotent_and_not_proof_authority() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let db = temp.path().join("state.sqlite");
    let mut store = StateStore::open(&db)?;
    install_historical_promotion(&mut store)?;
    store.mark_t3(
        &[CLAIM],
        &[],
        Change {
            actor: "test",
            git_commit: Some(HEAD),
        },
    )?;
    let claim_before = serde_json::to_value(store.claim(CLAIM)?)?;
    let before = last_seq(&store.db)?;
    let before_scope = scope(before);

    // A row failure must roll back the event. This helper starts after bound
    // broker validation, and uses synthetic digests so this runs without Lean.
    store.db.execute_batch(
        "CREATE TRIGGER fail_sediment_row BEFORE INSERT ON sediment_replays
         BEGIN SELECT RAISE(ABORT, 'write failure fixture'); END;",
    )?;
    let error = store
        .write(|tx| retain_bound_replay(tx, before_scope, T3, None))
        .unwrap_err();
    assert!(error.to_string().contains("write failure fixture"));
    assert_eq!(last_seq(&store.db)?, before);
    store.db.execute_batch("DROP TRIGGER fail_sediment_row")?;

    let retained = store.write(|tx| retain_bound_replay(tx, before_scope, T3, None))?;
    let after = last_seq(&store.db)?;
    assert_eq!(retained.event_seq, after);
    assert_eq!(retained.expected_event_seq, before);
    assert_eq!(retained.archive_id, ARCHIVE);
    assert_eq!(retained.candidate_receipt_sha256, T3);
    assert!(retained.comparison_receipt_sha256.is_none());
    assert!(valid_id(&retained.id));
    assert_eq!(
        store.write(|tx| retain_bound_replay(tx, before_scope, T3, None))?,
        retained
    );
    assert_eq!(
        store.write(|tx| retain_bound_replay(tx, scope(after), T3, None))?,
        retained
    );
    for (candidate, comparison) in [(COMPARISON, None), (T3, Some(COMPARISON))] {
        let error = store
            .write(|tx| retain_bound_replay(tx, before_scope, candidate, comparison))
            .unwrap_err();
        assert!(error.to_string().contains("ledger event sequence changed"));
    }
    assert_eq!(last_seq(&store.db)?, after);
    let events = store.events_after(before, 10)?;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event_type, RETAINED);
    assert_eq!(events[0].git_commit.as_deref(), Some(HEAD));
    assert_eq!(events[0].entity_id, CLAIM);
    assert_eq!(events[0].payload["id"], retained.id);
    assert_eq!(serde_json::to_value(store.claim(CLAIM)?)?, claim_before);
    let authority_counts = store.db.query_row(
        "SELECT (SELECT count(*) FROM verification_receipts),
                (SELECT count(*) FROM lean_spec_rechecks)",
        [],
        |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
    )?;
    assert_eq!(authority_counts, (1, 0));

    assert!(
        store
            .append_event(
                Change {
                    actor: "test",
                    git_commit: Some(HEAD)
                },
                RETAINED,
                "claim",
                CLAIM,
                json!({})
            )
            .is_err()
    );
    for sql in [
        "UPDATE sediment_replays SET repo='other'",
        "DELETE FROM sediment_replays",
        "INSERT OR REPLACE INTO sediment_replays SELECT * FROM sediment_replays",
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
    let readonly = StateStore::open_readonly(db)?;
    assert_eq!(readonly.sediment_replay(&retained.id)?, Some(retained));
    Ok(())
}

#[test]
fn exact_t1_lineage_checks_the_proof_prefix_or_comparison_receipt() -> Result<()> {
    let mut store = StateStore::open(":memory:")?;
    install_historical_promotion(&mut store)?;
    let recipe = store.managed_proof_recipe_at(CLAIM, PROMOTED)?.unwrap();
    assert!(reviewed_lean::matches_managed_t1(
        &store.db,
        &recipe,
        None,
        Some(EMPTY_DEPENDENCIES)
    )?);
    assert!(!reviewed_lean::matches_managed_t1(
        &store.db,
        &recipe,
        None,
        Some(T3)
    )?);
    let mut original = crate::LeanSpecComparison {
        promoted_commit: PROMOTED.into(),
        claim_id: CLAIM.into(),
        base_commit: BASE.into(),
        spec_file: "Spec.lean".into(),
        spec_decl: "reviewed".into(),
        spec_prefix_sha256: COMPARISON.into(),
        candidate_receipt_sha256: T1.into(),
        receipt_sha256: COMPARISON.into(),
    };
    assert!(store.managed_lean_t1_matches_comparison(&recipe, &original)?);
    original.candidate_receipt_sha256 = T3.into();
    assert!(!store.managed_lean_t1_matches_comparison(&recipe, &original)?);
    Ok(())
}

#[test]
fn legacy_readonly_does_not_create_retention_table() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let db = temp.path().join("state.sqlite");
    Connection::open(&db)?;
    let readonly = StateStore::open_readonly(&db)?;
    assert!(readonly.sediment_replay(T3)?.is_none());
    assert!(readonly.sediment_replay(ARCHIVE).is_err());
    assert!(readonly.sediment_replay(&"A".repeat(64)).is_err());
    assert!(!readonly.db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='sediment_replays')",
        [],
        |r| r.get::<_, bool>(0)
    )?);
    Ok(())
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "requires a native host (no outer Seatbelt) and AI4MATH_TEST_LEAN"]
fn native_retention_rejects_legacy_t3_and_unrelated_bindings() -> Result<()> {
    use ai4math_broker::{Broker, RecheckAssessment, Toolchain};

    let lean = std::env::var_os("AI4MATH_TEST_LEAN").context("set native Lean binary")?;
    let tool = Toolchain::lean(lean)?;
    let broker = Broker::new();
    let (target, proof) = target_and_proof();
    let mut store = StateStore::open(":memory:")?;
    install_historical_promotion(&mut store)?;
    let before = last_seq(&store.db)?;
    let legacy = broker.recheck_with_dependencies(&tool, &target, &proof, HEAD, &[])?;
    assert_eq!(legacy.receipt().tier(), ReceiptTier::T3TaintRebuild);
    assert!(!legacy.is_assessed_t3());
    let error = store
        .retain_sediment_replay(scope(before), &legacy, None)
        .unwrap_err();
    assert!(error.to_string().contains("natively assessed Lean T3"));

    let assessment = broker.assess_recheck_with_dependencies(&tool, &target, &proof, HEAD, &[]);
    let RecheckAssessment::Verified(candidate) = assessment else {
        anyhow::bail!("expected successful native assessment: {assessment:?}");
    };
    for (invalid, message) in [
        (
            SedimentReplayScope {
                repo: "/other",
                ..scope(before)
            },
            "bound managed repository",
        ),
        (
            SedimentReplayScope {
                promoted_commit: BASE,
                ..scope(before)
            },
            "exact historical",
        ),
        (
            SedimentReplayScope {
                reviewed_commit: PROMOTED,
                ..scope(before)
            },
            "does not match the reviewed commit",
        ),
        (scope(before - 1), "ledger event sequence changed"),
    ] {
        let error = store
            .retain_sediment_replay(invalid, &candidate, None)
            .unwrap_err();
        assert!(error.to_string().contains(message), "{error:#}");
        assert_eq!(last_seq(&store.db)?, before);
    }
    let assessment = broker.assess_recheck_with_dependencies(
        &tool,
        &target,
        &proof,
        HEAD,
        &[("Extra.lean", "def unused : Nat := 1\n")],
    );
    let RecheckAssessment::Verified(changed_prefix) = assessment else {
        anyhow::bail!("expected successful native assessment: {assessment:?}");
    };
    let error = store
        .retain_sediment_replay(scope(before), &changed_prefix, None)
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("managed T1 proof and dependency prefix")
    );
    assert_eq!(last_seq(&store.db)?, before);
    let retained = store.retain_sediment_replay(scope(before), &candidate, None)?;
    assert_eq!(
        store.retain_sediment_replay(scope(before), &candidate, None)?,
        retained
    );
    assert_eq!(last_seq(&store.db)?, retained.event_seq);
    Ok(())
}
