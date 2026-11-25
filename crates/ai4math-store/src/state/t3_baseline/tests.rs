use super::*;
use crate::{
    Change, NewClaim, PromotionIntent, RecheckOutcome, ReviewedLeanSpec, T3PlanApproval,
    VerifiedReceipt,
};
use ai4math_broker::lean_breaker_sources_sha256;
use rusqlite::params;
use serde_json::json;

const CLAIM: &str = "claim:baseline";
const FILE: &str = "Proof.lean";
const DECL: &str = "goal";
const BASE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const FIRST: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const SECOND: &str = "cccccccccccccccccccccccccccccccccccccccc";
const THIRD: &str = "dddddddddddddddddddddddddddddddddddddddd";
const FOURTH: &str = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";

fn register(store: &mut StateStore) -> Result<()> {
    store.create_claim(
        NewClaim {
            id: CLAIM,
            title: "Claim",
            informal_statement: "True",
            engine: "lean4",
            formal_target: Some("Proof.lean#goal"),
            module_path: Some(FILE),
            critical: false,
        },
        Change {
            actor: "fixture",
            git_commit: Some(BASE),
        },
    )
}

// Private projection fixture: exercise the real T1 store transaction without
// requiring Git or a platform-specific Lean executable.
fn receipt(tier: &str, base: &str, nonce: u32) -> VerifiedReceipt {
    let receipt_sha256 = format!("{nonce:064x}");
    let patch_sha256 = "f".repeat(64);
    VerifiedReceipt {
        id: receipt_sha256.clone(),
        issued_at: "2026-01-01T00:00:00Z".into(),
        tier: tier.into(),
        engine: "lean4".into(),
        base_commit: base.into(),
        patch_sha256: patch_sha256.clone(),
        target_decl: DECL.into(),
        file_path: FILE.into(),
        allowlist_passed: true,
        axioms_used: vec![],
        external_checker_passed: None,
        compile_seconds: 0.1,
        max_heartbeats: None,
        receipt_sha256: receipt_sha256.clone(),
        raw: json!({
            "engine": "lean4", "tier": tier, "base_commit": base,
            "file_path": FILE, "target_id": DECL, "patch_sha256": patch_sha256,
            "receipt_sha256": receipt_sha256, "dependencies_sha256": lean_breaker_sources_sha256(&[]),
        }),
        regression_checked: tier == "T1_PROMOTION",
        broker_assessed_t3: tier == "T3_TAINT_REBUILD",
        lean_comparison: None,
    }
}

fn promote(
    store: &mut StateStore,
    repo: &str,
    promoted: &str,
    receipt: &VerifiedReceipt,
) -> Result<()> {
    let intent = PromotionIntent {
        repo: repo.into(),
        claim_id: CLAIM.into(),
        base_commit: receipt.base_commit.clone(),
        new_commit: promoted.into(),
        file_path: FILE.into(),
        target_decl: DECL.into(),
        proof_body: "by trivial".into(),
    };
    store.prepare_promotion(&intent, receipt)?;
    store.finish_promotion(&intent, receipt)?;
    Ok(())
}

