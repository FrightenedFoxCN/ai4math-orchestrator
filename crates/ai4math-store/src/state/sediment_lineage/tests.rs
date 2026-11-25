use super::*;
use crate::state::sediment_adaptation::tests::approved_fixture;
use crate::{
    PromotionIntent, RecheckOutcome, SedimentAdaptationApproval, T3PlanApproval, TaskRunOutcome,
    VerifiedReceipt,
};
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
        broker_assessed_t3: false,
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
    assert_core_evidence(&store, &record, NEW)?;
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

fn assert_core_evidence(store: &StateStore, record: &SedimentLineage, commit: &str) -> Result<()> {
    assert_eq!(
        store.current_sediment_lineages()?,
        [SedimentCoreEvidence {
            lineage: record.clone(),
            verified_at_commit: commit.into(),
        }]
    );
    Ok(())
}

fn apply_core_t3_plan(
    store: &mut StateStore,
    adaptation: &SedimentAdaptationPlan,
    base: &str,
    reviewed: &str,
    n: usize,
) -> Result<T3PlanApproval> {
    let approval = T3PlanApproval {
        plan_sha256: format!("{n:064x}"),
        base_commit: base.into(),
        reviewed_commit: reviewed.into(),
        anchor_module: adaptation.target_module.clone(),
        preview_event_seq: last_seq(&store.db)?,
    };
    let impact = store
        .apply_t3_plan(&approval, &[&adaptation.target_module])?
        .context("fixture T3 plan was already applied")?;
    assert_eq!(impact.claims, [adaptation.target_claim.as_str()]);
    Ok(approval)
}

fn restore_core_t3(
    store: &mut StateStore,
    adaptation: &SedimentAdaptationPlan,
    plan: &T3PlanApproval,
    n: usize,
) -> Result<String> {
    let (_, mut receipt) = promotion(adaptation, NEW, &format!("{n:064x}"));
    receipt.base_commit = plan.reviewed_commit.clone();
    receipt.tier = "T3_TAINT_REBUILD".into();
    receipt.broker_assessed_t3 = true;
    receipt.regression_checked = false;
    receipt.lean_comparison.as_mut().unwrap().receipt_sha256 = format!("{:064x}", n + 1000);
    let id = receipt.id.clone();
    let mut receipt = Some(receipt);
    let report = store.recheck_one_stale_for_plan(
        &plan.plan_sha256,
        &adaptation.target_claim,
        "fixture:recheck",
        |_| Ok(RecheckOutcome::Verified(Box::new(receipt.take().unwrap()))),
    )?;
    assert_eq!(report.verified, [adaptation.target_claim.as_str()]);
    Ok(id)
}

fn retained_core_fixture(
    store: &mut StateStore,
) -> Result<(SedimentAdaptationApproval, SedimentLineage)> {
    let adaptation = fixture(store, Some(valid_result()))?;
    let record = store.retain_sediment_lineage(input(&adaptation, last_seq(&store.db)?))?;
    Ok((adaptation, record))
}

#[test]
fn legacy_t3_head_uniqueness_migrates_without_touching_receipts_events_or_checkpoints() -> Result<()>
{
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("legacy.sqlite");
    let mut legacy = StateStore::open(&path)?;
    let (adaptation, record) = retained_core_fixture(&mut legacy)?;
    let first = apply_core_t3_plan(&mut legacy, &adaptation.plan, NEW, LATER, 1)?;
    restore_core_t3(&mut legacy, &adaptation.plan, &first, 1)?;
    let step = legacy.t3_sweep_step(&first.plan_sha256, 1, false, "fixture:migration", |_| {
        anyhow::bail!("already restored Claim")
    })?;
    let checkpoint = step.progress;
    let receipt = legacy
        .lean_spec_recheck(&adaptation.plan.target_claim, &first.plan_sha256)?
        .unwrap();
    let seq = last_seq(&legacy.db)?;
    legacy.db.execute_batch(
        "CREATE UNIQUE INDEX legacy_t3_one_head ON applied_t3_plans(reviewed_commit);
                              DELETE FROM state_migrations WHERE version=2;",
    )?;
    drop(legacy);

    let readonly = StateStore::open_readonly(&path)?;
    assert_eq!(readonly.t3_sweep_status(&first.plan_sha256)?, checkpoint);
    assert_core_evidence(&readonly, &record, LATER)?;
    assert_eq!(last_seq(&readonly.db)?, seq);
    assert!(super::super::migrations::pending(&readonly.db)?);
    drop(readonly);

    let mut migrated = StateStore::open(&path)?;
    assert!(!super::super::migrations::pending(&migrated.db)?);
    assert_eq!(last_seq(&migrated.db)?, seq);
    assert_eq!(migrated.t3_sweep_status(&first.plan_sha256)?, checkpoint);
    assert_eq!(
        migrated.applied_t3_plan(&first.plan_sha256)?,
        Some(first.clone())
    );
    assert_eq!(
        migrated
            .lean_spec_recheck(&adaptation.plan.target_claim, &first.plan_sha256)?
            .as_ref(),
        Some(&receipt)
    );
    assert_core_evidence(&migrated, &record, LATER)?;
    assert!(
        migrated
            .db
            .query_row("PRAGMA foreign_keys", [], |r| r.get::<_, bool>(0))?
    );

    let second = apply_core_t3_plan(&mut migrated, &adaptation.plan, NEW, LATER, 2)?;
    assert!(migrated.current_sediment_lineages()?.is_empty());
    restore_core_t3(&mut migrated, &adaptation.plan, &second, 2)?;
    assert_core_evidence(&migrated, &record, LATER)?;
    assert_eq!(migrated.t3_sweep_status(&first.plan_sha256)?, checkpoint);
    assert_eq!(
        migrated
            .lean_spec_recheck(&adaptation.plan.target_claim, &first.plan_sha256)?
            .as_ref(),
        Some(&receipt)
    );
    Ok(())
}

