use super::*;
use crate::{Change, NewClaim, PromotionIntent, ReviewedLeanSpec, VerifiedReceipt};
use ai4math_broker::lean_breaker_sources_sha256;
use rusqlite::params;
use serde_json::json;

const CLAIM: &str = "claim:baseline";
const FILE: &str = "Proof.lean";
const DECL: &str = "goal";
const BASE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const FIRST: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const SECOND: &str = "cccccccccccccccccccccccccccccccccccccccc";

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

#[test]
fn reviewed_binding_requires_original_exact_sealed_comparison() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let repo = temp.path().to_str().unwrap();
    let mut store = StateStore::open(":memory:")?;
    register(&mut store)?;
    let spec = ReviewedLeanSpec {
        repo: repo.into(),
        claim_id: CLAIM.into(),
        approved_git_commit: BASE.into(),
        spec_file: "Spec.lean".into(),
        spec_decl: "reviewedGoal".into(),
        spec_prefix_sha256: "a".repeat(64),
    };
    store.bind_lean_spec(&spec)?;
    let mut t1 = receipt("T1_PROMOTION", BASE, 1);
    t1.lean_comparison = Some(reviewed_lean::ComparedSpec {
        spec_file: spec.spec_file,
        spec_decl: spec.spec_decl,
        spec_prefix_sha256: spec.spec_prefix_sha256,
        candidate_receipt_sha256: t1.receipt_sha256.clone(),
        receipt_sha256: "e".repeat(64),
    });
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