#[test]
fn exact_latest_t1_is_claim_scoped_and_read_only() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let repo = temp.path().to_str().unwrap();
    let db = temp.path().join("state.sqlite");
    let mut store = StateStore::open(&db)?;
    register(&mut store)?;
    assert_eq!(store.t3_discovery_baseline(CLAIM)?, None);
    let first = receipt("T1_PROMOTION", BASE, 1);
    promote(&mut store, repo, FIRST, &first)?;
    let latest = receipt("T1_PROMOTION", FIRST, 2);
    promote(&mut store, repo, SECOND, &latest)?;

    let readonly = StateStore::open_readonly(&db)?;
    let before = readonly.status()?.last_event_seq;
    let expected = T3DiscoveryBaseline {
        claim_id: CLAIM.into(),
        repo: repo.into(),
        engine: "lean4".into(),
        recipe: ManagedProofRecipe {
            claim_id: CLAIM.into(),
            base_commit: FIRST.into(),
            promoted_commit: SECOND.into(),
            file_path: FILE.into(),
            target_decl: DECL.into(),
            proof_body: "by trivial".into(),
            source_sha256: latest.patch_sha256.clone(),
        },
        receipt_sha256: latest.receipt_sha256.clone(),
        dependencies_sha256: lean_breaker_sources_sha256(&[]),
        verified_at_commit: SECOND.into(),
        baseline_scope: "claim_t1",
    };
    assert_eq!(readonly.t3_discovery_baseline(CLAIM)?, Some(expected));
    assert_eq!(readonly.t3_discovery_baseline("claim:other")?, None);
    assert_eq!(readonly.status()?.last_event_seq, before);

    store.db.execute(
        "UPDATE claims SET receipt_id=?1 WHERE claim_id=?2",
        params![first.id, CLAIM],
    )?;
    assert_eq!(readonly.t3_discovery_baseline(CLAIM)?, None);
    store.db.execute(
        "DELETE FROM managed_proof_recipes WHERE git_commit=?1",
        [SECOND],
    )?;
    assert_eq!(readonly.t3_discovery_baseline(CLAIM)?, None);
    Ok(())
}

#[test]
fn rejects_ineligible_current_claim_and_plain_t3_replacement() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let mut store = StateStore::open(":memory:")?;
    register(&mut store)?;
    let t1 = receipt("T1_PROMOTION", BASE, 1);
    promote(&mut store, temp.path().to_str().unwrap(), FIRST, &t1)?;

    for (field, invalid, valid) in [
        ("stale", "1", "0"),
        ("effective_stale", "1", "0"),
        ("local_trust_level", "L3", "L5"),
        ("effective_trust_level", "L3", "L5"),
        ("module_path", "Other.lean", FILE),
        ("formal_target", "Proof.lean#other", "Proof.lean#goal"),
        ("engine", "rocq", "lean4"),
    ] {
        let update = format!("UPDATE claims SET {field}=?1 WHERE claim_id=?2");
        store.db.execute(&update, [invalid, CLAIM])?;
        assert_eq!(store.t3_discovery_baseline(CLAIM)?, None, "{field}");
        store.db.execute(&update, [valid, CLAIM])?;
    }
    assert!(store.t3_discovery_baseline(CLAIM)?.is_some());
    let t3 = receipt("T3_TAINT_REBUILD", FIRST, 3);
    store.record_verified(
        CLAIM,
        &t3,
        Change {
            actor: "fixture",
            git_commit: Some(FIRST),
        },
    )?;
    assert_eq!(store.claim(CLAIM)?.unwrap().receipt_id, Some(t3.id));
    assert_eq!(store.t3_discovery_baseline(CLAIM)?, None);
    Ok(())
}

#[test]
fn rejects_receipt_json_mismatches_and_noncanonical_dependencies_digest() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let mut store = StateStore::open(":memory:")?;
    register(&mut store)?;
    let t1 = receipt("T1_PROMOTION", BASE, 1);
    promote(&mut store, temp.path().to_str().unwrap(), FIRST, &t1)?;
    let update = "UPDATE verification_receipts SET raw_receipt_json=?1 WHERE receipt_id=?2";
    for (field, wrong) in [
        ("engine", "rocq".into()),
        ("tier", "T3_TAINT_REBUILD".into()),
        ("base_commit", SECOND.into()),
        ("file_path", "Other.lean".into()),
        ("target_id", "other".into()),
        ("patch_sha256", "1".repeat(64)),
        ("receipt_sha256", "2".repeat(64)),
        ("dependencies_sha256", "A".repeat(64)),
    ] {
        let mut raw = t1.raw.clone();
        raw[field] = json!(wrong);
        store.db.execute(update, [raw.to_string(), t1.id.clone()])?;
        assert_eq!(store.t3_discovery_baseline(CLAIM)?, None, "{field}");
    }
    store.db.execute(update, ["not JSON", &t1.id])?;
    assert_eq!(store.t3_discovery_baseline(CLAIM)?, None);
    store.db.execute(update, [t1.raw.to_string(), t1.id])?;
    assert!(store.t3_discovery_baseline(CLAIM)?.is_some());
    Ok(())
}

