use super::*;
use crate::state::sediment_adaptation::tests::approved_fixture;
use crate::{PromotionIntent, SedimentAdaptationApproval, TaskRunOutcome, VerifiedReceipt};
use std::time::Duration;

const NEW: &str = "dddddddddddddddddddddddddddddddddddddddd";
const LATER: &str = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
const BODY: &str = "by\n  trivial";
const CHANGE: Change<'_> = Change {
    actor: "test",
    git_commit: Some(NEW),
};

fn promotion(
    plan: &SedimentAdaptationPlan,
    commit: &str,
    digest: &str,
) -> (PromotionIntent, VerifiedReceipt) {
    let intent = PromotionIntent {
        repo: plan.repo.clone(),
        claim_id: plan.target_claim.clone(),
        base_commit: plan.reviewed_commit.clone(),
        new_commit: commit.into(),
        file_path: plan.target_module.clone(),
        target_decl: plan.target_decl.clone(),
        proof_body: BODY.into(),
    };
    // Simulate the private projection of sealed broker evidence; native tests
    // exercise the external path using real comparison and promotion receipts.
    let receipt = VerifiedReceipt {
        id: format!("receipt-{digest}"),
        issued_at: "2026-01-01T00:00:00Z".into(),
        tier: "T1_PROMOTION".into(),
        engine: "lean4".into(),
        base_commit: intent.base_commit.clone(),
        patch_sha256: "8".repeat(64),
        target_decl: intent.target_decl.clone(),
        file_path: intent.file_path.clone(),
        allowlist_passed: true,
        axioms_used: vec![],
        external_checker_passed: None,
        compile_seconds: 0.1,
        max_heartbeats: None,
        receipt_sha256: digest.into(),
        raw: json!({"file_path": intent.file_path}),
        regression_checked: true,
        lean_comparison: Some(reviewed_lean::ComparedSpec {
            spec_file: plan.spec_file.clone(),
            spec_decl: plan.spec_decl.clone(),
            spec_prefix_sha256: plan.spec_prefix_sha256.clone(),
            candidate_receipt_sha256: digest.into(),
            receipt_sha256: "9".repeat(64),
        }),
    };
    (intent, receipt)
}

fn complete_candidate(
    store: &mut StateStore,
    approval: &SedimentAdaptationApproval,
    result: Option<Value>,
) -> Result<()> {
    let plan = &approval.plan;
    let seq = last_seq(&store.db)?;
    let lease = store
        .lease_sediment_adaptation(
            &approval.approval_sha256,
            &plan.repo,
            &plan.reviewed_commit,
            seq,
            Duration::from_secs(60),
        )?
        .unwrap();
    if let Some(result) = result {
        store.finish_task_run(
            &lease,
            TaskRunOutcome::Candidate {
                base_commit: &plan.reviewed_commit,
                claim_id: &plan.target_claim,
                role: "prover_worker",
                executable_sha256: &"6".repeat(64),
                result_json: &result.to_string(),
            },
            CHANGE,
        )?;
    }
    Ok(())
}

fn promote_destination(store: &mut StateStore, plan: &SedimentAdaptationPlan) -> Result<()> {
    let (intent, receipt) = promotion(plan, NEW, &"7".repeat(64));
    store.prepare_promotion(&intent, &receipt)?;
    store.finish_promotion(&intent, &receipt)?;
    Ok(())
}

fn fixture(store: &mut StateStore, result: Option<Value>) -> Result<SedimentAdaptationApproval> {
    let approval = approved_fixture(store)?;
    complete_candidate(store, &approval, result)?;
    promote_destination(store, &approval.plan)?;
    Ok(approval)
}

fn valid_result() -> Value {
    json!({"kind": "proof_body", "proof_body": BODY})
}

fn input(a: &SedimentAdaptationApproval, expected_event_seq: i64) -> SedimentLineageInput<'_> {
    SedimentLineageInput {
        approval_sha256: &a.approval_sha256,
        repo: &a.plan.repo,
        promoted_commit: NEW,
        expected_event_seq,
    }
}