#[test]
fn core_selector_tracks_exact_current_recheck_across_repeated_taint() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let db = temp.path().join("state.sqlite");
    let mut store = StateStore::open(&db)?;
    let (adaptation, record) = retained_core_fixture(&mut store)?;
    assert_core_evidence(&store, &record, NEW)?;

    let first = apply_core_t3_plan(&mut store, &adaptation.plan, NEW, LATER, 1)?;
    assert!(store.current_sediment_lineages()?.is_empty());
    let old_receipt = restore_core_t3(&mut store, &adaptation.plan, &first, 1)?;
    assert_core_evidence(&StateStore::open_readonly(&db)?, &record, LATER)?;

    let newest = "ffffffffffffffffffffffffffffffffffffffff";
    let second = apply_core_t3_plan(&mut store, &adaptation.plan, LATER, newest, 2)?;
    assert!(store.current_sediment_lineages()?.is_empty());
    let current_receipt = restore_core_t3(&mut store, &adaptation.plan, &second, 2)?;
    let seq = last_seq(&store.db)?;
    assert_core_evidence(&store, &record, newest)?;
    assert_eq!(last_seq(&store.db)?, seq);

    // Merely reattaching an older, otherwise exact sealed receipt cannot
    // select the earlier review snapshot after a later trust restoration.
    store.db.execute(
        "UPDATE claims SET receipt_id=?2 WHERE claim_id=?1",
        params![adaptation.plan.target_claim, old_receipt],
    )?;
    assert!(store.current_sediment_lineages()?.is_empty());
    store.db.execute(
        "UPDATE claims SET receipt_id=?2 WHERE claim_id=?1",
        params![adaptation.plan.target_claim, current_receipt],
    )?;
    assert_core_evidence(&store, &record, newest)?;
    assert_eq!(
        store.sediment_lineage(&adaptation.approval_sha256)?,
        Some(record)
    );
    Ok(())
}

#[test]
fn core_selector_rejects_generic_t3_and_recheck_of_a_newer_promotion() -> Result<()> {
    let mut store = StateStore::open(":memory:")?;
    let (adaptation, record) = retained_core_fixture(&mut store)?;
    let newest = "ffffffffffffffffffffffffffffffffffffffff";
    // A different T1 promotion of this Claim supersedes the lineage anchor.
    let (intent, receipt) = promotion(&adaptation.plan, LATER, &"a".repeat(64));
    store.prepare_promotion(&intent, &receipt)?;
    store.finish_promotion(&intent, &receipt)?;
    assert!(store.current_sediment_lineages()?.is_empty());
    let t3 = apply_core_t3_plan(&mut store, &adaptation.plan, LATER, newest, 1)?;
    restore_core_t3(&mut store, &adaptation.plan, &t3, 1)?;
    assert!(store.current_sediment_lineages()?.is_empty());
    assert_eq!(
        store.sediment_lineage(&adaptation.approval_sha256)?,
        Some(record)
    );

    // Even an otherwise identical current T3 receipt is not eligible without
    // its own sealed comparison and exact recorded trust restoration.
    let mut store = StateStore::open(":memory:")?;
    let (adaptation, record) = retained_core_fixture(&mut store)?;
    let t3 = apply_core_t3_plan(&mut store, &adaptation.plan, NEW, LATER, 1)?;
    let verified = restore_core_t3(&mut store, &adaptation.plan, &t3, 1)?;
    assert_core_evidence(&store, &record, LATER)?;
    let generic_sha = "b".repeat(64);
    store.db.execute(
        "INSERT INTO verification_receipts
         SELECT 'generic',issued_at,tier,engine,base_commit,patch_sha256,target_decl,allowlist_passed,
                axioms_used_json,external_checker_passed,compile_seconds,max_heartbeats,?2,raw_receipt_json
         FROM verification_receipts WHERE receipt_id=?1",
        params![verified, generic_sha],
    )?;
    store.db.execute(
        "UPDATE claims SET receipt_id='generic' WHERE claim_id=?1",
        [&adaptation.plan.target_claim],
    )?;
    assert!(store.current_sediment_lineages()?.is_empty());
    store
        .db
        .execute_batch("DROP TRIGGER lean_spec_rechecks_no_update")?;
    store.db.execute(
        "UPDATE lean_spec_rechecks SET candidate_receipt_sha256=?1",
        [&generic_sha],
    )?;
    assert!(store.current_sediment_lineages()?.is_empty());
    Ok(())
}