fn bind_reviewed_proof(store: &mut StateStore, repo: &str) -> Result<ReviewedLeanSpec> {
    register(store)?;
    let spec = ReviewedLeanSpec {
        repo: repo.into(),
        claim_id: CLAIM.into(),
        approved_git_commit: BASE.into(),
        spec_file: "Spec.lean".into(),
        spec_decl: "reviewedGoal".into(),
        spec_prefix_sha256: "a".repeat(64),
    };
    store.bind_lean_spec(&spec)?;
    Ok(spec)
}

fn compared_receipt(
    spec: &ReviewedLeanSpec,
    tier: &str,
    base: &str,
    nonce: u32,
) -> VerifiedReceipt {
    let mut candidate = receipt(tier, base, nonce);
    candidate.lean_comparison = Some(reviewed_lean::ComparedSpec {
        spec_file: spec.spec_file.clone(),
        spec_decl: spec.spec_decl.clone(),
        spec_prefix_sha256: spec.spec_prefix_sha256.clone(),
        candidate_receipt_sha256: candidate.receipt_sha256.clone(),
        receipt_sha256: format!("{:064x}", nonce + 1000),
    });
    candidate
}

#[test]
fn reviewed_binding_requires_original_exact_sealed_comparison() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let repo = temp.path().to_str().unwrap();
    let mut store = StateStore::open(":memory:")?;
    let spec = bind_reviewed_proof(&mut store, repo)?;
    let t1 = compared_receipt(&spec, "T1_PROMOTION", BASE, 1);
    promote(&mut store, repo, FIRST, &t1)?;
    assert_eq!(
        store.t3_discovery_baseline(CLAIM)?.unwrap().receipt_sha256,
        t1.receipt_sha256
    );

    // Simulate inconsistent/corrupt retained evidence despite immutable DDL.
    store
        .db
        .execute_batch("DROP TRIGGER lean_spec_comparisons_no_update;")?;
    store.db.execute(
        "UPDATE lean_spec_comparisons SET spec_decl='other' WHERE git_commit=?1",
        [FIRST],
    )?;
    assert_eq!(store.t3_discovery_baseline(CLAIM)?, None);
    store.db.execute("UPDATE lean_spec_comparisons SET spec_decl='reviewedGoal',receipt_sha256=?1 WHERE git_commit=?2", params!["d".repeat(64), FIRST])?;
    assert_eq!(store.t3_discovery_baseline(CLAIM)?, None);
    store.db.execute_batch(
        "DROP TRIGGER lean_spec_comparisons_no_delete; DELETE FROM lean_spec_comparisons;",
    )?;
    assert_eq!(store.t3_discovery_baseline(CLAIM)?, None);
    Ok(())
}

fn apply_plan(
    store: &mut StateStore,
    base: &str,
    reviewed: &str,
    nonce: u32,
) -> Result<T3PlanApproval> {
    let plan = T3PlanApproval {
        plan_sha256: format!("{nonce:064x}"),
        reviewed_commit: reviewed.into(),
        base_commit: base.into(),
        anchor_module: FILE.into(),
        preview_event_seq: store.status()?.last_event_seq,
    };
    let impact = store.apply_t3_plan(&plan, &[FILE])?.unwrap();
    assert_eq!(impact.claims, [CLAIM]);
    Ok(plan)
}

fn restore(
    store: &mut StateStore,
    spec: &ReviewedLeanSpec,
    plan: &T3PlanApproval,
    nonce: u32,
) -> Result<(String, Value)> {
    let mut t3 = compared_receipt(spec, "T3_TAINT_REBUILD", &plan.reviewed_commit, nonce);
    // This is the T3 dependency digest, deliberately different from the T1 prefix.
    t3.raw["dependencies_sha256"] = json!(format!("{:064x}", nonce + 2000));
    let (id, raw) = (t3.id.clone(), t3.raw.clone());
    let mut candidate = Some(t3);
    let report =
        store.recheck_one_stale_for_plan(&plan.plan_sha256, CLAIM, "fixture:t3", |_| {
            Ok(RecheckOutcome::Verified(Box::new(
                candidate.take().unwrap(),
            )))
        })?;
    assert_eq!(report.verified, [CLAIM]);
    Ok((id, raw))
}