fn reject(store: &mut StateStore, input: SedimentLineageInput<'_>, reason: &str) -> Result<()> {
    let before = last_seq(&store.db)?;
    let error = store.retain_sediment_lineage(input).unwrap_err();
    assert!(error.to_string().contains(reason), "{error:#}");
    assert_eq!(store.sediment_lineage(input.approval_sha256)?, None);
    assert_eq!(last_seq(&store.db)?, before);
    Ok(())
}

#[test]
fn lineage_is_atomic_immutable_readback_and_historically_idempotent() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let db = temp.path().join("state.sqlite");
    let mut store = StateStore::open(&db)?;
    let a = fixture(&mut store, Some(valid_result()))?;
    // A stale historical origin remains advisory evidence, unlike refutation.
    store.mark_t3(&[&a.plan.origin_claim], &[], CHANGE)?;
    assert!(store.claim(&a.plan.origin_claim)?.unwrap().stale);
    let old_seq = last_seq(&store.db)?;
    assert!(
        store
            .append_event(CHANGE, RETAINED, "claim", &a.plan.target_claim, json!({}))
            .is_err()
    );
    let seq = StateStore::open(&db)?.append_event(
        CHANGE,
        "TEST_DRIFT",
        "claim",
        &a.plan.target_claim,
        json!({}),
    )?;
    reject(
        &mut store,
        input(&a, old_seq),
        "ledger event sequence changed",
    )?;
    store.db.execute_batch(
        "CREATE TRIGGER fail_lineage BEFORE INSERT ON sediment_adaptation_lineage
         BEGIN SELECT RAISE(ABORT, 'fixture insert failure'); END;",
    )?;
    reject(&mut store, input(&a, seq), "fixture insert failure")?;
    store.db.execute_batch("DROP TRIGGER fail_lineage")?;

    let task = StateStore::sediment_adaptation_task_id(&a.approval_sha256)?;
    let candidate = store.task_candidate(&task)?;
    let claim = serde_json::to_value(store.claim(&a.plan.target_claim)?)?;
    let recipe = store.managed_proof_recipe(&a.plan.target_claim)?;
    let record = store.retain_sediment_lineage(input(&a, seq))?;
    assert_eq!(record.approval_sha256, a.approval_sha256);
    assert_eq!(record.archive_id, a.plan.archive_id);
    assert_eq!(record.target_claim, a.plan.target_claim);
    assert_eq!(record.promoted_commit, NEW);
    assert_eq!(record.task_id, task);
    assert_eq!(
        record.core_entry_id,
        format!("CORE-ADAPT-{}", a.approval_sha256)
    );
    assert!(record.core_entry_prospective);
    assert_eq!(record.origin_t1_receipt_sha256, "4".repeat(64));
    assert_eq!(record.origin_comparison_receipt_sha256, "3".repeat(64));
    assert_eq!(record.candidate_receipt_sha256, "7".repeat(64));
    assert_eq!(record.comparison_receipt_sha256, "9".repeat(64));
    assert_eq!(record.expected_event_seq, seq);
    let events = store.events_after(seq, 10)?;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event_type, RETAINED);
    assert_eq!(events[0].entity_id, a.plan.target_claim);
    assert_eq!(events[0].git_commit.as_deref(), Some(NEW));
    assert_eq!(events[0].event_seq, record.event_seq);
    assert_eq!(events[0].payload, record.event_payload());
    assert_eq!(store.task_candidate(&task)?, candidate);
    assert_eq!(
        serde_json::to_value(store.claim(&a.plan.target_claim)?)?,
        claim
    );
    assert_eq!(store.managed_proof_recipe(&a.plan.target_claim)?, recipe);
    for sql in [
        "UPDATE sediment_adaptation_lineage SET promoted_commit='unrelated'",
        "DELETE FROM sediment_adaptation_lineage",
        "INSERT OR REPLACE INTO sediment_adaptation_lineage SELECT * FROM sediment_adaptation_lineage",
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

    store.mark_t3(&[&a.plan.target_claim], &[], CHANGE)?;
    let drift = last_seq(&store.db)?;
    assert!(drift > record.event_seq);
    assert_eq!(store.retain_sediment_lineage(input(&a, seq))?, record);
    assert_eq!(store.retain_sediment_lineage(input(&a, drift))?, record);
    assert!(
        store
            .retain_sediment_lineage(SedimentLineageInput {
                promoted_commit: LATER,
                ..input(&a, drift)
            })
            .unwrap_err()
            .to_string()
            .contains("conflicting")
    );
    assert_eq!(last_seq(&store.db)?, drift);
    let readonly = StateStore::open_readonly(&db)?;
    assert_eq!(readonly.sediment_lineage(&a.approval_sha256)?, Some(record));
    Ok(())
}