#[test]
fn core_selector_checks_t3_ledger_bindings_and_current_l5_state() -> Result<()> {
    for mutation in [
        "UPDATE claims SET stale=1 WHERE receipt_id=?1",
        "UPDATE claims SET effective_stale=1 WHERE receipt_id=?1",
        "UPDATE claims SET local_trust_level='L4b' WHERE receipt_id=?1",
        "UPDATE claims SET effective_trust_level='L4b' WHERE receipt_id=?1",
        "UPDATE verification_receipts SET tier='T2_AUDIT' WHERE receipt_id=?1",
        "UPDATE verification_receipts SET engine='agda' WHERE receipt_id=?1",
        "UPDATE verification_receipts SET base_commit='different' WHERE receipt_id=?1",
        "UPDATE verification_receipts SET target_decl='different' WHERE receipt_id=?1",
        "UPDATE verification_receipts SET patch_sha256='different' WHERE receipt_id=?1",
        r#"UPDATE verification_receipts SET raw_receipt_json='{"file_path":"Other.lean"}' WHERE receipt_id=?1"#,
        "UPDATE verification_receipts SET patch_sha256='different' WHERE tier='T1_PROMOTION' AND receipt_id<>?1",
        "UPDATE applied_t3_plans SET reviewed_commit='different' WHERE ?1 IS NOT NULL",
        "UPDATE lean_spec_rechecks SET promoted_commit=(SELECT promoted_commit FROM sediment_adaptation_approvals) WHERE ?1 IS NOT NULL",
        "UPDATE lean_spec_rechecks SET spec_file='Other.lean' WHERE ?1 IS NOT NULL",
        "UPDATE lean_spec_rechecks SET spec_decl='different' WHERE ?1 IS NOT NULL",
        "UPDATE lean_spec_rechecks SET spec_prefix_sha256=printf('%064x',42) WHERE ?1 IS NOT NULL",
        "UPDATE reviewed_lean_specs SET spec_decl='different' WHERE ?1 IS NOT NULL",
        "UPDATE lean_spec_comparisons SET receipt_sha256=printf('%064x',42) WHERE ?1 IS NOT NULL",
        "UPDATE events SET payload_json=json_set(payload_json,'$.claims',json('[]'))
         WHERE event_type='T3_TAINT_APPLIED' AND ?1 IS NOT NULL",
        "UPDATE events SET payload_json=json_set(payload_json,'$.detail.receipt_id','generic')
         WHERE event_type='CLAIM_TRUST_CHANGED' AND json_extract(payload_json,'$.detail.receipt_id')=?1",
        "UPDATE events SET payload_json=json_set(payload_json,'$.detail.lean_spec_recheck.comparison_receipt_sha256','different')
         WHERE event_type='CLAIM_TRUST_CHANGED' AND json_extract(payload_json,'$.detail.receipt_id')=?1",
    ] {
        let mut store = StateStore::open(":memory:")?;
        let (adaptation, record) = retained_core_fixture(&mut store)?;
        let t3 = apply_core_t3_plan(&mut store, &adaptation.plan, NEW, LATER, 1)?;
        let receipt = restore_core_t3(&mut store, &adaptation.plan, &t3, 1)?;
        assert_core_evidence(&store, &record, LATER)?;
        store.db.execute_batch(
            "DROP TRIGGER lean_spec_rechecks_no_update;
             DROP TRIGGER reviewed_lean_specs_no_update;
             DROP TRIGGER lean_spec_comparisons_no_update;
             DROP TRIGGER events_no_update;",
        )?;
        assert!(store.db.execute(mutation, [&receipt])? > 0, "{mutation}");
        assert!(store.current_sediment_lineages()?.is_empty(), "{mutation}");
    }
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