#[test]
fn sealed_t3_selects_current_receipt_review_commit_and_original_recipe_without_wiki() -> Result<()>
{
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("state.sqlite");
    let repo = temp.path().to_str().unwrap();
    let mut store = StateStore::open(&path)?;
    let spec = bind_reviewed_proof(&mut store, repo)?;
    let t1 = compared_receipt(&spec, "T1_PROMOTION", BASE, 1);
    promote(&mut store, repo, FIRST, &t1)?;
    let t1_baseline = store.t3_discovery_baseline(CLAIM)?.unwrap();
    let plan = apply_plan(&mut store, FIRST, SECOND, 2)?;
    assert_eq!(store.t3_discovery_baseline(CLAIM)?, None);
    let (id, raw) = restore(&mut store, &spec, &plan, 3)?;
    assert!(store.current_sediment_lineages()?.is_empty());

    let readonly = StateStore::open_readonly(&path)?;
    let seq = readonly.status()?.last_event_seq;
    let expected = T3DiscoveryBaseline {
        receipt_sha256: id,
        dependencies_sha256: raw["dependencies_sha256"].as_str().unwrap().into(),
        verified_at_commit: SECOND.into(),
        baseline_scope: "claim_t3",
        ..t1_baseline
    };
    assert_eq!(readonly.t3_discovery_baseline(CLAIM)?, Some(expected));
    assert_eq!(readonly.status()?.last_event_seq, seq);
    Ok(())
}

#[test]
fn sealed_t3_never_falls_back_to_old_restoration_or_superseded_promotion() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let repo = temp.path().to_str().unwrap();
    let mut store = StateStore::open(":memory:")?;
    let spec = bind_reviewed_proof(&mut store, repo)?;
    let original = compared_receipt(&spec, "T1_PROMOTION", BASE, 1);
    promote(&mut store, repo, FIRST, &original)?;
    let plan = apply_plan(&mut store, FIRST, SECOND, 2)?;
    let (old_id, _) = restore(&mut store, &spec, &plan, 2)?;
    let plan = apply_plan(&mut store, SECOND, THIRD, 3)?;
    store.db.execute_batch("SAVEPOINT pretend_current")?;
    store.db.execute(
        "UPDATE claims SET stale=0,effective_stale=0 WHERE claim_id=?1",
        [CLAIM],
    )?;
    // The later taint event is still authoritative even if the materialized flags lie.
    assert_eq!(read_baseline(&store.db, CLAIM)?, None);
    store
        .db
        .execute_batch("ROLLBACK TO pretend_current; RELEASE pretend_current")?;
    let (current_id, _) = restore(&mut store, &spec, &plan, 3)?;
    let update = "UPDATE claims SET receipt_id=?1 WHERE claim_id=?2";
    assert_eq!(
        store.t3_discovery_baseline(CLAIM)?.unwrap().receipt_sha256,
        current_id
    );
    store.db.execute(update, params![old_id, CLAIM])?;
    assert_eq!(store.t3_discovery_baseline(CLAIM)?, None);
    store.db.execute(update, params![original.id, CLAIM])?;
    assert_eq!(store.t3_discovery_baseline(CLAIM)?, None);
    store.db.execute(update, params![current_id, CLAIM])?;

    let newer = compared_receipt(&spec, "T1_PROMOTION", THIRD, 4);
    promote(&mut store, repo, FOURTH, &newer)?;
    assert_eq!(
        store
            .t3_discovery_baseline(CLAIM)?
            .unwrap()
            .verified_at_commit,
        FOURTH
    );
    store.db.execute(update, params![current_id, CLAIM])?;
    assert_eq!(store.t3_discovery_baseline(CLAIM)?, None);
    store.db.execute(
        "DELETE FROM managed_proof_recipes WHERE git_commit=?1",
        [FOURTH],
    )?;
    assert_eq!(store.t3_discovery_baseline(CLAIM)?, None);
    Ok(())
}