#[test]
fn managed_wiki_stamp_refuses_pending_journals_without_advancing_sequence() -> Result<()> {
    let mut store = StateStore::open(":memory:")?;
    assert!(store.managed_wiki_stamp()?.is_none());
    let approval = fixture(&mut store, Some(valid_result()))?;
    let seq = last_seq(&store.db)?;
    let expected = Some((approval.plan.repo.clone(), seq));
    assert_eq!(store.managed_wiki_stamp()?, expected);
    store.db.execute(
        "INSERT INTO pivot_resume_intent VALUES (1,'fixture','{}',?1)",
        [NEW],
    )?;
    assert!(store.managed_wiki_stamp()?.is_none());
    assert_eq!(last_seq(&store.db)?, seq);
    store.db.execute("DELETE FROM pivot_resume_intent", [])?;
    let plan = &approval.plan;
    store.db.execute(
        "INSERT INTO promotion_intent VALUES (1,?1,?2,?3,?4,?5,?6,?7)",
        params![
            plan.repo,
            plan.target_claim,
            plan.reviewed_commit,
            NEW,
            plan.target_module,
            plan.target_decl,
            BODY
        ],
    )?;
    assert!(store.managed_wiki_stamp()?.is_none());
    assert_eq!(last_seq(&store.db)?, seq);
    store.db.execute("DELETE FROM promotion_intent", [])?;
    assert_eq!(store.managed_wiki_stamp()?, expected);
    Ok(())
}

#[test]
fn core_selector_requires_current_exact_t1_and_preserves_historical_lineage() -> Result<()> {
    let mut store = StateStore::open(":memory:")?;
    let approval = fixture(&mut store, Some(valid_result()))?;
    assert!(store.current_sediment_lineages()?.is_empty());
    let seq = last_seq(&store.db)?;
    let record = store.retain_sediment_lineage(input(&approval, seq))?;
    assert_eq!(store.current_sediment_lineages()?, vec![record.clone()]);
    let receipt = store
        .claim(&approval.plan.target_claim)?
        .unwrap()
        .receipt_id
        .unwrap();
    store.db.execute(
        "UPDATE claims SET receipt_id=NULL WHERE claim_id=?1",
        [&approval.plan.target_claim],
    )?;
    assert!(store.current_sediment_lineages()?.is_empty());
    store.db.execute(
        "UPDATE claims SET receipt_id=?2 WHERE claim_id=?1",
        params![approval.plan.target_claim, receipt],
    )?;
    store.mark_t3(&[&approval.plan.target_claim], &[], CHANGE)?;
    assert!(store.current_sediment_lineages()?.is_empty());
    assert_eq!(
        store.sediment_lineage(&approval.approval_sha256)?,
        Some(record)
    );
    Ok(())
}