#[test]
fn sealed_t3_rejects_inconsistent_t1_t3_or_applied_evidence() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let repo = temp.path().to_str().unwrap();
    let mut store = StateStore::open(":memory:")?;
    let spec = bind_reviewed_proof(&mut store, repo)?;
    let t1 = compared_receipt(&spec, "T1_PROMOTION", BASE, 1);
    promote(&mut store, repo, FIRST, &t1)?;
    let plan = apply_plan(&mut store, FIRST, SECOND, 2)?;
    let (id, raw_t3) = restore(&mut store, &spec, &plan, 3)?;
    let receipt_update = "UPDATE verification_receipts SET raw_receipt_json=?1 WHERE receipt_id=?2";
    for (field, wrong) in [
        ("engine", json!("rocq")),
        ("tier", json!("T1_PROMOTION")),
        ("base_commit", json!(FIRST)),
        ("file_path", json!("Other.lean")),
        ("target_id", json!("other")),
        ("patch_sha256", json!("1".repeat(64))),
        ("receipt_sha256", json!(t1.id)),
        ("dependencies_sha256", json!("A".repeat(64))),
    ] {
        let mut raw = raw_t3.clone();
        raw[field] = wrong;
        store
            .db
            .execute(receipt_update, params![raw.to_string(), id])?;
        assert_eq!(store.t3_discovery_baseline(CLAIM)?, None, "raw {field}");
    }
    store
        .db
        .execute(receipt_update, params![raw_t3.to_string(), id])?;
    store.db.execute_batch(
        "PRAGMA foreign_keys=OFF;
         DROP TRIGGER lean_spec_rechecks_no_update;
         DROP TRIGGER lean_spec_comparisons_no_update;
         DROP TRIGGER events_no_update;",
    )?;
    for mutation in [
        "UPDATE claims SET effective_stale=1",
        "UPDATE claims SET local_trust_level='L4b'",
        "UPDATE verification_receipts SET receipt_sha256=printf('%064x',29) WHERE tier='T3_TAINT_REBUILD'",
        "UPDATE verification_receipts SET allowlist_passed=0 WHERE tier='T3_TAINT_REBUILD'",
        "UPDATE lean_spec_rechecks SET promoted_commit=reviewed_commit",
        "UPDATE lean_spec_rechecks SET spec_decl='wrong'",
        "UPDATE lean_spec_comparisons SET receipt_sha256=printf('%064x',29)",
        "UPDATE verification_receipts SET raw_receipt_json='{}' WHERE tier='T1_PROMOTION'",
        "UPDATE events SET payload_json='{}' WHERE event_type='PROMOTION_COMMITTED'",
        "UPDATE events SET payload_json=json_set(payload_json,'$.claims',json('[]')) WHERE event_type='T3_TAINT_APPLIED'",
        "UPDATE events SET payload_json=json_set(payload_json,'$.was_stale',json('false')) WHERE event_type='CLAIM_TRUST_CHANGED' AND json_type(payload_json,'$.detail.lean_spec_recheck')='object'",
        "UPDATE events SET payload_json=json_set(payload_json,'$.detail.receipt_id','wrong') WHERE event_type='CLAIM_TRUST_CHANGED' AND json_type(payload_json,'$.detail.lean_spec_recheck')='object'",
        "UPDATE applied_t3_plans SET reviewed_commit=base_commit",
    ] {
        store.db.execute_batch("SAVEPOINT evidence_corruption")?;
        assert_eq!(store.db.execute(mutation, [])?, 1, "{mutation}");
        assert_eq!(read_baseline(&store.db, CLAIM)?, None, "{mutation}");
        store
            .db
            .execute_batch("ROLLBACK TO evidence_corruption; RELEASE evidence_corruption")?;
    }
    store.db.execute_batch("PRAGMA foreign_keys=ON;")?;
    assert_eq!(
        store.t3_discovery_baseline(CLAIM)?.unwrap().receipt_sha256,
        id
    );
    Ok(())
}