#[test]
fn lineage_requires_completed_exact_scoped_native_result_and_body() -> Result<()> {
    for (candidate, message) in [
        (None, "completed native candidate"),
        (
            Some(json!({"kind":"proof_body", "proof_body": format!("{BODY} ")})),
            "does not exactly match",
        ),
        (
            Some(json!({"kind":"findings", "proof_body": BODY})),
            "not a supported proof-body",
        ),
        (
            Some(json!({"kind":"proof_body", "proof_body": BODY, "unknown":1})),
            "not a supported proof-body",
        ),
    ] {
        let mut store = StateStore::open(":memory:")?;
        let a = fixture(&mut store, candidate)?;
        let seq = last_seq(&store.db)?;
        reject(&mut store, input(&a, seq), message)?;
    }
    for mutation in [
        "UPDATE scheduler_tasks SET completed_at_ms=NULL",
        "UPDATE scheduler_tasks SET module_path='Wrong.lean'",
        "UPDATE scheduler_tasks SET target='unrelated'",
        "UPDATE scheduler_tasks SET track='exploration'",
        "UPDATE scheduler_tasks SET engine='agda'",
        "UPDATE scheduler_task_candidates SET base_commit='eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee'",
        "UPDATE scheduler_task_candidates SET claim_id='claim:origin'",
        "UPDATE scheduler_task_candidates SET role='independent_reviewer'",
        "INSERT INTO scheduler_model_task_candidates (task_seq,base_commit,claim_id,role,result_json)
          SELECT task_seq,base_commit,claim_id,role,result_json FROM scheduler_task_candidates;
         DELETE FROM scheduler_task_candidates",
        "DROP TRIGGER scheduler_sediment_adaptations_no_delete; DELETE FROM scheduler_sediment_adaptations",
    ] {
        let mut store = StateStore::open(":memory:")?;
        let a = fixture(&mut store, Some(valid_result()))?;
        store.db.execute_batch(mutation)?;
        let seq = last_seq(&store.db)?;
        reject(&mut store, input(&a, seq), "completed native candidate")?;
    }
    let mut store = StateStore::open(":memory:")?;
    let a = fixture(&mut store, Some(valid_result()))?;
    store.db.execute_batch(
        "DROP TRIGGER events_no_delete;
        DELETE FROM events WHERE event_type='TASK_COMPLETED'",
    )?;
    let id = StateStore::sediment_adaptation_task_id(&a.approval_sha256)?;
    let late_seq = store.append_event(
        CHANGE,
        "TASK_COMPLETED",
        "task",
        &id,
        json!({"untrusted_candidate": true}),
    )?;
    reject(
        &mut store,
        input(&a, late_seq),
        "completed native candidate",
    )?;
    Ok(())
}

#[test]
fn only_one_approval_can_retain_lineage_for_a_promotion() -> Result<()> {
    let mut store = StateStore::open(":memory:")?;
    let first = approved_fixture(&mut store)?;
    let second_plan = SedimentAdaptationPlan {
        expected_event_seq: last_seq(&store.db)?,
        ..first.plan.clone()
    };
    let second =
        store.approve_sediment_adaptation(&second_plan, &second_plan.approval_sha256()?)?;
    assert_ne!(first.approval_sha256, second.approval_sha256);
    complete_candidate(&mut store, &first, Some(valid_result()))?;
    complete_candidate(&mut store, &second, Some(valid_result()))?;
    promote_destination(&mut store, &first.plan)?;
    let seq = last_seq(&store.db)?;
    let retained = store.retain_sediment_lineage(input(&first, seq))?;
    reject(
        &mut store,
        input(&second, retained.event_seq),
        "already has retained",
    )?;

    let alternate_event = store.append_event(
        CHANGE,
        "TEST_DRIFT",
        "claim",
        &first.plan.target_claim,
        json!({}),
    )?;
    let replace = store
        .db
        .execute(
            "INSERT OR REPLACE INTO sediment_adaptation_lineage
         SELECT ?1, (SELECT task_seq FROM scheduler_sediment_adaptations WHERE approval_sha256=?1),
            promoted_commit,origin_t1_receipt_sha256,origin_comparison_receipt_sha256,
            candidate_receipt_sha256,comparison_receipt_sha256,expected_event_seq,?2
         FROM sediment_adaptation_lineage WHERE approval_sha256=?3",
            params![
                second.approval_sha256,
                alternate_event,
                first.approval_sha256
            ],
        )
        .unwrap_err();
    assert!(replace.to_string().contains("immutable"), "{replace}");
    assert_eq!(
        store.sediment_lineage(&first.approval_sha256)?,
        Some(retained)
    );
    assert_eq!(store.sediment_lineage(&second.approval_sha256)?, None);
    Ok(())
}

#[test]
fn lineage_requires_exact_paired_receipts_and_current_latest_l5() -> Result<()> {
    let mut store = StateStore::open(":memory:")?;
    let a = fixture(&mut store, Some(valid_result()))?;
    let seq = last_seq(&store.db)?;
    let wrong_repo = Path::new(&a.plan.repo).join("different-repo");
    reject(
        &mut store,
        SedimentLineageInput {
            repo: wrong_repo.to_str().unwrap(),
            ..input(&a, seq)
        },
        "not the bound managed repository",
    )?;
    reject(
        &mut store,
        SedimentLineageInput {
            promoted_commit: LATER,
            ..input(&a, seq)
        },
        "latest destination promotion",
    )?;
    let (intent, receipt) = promotion(&a.plan, LATER, &"a".repeat(64));
    store.prepare_promotion(&intent, &receipt)?;
    reject(&mut store, input(&a, seq), "pending promotion or Pivot")?;
    store.finish_promotion(&intent, &receipt)?;
    let seq = last_seq(&store.db)?;
    reject(&mut store, input(&a, seq), "latest destination promotion")?;

    for (sql, reason) in [
        (
            "UPDATE claims SET stale=1 WHERE claim_id=?1",
            "non-stale local and effective L5",
        ),
        (
            "UPDATE claims SET effective_stale=1 WHERE claim_id=?1",
            "non-stale local and effective L5",
        ),
        (
            "UPDATE claims SET local_trust_level='L4b' WHERE claim_id=?1",
            "non-stale local and effective L5",
        ),
        (
            "UPDATE claims SET effective_trust_level='L4b' WHERE claim_id=?1",
            "non-stale local and effective L5",
        ),
        (
            "UPDATE claims SET receipt_id='replaced' WHERE claim_id=?1",
            "not the current Claim receipt",
        ),
    ] {
        let mut store = StateStore::open(":memory:")?;
        let a = fixture(&mut store, Some(valid_result()))?;
        store.db.execute(sql, [&a.plan.target_claim])?;
        let seq = last_seq(&store.db)?;
        reject(&mut store, input(&a, seq), reason)?;
    }
    let mut store = StateStore::open(":memory:")?;
    let a = fixture(&mut store, Some(valid_result()))?;
    store.db.execute(
        "UPDATE claims SET effective_trust_level='L2b' WHERE claim_id=?1",
        [&a.plan.origin_claim],
    )?;
    let seq = last_seq(&store.db)?;
    reject(&mut store, input(&a, seq), "origin Claim is refuted")?;

    for origin in [true, false] {
        let mut store = StateStore::open(":memory:")?;
        let a = fixture(&mut store, Some(valid_result()))?;
        let seq = last_seq(&store.db)?;
        let commit = if origin { &a.plan.promoted_commit } else { NEW };
        store.db.execute(
            "UPDATE verification_receipts SET tier='T3_TAINT_REBUILD'
             WHERE receipt_id=(SELECT receipt_id FROM recorded_promotions WHERE git_commit=?1)",
            [commit],
        )?;
        reject(&mut store, input(&a, seq), "exact recorded T1 receipt")?;
        store
            .db
            .execute_batch("DROP TRIGGER lean_spec_comparisons_no_update")?;
        store.db.execute(
            "UPDATE lean_spec_comparisons SET spec_decl='changed' WHERE git_commit=?1",
            [commit],
        )?;
        reject(&mut store, input(&a, seq), "approved spec identity")?;

        let mut store = StateStore::open(":memory:")?;
        let a = fixture(&mut store, Some(valid_result()))?;
        let claim = if origin {
            &a.plan.origin_claim
        } else {
            &a.plan.target_claim
        };
        store
            .db
            .execute_batch("DROP TRIGGER reviewed_lean_specs_no_update")?;
        store.db.execute(
            "UPDATE reviewed_lean_specs SET spec_decl='changed' WHERE claim_id=?1",
            [claim],
        )?;
        let seq = last_seq(&store.db)?;
        reject(
            &mut store,
            input(&a, seq),
            "reviewed Lean spec binding does not match",
        )?;
    }
    Ok(())
}

#[test]
fn legacy_readonly_returns_none_without_migrating() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let db = temp.path().join("legacy.sqlite");
    Connection::open(&db)?;
    let store = StateStore::open_readonly(&db)?;
    assert_eq!(store.sediment_lineage(&"5".repeat(64))?, None);
    assert!(store.sediment_lineage("bad-approval").is_err());
    assert!(!store.db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name=?1)",
        [TABLE],
        |r| r.get::<_, bool>(0),
    )?);
    Ok(())
}
