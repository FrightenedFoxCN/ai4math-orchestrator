use super::*;
use anyhow::Result;
use serde_json::json;

const BASE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const T3_OLD: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const T3_HEAD: &str = "cccccccccccccccccccccccccccccccccccccccc";

fn change(commit: &str) -> Change<'_> {
    Change {
        actor: "test",
        git_commit: Some(commit),
    }
}

fn claim(store: &mut StateStore, id: &str, module: Option<&str>, critical: bool) -> Result<()> {
    store.create_claim(
        NewClaim {
            id,
            title: id,
            informal_statement: "fixture",
            engine: "lean4",
            formal_target: Some(id),
            module_path: module.or(Some("Spec.lean")),
            critical,
        },
        change(BASE),
    )
}

fn plan(store: &mut StateStore, id: &str, target: &str, deps: &[&str]) -> Result<()> {
    let deps: Vec<_> = deps.iter().map(|&id| (id, "lemma")).collect();
    store.create_plan(
        NewPlan {
            id,
            name: id,
            target,
            status: PlanStatus::Active,
            track: "exploration",
            dependencies: &deps,
        },
        change(BASE),
    )
}

fn receipt(target: &str, base: &str, nonce: u32) -> VerifiedReceipt {
    VerifiedReceipt {
        id: format!("receipt-{nonce}"),
        issued_at: "2026-01-01T00:00:00Z".into(),
        tier: if base == BASE {
            "T1_PROMOTION"
        } else {
            "T3_TAINT_REBUILD"
        }
        .into(),
        engine: "lean4".into(),
        base_commit: base.into(),
        patch_sha256: "f".repeat(64),
        target_decl: target.into(),
        file_path: match target {
            "a-top" => "Remote.lean",
            "b-leaf" => "Uses.lean",
            "z-def" => "Defs.lean",
            _ => "Spec.lean",
        }
        .into(),
        allowlist_passed: true,
        axioms_used: vec![],
        external_checker_passed: None,
        compile_seconds: 0.03,
        max_heartbeats: None,
        receipt_sha256: format!("{nonce:064x}"),
        raw: json!({"test": true}),
        regression_checked: true,
        lean_comparison: None,
    }
}

#[test]
fn and_or_refutation_preserves_alternatives_until_exhaustion() -> Result<()> {
    use Trust::*;
    let mut store = StateStore::open(":memory:")?;
    for id in ["parent", "top-lemma", "spectral", "combinatorial"] {
        claim(&mut store, id, None, id == "parent")?;
    }
    for (id, nonce) in [("parent", 1), ("top-lemma", 2)] {
        store.record_verified(id, &receipt(id, BASE, nonce), change(BASE))?;
    }
    store.set_local_trust("spectral", L4a, change(BASE))?;
    store.set_local_trust("combinatorial", L3, change(BASE))?;
    plan(
        &mut store,
        "spectral-plan",
        "parent",
        &["top-lemma", "spectral"],
    )?;
    plan(&mut store, "combo-plan", "parent", &["combinatorial"])?;
    assert_eq!(store.plan_state("spectral-plan")?.unwrap().1, L4a);
    assert_eq!(store.plan_state("combo-plan")?.unwrap().1, L3);
    let parent = store.claim("parent")?.unwrap();
    assert_eq!((parent.local_trust, parent.effective_trust), (L5, L4a));

    store.hard_refute("spectral", "GRV-1", change(BASE))?;
    assert_eq!(
        store.plan_state("spectral-plan")?,
        Some(("INVALIDATED_BY_REFUTATION".into(), L0))
    );
    assert_eq!(store.plan_state("combo-plan")?, Some(("ACTIVE".into(), L3)));
    assert_eq!(store.claim("parent")?.unwrap().effective_trust, L3);
    assert_eq!(store.status()?.pending_decisions, 0);
    let refuted = store.claim("spectral")?.unwrap();
    assert_eq!(
        (refuted.local_trust, refuted.wiki_id.as_deref()),
        (L2b, Some("GRV-1"))
    );

    store.hard_refute("combinatorial", "GRV-2", change(BASE))?;
    let parent = store.claim("parent")?.unwrap();
    assert_eq!((parent.local_trust, parent.effective_trust), (L5, L0));
    assert_eq!(store.status()?.pending_decisions, 1);
    let events = store.events_after(0, 100)?;
    let alerts: Vec<_> = events
        .iter()
        .filter(|event| event.event_type == "CRITICAL_CLAIM_REFUTED")
        .collect();
    assert_eq!(alerts.len(), 1);
    assert_eq!(alerts[0].entity_id, "parent");
    assert_eq!(alerts[0].payload["claim_itself_refuted"], false);
    Ok(())
}

#[test]
fn t3_marks_both_graphs_and_rechecks_only_previous_l5_in_order() -> Result<()> {
    use Trust::*;
    let mut store = StateStore::open(":memory:")?;
    for (id, module) in [
        ("a-top", "Remote.lean"),
        ("b-leaf", "Uses.lean"),
        ("z-def", "Defs.lean"),
        ("y-oracle", "Uses.lean"),
        ("unrelated", "Outside.lean"),
    ] {
        claim(&mut store, id, Some(module), false)?;
    }
    store.add_module_dependency("Uses.lean", "Mid.lean", "lean4", "leaf_proof", change(BASE))?;
    store.add_module_dependency("Mid.lean", "Defs.lean", "lean4", "defs_spec", change(BASE))?;
    store.set_local_trust("unrelated", L4a, change(BASE))?;
    store.set_local_trust("y-oracle", L4a, change(BASE))?;
    store.set_local_trust("z-def", L3, change(BASE))?;
    for (id, nonce) in [("a-top", 1), ("b-leaf", 2)] {
        store.record_verified(id, &receipt(id, BASE, nonce), change(BASE))?;
    }
    plan(&mut store, "leaf", "b-leaf", &["z-def"])?;
    plan(&mut store, "main", "a-top", &["b-leaf"])?;
    plan(&mut store, "backup", "a-top", &["unrelated"])?;
    assert_eq!(store.claim("a-top")?.unwrap().effective_trust, L4a);
    // A proof-only evidence update bubbles up through both levels with no taint.
    store.record_verified("z-def", &receipt("z-def", BASE, 3), change(BASE))?;
    assert_eq!(store.claim("a-top")?.unwrap().effective_trust, L5);
    assert_eq!(store.status()?.stale_claims, 0);

    let preview = store.preview_t3(&["z-def"], &[])?;
    assert_eq!(preview.event_seq, store.status()?.last_event_seq);
    assert!(!store.claim("z-def")?.unwrap().stale);
    let impact = store.mark_t3(&["z-def"], &[], change(T3_OLD))?;
    assert_eq!(preview.impact, impact);
    assert_eq!(impact.modules, ["Defs.lean", "Mid.lean", "Uses.lean"]);
    // y-oracle is module-only, while a-top is logical-only.
    assert_eq!(impact.claims, ["y-oracle", "z-def", "b-leaf", "a-top"]);
    assert!(!store.claim("unrelated")?.unwrap().stale);
    assert_eq!(store.status()?.stale_claims, 4);
    for (id, previous) in [
        ("a-top", L5),
        ("b-leaf", L5),
        ("z-def", L5),
        ("y-oracle", L4a),
    ] {
        let state = store.claim(id)?.unwrap();
        assert!(state.stale && state.effective_stale);
        assert_eq!(state.prior_trust, Some(previous));
        assert_eq!(state.taint_root_commit.as_deref(), Some(T3_OLD));
    }
    store.mark_t3(&[], &["Defs.lean"], change(T3_HEAD))?;
    let old_seq = store.status()?.last_event_seq;
    let err = store
        .record_verified("b-leaf", &receipt("b-leaf", T3_OLD, 4), change(T3_OLD))
        .unwrap_err();
    assert!(err.to_string().contains("current taint root"));
    assert_eq!(store.status()?.last_event_seq, old_seq);
    let report = store.recheck_stale(T3_OLD, "broker", |_| panic!("superseded T3 must not run"))?;
    assert!(report.verified.is_empty() && report.needs_reproof.is_empty());

    let mut visit = Vec::new();
    let report = store.recheck_stale(T3_HEAD, "broker", |c| {
        assert_eq!(c.prior_trust, Some(L5));
        assert_eq!(c.taint_root_commit.as_deref(), Some(T3_HEAD));
        visit.push(c.id.clone());
        Ok(if c.id == "b-leaf" {
            RecheckOutcome::Failed {
                diagnostics: "old proof no longer compiles".into(),
            }
        } else {
            RecheckOutcome::Verified(Box::new(receipt(&c.id, T3_HEAD, 10 + visit.len() as u32)))
        })
    })?;
    assert_eq!(visit, ["z-def", "b-leaf", "a-top"]);
    assert_eq!(report.verified, ["z-def", "a-top"]);
    assert_eq!(report.needs_reproof, ["b-leaf"]);
    let leaf = store.claim("b-leaf")?.unwrap();
    assert_eq!((leaf.local_trust, leaf.receipt_id), (L3, None));
    let top = store.claim("a-top")?.unwrap();
    assert_eq!((top.local_trust, top.effective_trust), (L5, L4a));
    assert_eq!(top.receipt_id.as_deref(), Some("receipt-13"));
    assert!(!top.stale && !top.effective_stale);
    assert_eq!((top.prior_trust, top.taint_root_commit), (None, None));
    assert_eq!(store.status()?.stale_claims, 1);
    let events = store.events_after(old_seq, 100)?;
    assert!(
        events
            .iter()
            .any(|event| event.event_type == "CLAIM_REPROOF_REQUESTED"
                && event.entity_id == "b-leaf"
                && event.payload["diagnostics"] == "old proof no longer compiles")
    );
    Ok(())
}

#[test]
fn t3_discards_a_receipt_if_state_changes_during_callback() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("state.sqlite");
    let mut store = StateStore::open(&path)?;
    let mut other = StateStore::open(&path)?;
    claim(&mut store, "goal", None, false)?;
    store.record_verified("goal", &receipt("goal", BASE, 1), change(BASE))?;
    store.mark_t3(&["goal"], &[], change(T3_HEAD))?;
    let seq = store.status()?.last_event_seq;
    let err = store
        .recheck_stale(T3_HEAD, "broker", |c| {
            other.append_event(
                change(T3_HEAD),
                "CONCURRENT_EVENT",
                "claim",
                &c.id,
                json!({}),
            )?;
            Ok(RecheckOutcome::Verified(Box::new(receipt(
                &c.id, T3_HEAD, 2,
            ))))
        })
        .unwrap_err();
    assert!(err.to_string().contains("discard result"));
    let events = store.events_after(seq, 10)?;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event_type, "CONCURRENT_EVENT");
    let goal = store.claim("goal")?.unwrap();
    assert!(goal.stale);
    assert_eq!(goal.receipt_id.as_deref(), Some("receipt-1"));
    Ok(())
}

#[test]
fn cyclic_plan_rolls_back_dependencies_and_events() -> Result<()> {
    let mut store = StateStore::open(":memory:")?;
    claim(&mut store, "first", None, false)?;
    claim(&mut store, "second", None, false)?;
    plan(&mut store, "forward", "second", &["first"])?;
    let before = store.status()?;
    let err = plan(&mut store, "cycle", "first", &["second"]).unwrap_err();
    assert!(err.to_string().contains("cycle"));
    assert!(store.plan_state("cycle")?.is_none());
    let after = store.status()?;
    assert_eq!(after.proof_plans, before.proof_plans);
    assert_eq!(after.last_event_seq, before.last_event_seq);
    let failed = store.create_claim(
        NewClaim {
            id: "draft",
            title: "draft",
            informal_statement: "draft",
            engine: "lean4",
            formal_target: None,
            module_path: None,
            critical: false,
        },
        Change {
            actor: "",
            git_commit: Some(BASE),
        },
    );
    assert!(failed.is_err());
    assert!(store.claim("draft")?.is_none());
    assert_eq!(store.status()?.last_event_seq, before.last_event_seq);
    Ok(())
}

#[test]
fn append_only_rejects_replacement_without_recursive_triggers() -> Result<()> {
    let mut store = StateStore::open(":memory:")?;
    store.db.execute_batch("PRAGMA recursive_triggers=OFF;")?;
    claim(&mut store, "immutable", None, false)?;
    let mutation=store.db.execute("INSERT OR REPLACE INTO events (event_seq,occurred_at,event_type,entity_type,entity_id,actor) VALUES (1,'timestamp','changed','claim','immutable','test')",[]);
    assert!(mutation.is_err());
    assert_eq!(store.events_after(0, 10)?[0].event_type, "CLAIM_CREATED");
    claim(&mut store, "next", None, false)?;
    assert_eq!(store.events_after(0, 10)?[1].event_seq, 2);
    Ok(())
}

#[test]
fn multi_hop_refutation_exhausts_only_known_routes_and_reopens_on_a_new_alternative() -> Result<()>
{
    let dir = tempfile::tempdir()?;
    let db = dir.path().join("state.sqlite");
    let mut store = StateStore::open(&db)?;
    for id in ["top", "middle", "leaf", "other", "no_plan"] {
        claim(&mut store, id, None, id == "top" || id == "no_plan")?;
    }
    for id in ["top", "middle", "leaf", "other"] {
        store.set_local_trust(id, Trust::L3, change(BASE))?;
    }
    plan(&mut store, "t", "top", &["middle"])?;
    plan(&mut store, "m", "middle", &["leaf"])?;
    assert_eq!(store.claim("top")?.unwrap().effective_trust, Trust::L3);
    store.hard_refute("leaf", "GRV-leaf", change(BASE))?;
    assert_eq!(
        store.plan_state("m")?,
        Some(("INVALIDATED_BY_REFUTATION".into(), Trust::L0))
    );
    assert_eq!(
        store.plan_state("t")?,
        Some(("BLOCKED_BY_REFUTATION".into(), Trust::L0))
    );
    let top = store.claim("top")?.unwrap();
    assert_eq!(
        (top.local_trust, top.effective_trust),
        (Trust::L3, Trust::L0)
    );
    assert_eq!(store.status()?.pending_decisions, 1);
    let page = store.frontier_page(16)?;
    assert!(
        page.plans
            .items
            .iter()
            .any(|p| p.plan_id == "t" && p.plan_status == "BLOCKED_BY_REFUTATION")
    );
    let p0 = store
        .events_after(0, 100)?
        .into_iter()
        .find(|e| e.event_type == "CRITICAL_CLAIM_REFUTED")
        .unwrap();
    assert_eq!(p0.entity_id, "top");
    assert_eq!(p0.payload["refuted_claim"], "leaf");
    assert_eq!(p0.payload["claim_itself_refuted"], false);
    drop(store);
    let mut store = StateStore::open(&db)?;
    assert_eq!(store.status()?.pending_decisions, 1);
    assert_eq!(store.plan_state("t")?.unwrap().0, "BLOCKED_BY_REFUTATION");
    plan(&mut store, "backup", "middle", &["other"])?;
    assert_eq!(store.plan_state("t")?, Some(("ACTIVE".into(), Trust::L3)));
    assert_eq!(
        store.plan_state("m")?.unwrap().0,
        "INVALIDATED_BY_REFUTATION"
    );
    assert_eq!(store.status()?.pending_decisions, 1); // Historical inbox entry is never silently deleted.
    assert!(
        store
            .events_after(p0.event_seq, 100)?
            .iter()
            .any(|e| e.event_type == "CLAIM_ROUTES_REOPENED" && e.entity_id == "top")
    );
    Ok(())
}

#[test]
fn t3_does_not_taint_claims_through_refuted_or_archived_routes() -> Result<()> {
    let mut store = StateStore::open(":memory:")?;
    for id in ["top", "via_invalid", "via_archive", "root_refuted", "other"] {
        let module = format!("{id}.lean");
        claim(&mut store, id, Some(&module), false)?;
    }
    plan(
        &mut store,
        "invalid",
        "top",
        &["via_invalid", "root_refuted"],
    )?;
    plan(&mut store, "archive", "top", &["via_archive"])?;
    plan(&mut store, "live", "top", &["other"])?;
    store.db.execute(
        "UPDATE proof_plans SET status='ARCHIVED_BY_ADR' WHERE plan_id='archive'",
        [],
    )?;
    store.hard_refute("root_refuted", "GRV-root", change(BASE))?;
    let outcome = store.mark_t3(&["via_invalid", "via_archive"], &[], change(T3_HEAD))?;
    assert_eq!(outcome.claims, ["via_archive", "via_invalid"]);
    assert!(!store.claim("top")?.unwrap().stale);
    assert!(!store.claim("top")?.unwrap().effective_stale);
    Ok(())
}

#[test]
fn migration_backfills_route_projection_without_repeating_legacy_alert() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let db = dir.path().join("state.sqlite");
    let mut store = StateStore::open(&db)?;
    for id in ["leaf", "middle", "top"] {
        claim(&mut store, id, None, id != "leaf")?;
    }
    plan(&mut store, "top-route", "top", &["middle"])?;
    plan(&mut store, "middle-route", "middle", &["leaf"])?;
    // Simulate the pre-migration state: direct invalidation + one legacy alert.
    store.write(|tx| {
        let leaf = require_claim(tx, "leaf")?;
        update_trust(
            tx,
            &leaf,
            Trust::L2b,
            None,
            change(BASE),
            json!({"wiki_id":"witness:leaf"}),
        )?;
        invalidate_against_known_refutations(tx, "middle-route", change(BASE))?;
        critical_alert(tx, "middle", "leaf", change(BASE))?;
        tx.execute("DELETE FROM state_migrations WHERE version=1", [])?;
        Ok(())
    })?;
    drop(store);
    let store = StateStore::open(&db)?;
    assert_eq!(
        store.plan_state("top-route")?.unwrap().0,
        "BLOCKED_BY_REFUTATION"
    );
    let alerts = store
        .events_after(0, 100)?
        .into_iter()
        .filter(|e| e.event_type == "CRITICAL_CLAIM_REFUTED")
        .map(|e| e.entity_id)
        .collect::<Vec<_>>();
    assert_eq!(alerts, ["middle", "top"]);
    assert_eq!(StateStore::open(&db)?.status()?.pending_decisions, 2);
    Ok(())
}

#[test]
fn t3_approval_is_compare_and_apply_with_nonmutating_deferred_results() -> Result<()> {
    let mut store = StateStore::open(":memory:")?;
    claim(&mut store, "C", Some("Spec.lean"), false)?;
    store.record_verified("C", &receipt("C", BASE, 1), change(BASE))?;
    let snapshot = store.preview_t3(&[], &["Spec.lean"])?;
    let mut approval = T3PlanApproval {
        plan_sha256: "a".repeat(64),
        base_commit: BASE.into(),
        reviewed_commit: T3_HEAD.into(),
        anchor_module: "Spec.lean".into(),
        preview_event_seq: snapshot.event_seq,
    };
    store.append_event(change(BASE), "OTHER", "claim", "C", json!({}))?;
    assert!(store.apply_t3_plan(&approval, &["Spec.lean"]).is_err());
    assert!(store.applied_t3_plan(&approval.plan_sha256)?.is_none());
    assert!(!store.claim("C")?.unwrap().stale);
    approval.preview_event_seq = store.status()?.last_event_seq;
    assert_eq!(
        store
            .apply_t3_plan(&approval, &["Spec.lean"])?
            .unwrap()
            .claims,
        ["C"]
    );
    assert_eq!(
        store.applied_t3_plan(&approval.plan_sha256)?,
        Some(approval.clone())
    );
    let seq = store.status()?.last_event_seq;
    let report = store.recheck_stale(T3_HEAD, "fixture", |_| {
        Ok(RecheckOutcome::Deferred {
            reason: "dependency blocked".into(),
        })
    })?;
    assert_eq!(
        report.deferred.get("C").map(String::as_str),
        Some("dependency blocked")
    );
    assert!(report.verified.is_empty() && report.needs_reproof.is_empty());
    assert!(store.claim("C")?.unwrap().stale);
    assert!(store.apply_t3_plan(&approval, &["Spec.lean"])?.is_none());
    assert_eq!(seq, store.status()?.last_event_seq);
    Ok(())
}

#[test]
fn t3_plan_uses_applied_scope_not_same_commit_or_current_graph_impact() -> Result<()> {
    let mut store = StateStore::open(":memory:")?;
    claim(&mut store, "planned", Some("Spec.lean"), false)?;
    store.record_verified("planned", &receipt("planned", BASE, 1), change(BASE))?;
    for (id, nonce) in [("outside-before", 2), ("outside-after", 3)] {
        claim(&mut store, id, Some("Other.lean"), false)?;
        let mut r = receipt(id, BASE, nonce);
        r.file_path = "Other.lean".into();
        store.record_verified(id, &r, change(BASE))?;
    }
    let operator = Change {
        actor: T3_OPERATOR,
        git_commit: Some(T3_HEAD),
    };
    store.mark_t3(&["outside-before"], &[], operator)?;
    let approval = apply_reviewed_fixture_plan(&mut store, "Spec.lean")?;
    let hash = &approval.plan_sha256;
    store.mark_t3(&["outside-after"], &[], operator)?;
    // Refreshing the preview now would also capture a newly related claim.
    plan(&mut store, "new-dependent", "outside-after", &["planned"])?;
    assert_eq!(
        store.preview_t3(&[], &["Spec.lean"])?.impact.claims,
        ["planned", "outside-after"]
    );
    let seq = store.status()?.last_event_seq;
    let page = store.t3_worklist(hash, 64, None)?;
    assert_eq!(page.plan_sha256, *hash);
    assert_eq!(page.reviewed_commit, T3_HEAD);
    assert_eq!(page.last_event_seq, seq);
    assert_eq!(
        page.entries,
        [T3WorklistEntry {
            claim_id: "planned".into(),
            status: T3WorklistStatus::Pending,
        }]
    );
    assert!(page.next_after_claim.is_none());
    for id in ["outside-before", "outside-after", "missing"] {
        let err = store.t3_worklist(hash, 1, Some(id)).unwrap_err();
        assert!(err.to_string().contains("invalid T3 worklist cursor"));
        let err = store
            .recheck_one_stale_for_plan(hash, id, "test", |_| panic!("out of scope"))
            .unwrap_err();
        assert!(err.to_string().contains("not in the applied T3 plan"));
    }
    assert_eq!(store.status()?.last_event_seq, seq);
    let report = store.recheck_stale_for_plan(hash, "test", |c| {
        assert_eq!(c.id, "planned");
        Ok(RecheckOutcome::Verified(Box::new(receipt(
            &c.id, T3_HEAD, 4,
        ))))
    })?;
    assert_eq!(report.verified, ["planned"]);
    assert!(report.deferred.is_empty() && report.needs_reproof.is_empty());
    for id in ["outside-before", "outside-after"] {
        assert!(store.claim(id)?.unwrap().stale);
    }
    // Legacy commit-based recheck can still visit the unbound claims.
    let report = store.recheck_stale(T3_HEAD, "test", |c| {
        assert_ne!(c.id, "planned");
        Ok(RecheckOutcome::Deferred {
            reason: "fixture".into(),
        })
    })?;
    assert_eq!(
        report
            .deferred
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["outside-after", "outside-before"]
    );
    Ok(())
}

#[test]
fn t3_worklist_bounds_pages_and_keeps_cursors_when_claims_change_status() -> Result<()> {
    use T3WorklistStatus::*;
    let dir = tempfile::tempdir()?;
    let db = dir.path().join("state.sqlite");
    let mut store = StateStore::open(&db)?;
    for i in 0..=64 {
        claim(&mut store, &format!("scope-{i:02}"), None, false)?;
    }
    for (id, nonce) in [("scope-00", 1), ("scope-64", 2)] {
        store.record_verified(id, &receipt(id, BASE, nonce), change(BASE))?;
    }
    let approval = apply_reviewed_fixture_plan(&mut store, "Spec.lean")?;
    let hash = &approval.plan_sha256;
    // The current graph determines ordering, not the original event payload order.
    plan(&mut store, "new-route", "scope-00", &["scope-64"])?;
    let mut reader = StateStore::open_readonly(&db)?;
    assert!(reader.t3_worklist(hash, 0, None).is_err());
    assert!(reader.t3_worklist(hash, 65, None).is_err());
    assert!(reader.t3_worklist(&"b".repeat(64), 64, None).is_err());
    let head = reader.t3_worklist(hash, 64, None)?;
    assert_eq!(head.entries.len(), 64);
    assert_eq!(head.next_after_claim.as_deref(), Some("scope-64"));
    assert_eq!(head.entries[0].claim_id, "scope-01");
    assert_eq!(head.entries[0].status, NotEligible);
    assert_eq!(head.entries[63].claim_id, "scope-64");
    assert_eq!(head.entries[63].status, Pending);
    let report = store.recheck_one_stale_for_plan(hash, "scope-64", "test", |c| {
        assert_eq!(c.id, "scope-64");
        Ok(RecheckOutcome::Failed {
            diagnostics: "private diagnostics".into(),
        })
    })?;
    assert_eq!(report.needs_reproof, ["scope-64"]);
    let seq = store.status()?.last_event_seq;
    let replay = store.recheck_one_stale_for_plan(hash, "scope-64", "test", |_| {
        panic!("resolved claim must not replay")
    })?;
    assert!(
        replay.verified.is_empty() && replay.needs_reproof.is_empty() && replay.deferred.is_empty()
    );
    assert_eq!(store.status()?.last_event_seq, seq);
    let head_after = reader.t3_worklist(hash, 64, None)?;
    assert_eq!(head_after.entries[63].status, Resolved);
    assert_eq!(head_after.next_after_claim, head.next_after_claim);
    assert!(head_after.last_event_seq > head.last_event_seq);
    assert_eq!(head_after.last_event_seq, seq);
    assert!(!serde_json::to_string(&head_after)?.contains("private diagnostics"));
    let tail = reader.t3_worklist(hash, 64, head.next_after_claim.as_deref())?;
    assert_eq!(
        tail.entries,
        [T3WorklistEntry {
            claim_id: "scope-00".into(),
            status: Pending
        }]
    );
    assert!(tail.next_after_claim.is_none());
    assert!(
        reader
            .t3_worklist(hash, 1, Some("scope-00"))?
            .entries
            .is_empty()
    );

    store.mark_t3(&["scope-00"], &[], change(T3_OLD))?;
    assert_eq!(
        reader.t3_worklist(hash, 1, Some("scope-64"))?.entries[0].status,
        NotEligible
    );
    let report = store.recheck_one_stale_for_plan(hash, "scope-00", "test", |_| {
        panic!("another commit's recheck must not run")
    })?;
    assert!(
        report.verified.is_empty() && report.needs_reproof.is_empty() && report.deferred.is_empty()
    );
    Ok(())
}

#[test]
fn t3_batch_bounds_examined_approved_rows_in_dependency_order() -> Result<()> {
    let mut store = StateStore::open(":memory:")?;
    for i in 0..10 {
        let id = format!("scope-{i:02}");
        claim(&mut store, &id, None, false)?;
        if i != 1 {
            store.record_verified(&id, &receipt(&id, BASE, i + 1), change(BASE))?;
        }
    }
    claim(&mut store, "outside", Some("Other.lean"), false)?;
    let mut outside = receipt("outside", BASE, 20);
    outside.file_path = "Other.lean".into();
    store.record_verified("outside", &outside, change(BASE))?;
    plan(&mut store, "late-dependency", "scope-00", &["scope-09"])?;
    let approval = apply_reviewed_fixture_plan(&mut store, "Spec.lean")?;
    let hash = &approval.plan_sha256;
    store.mark_t3(&["outside"], &[], change(T3_HEAD))?;
    store.recheck_one_stale_for_plan(hash, "scope-02", "test", |c| {
        Ok(RecheckOutcome::Verified(Box::new(receipt(
            &c.id, T3_HEAD, 21,
        ))))
    })?;
    let seq = store.status()?.last_event_seq;
    let unexpected = |_: &ClaimState| panic!("invalid batch must not invoke checker");
    for limit in [0, 9] {
        let err = store
            .recheck_page_stale_for_plan(hash, limit, None, "test", unexpected)
            .unwrap_err();
        assert!(err.to_string().contains("limit must be 1..=8"));
    }
    let err = store
        .recheck_page_stale_for_plan(&"b".repeat(64), 8, None, "test", unexpected)
        .unwrap_err();
    assert!(err.to_string().contains("T3 plan has not been applied"));
    let err = store
        .recheck_page_stale_for_plan(hash, 8, Some(("outside", seq)), "test", unexpected)
        .unwrap_err();
    assert!(err.to_string().contains("invalid T3 worklist cursor"));

    let mut calls = Vec::new();
    let mut defer = |c: &ClaimState| {
        calls.push(c.id.clone());
        Ok(RecheckOutcome::Deferred {
            reason: "fixture".into(),
        })
    };
    let first = store.recheck_page_stale_for_plan(hash, 8, None, "test", &mut defer)?;
    assert_eq!(first.plan_sha256, *hash);
    assert_eq!(first.reviewed_commit, T3_HEAD);
    assert_eq!(
        first.examined_claims,
        [
            "scope-01", "scope-02", "scope-03", "scope-04", "scope-05", "scope-06", "scope-07",
            "scope-08",
        ]
    );
    assert_eq!(first.report.deferred.len(), 6);
    assert!(first.report.verified.is_empty() && first.report.needs_reproof.is_empty());
    assert_eq!(first.next_after_claim.as_deref(), Some("scope-08"));
    assert_eq!(first.last_event_seq, seq);
    let tail = store.recheck_page_stale_for_plan(
        hash,
        8,
        Some((
            first.next_after_claim.as_deref().unwrap(),
            first.last_event_seq,
        )),
        "test",
        &mut defer,
    )?;
    assert_eq!(tail.examined_claims, ["scope-09", "scope-00"]);
    assert_eq!(tail.report.deferred.len(), 2);
    assert!(tail.next_after_claim.is_none());
    assert_eq!(tail.last_event_seq, seq);
    assert_eq!(
        calls,
        [
            "scope-03", "scope-04", "scope-05", "scope-06", "scope-07", "scope-08", "scope-09",
            "scope-00",
        ]
    );
    assert!(store.claim("outside")?.unwrap().stale);
    assert_eq!(store.status()?.last_event_seq, seq);
    Ok(())
}

#[test]
fn t3_batch_resume_uses_post_commit_seq_and_does_not_replay_receipts() -> Result<()> {
    let mut store = StateStore::open(":memory:")?;
    for (id, nonce) in [("A", 1), ("B", 2), ("C", 3)] {
        claim(&mut store, id, None, false)?;
        store.record_verified(id, &receipt(id, BASE, nonce), change(BASE))?;
    }
    let approval = apply_reviewed_fixture_plan(&mut store, "Spec.lean")?;
    let hash = &approval.plan_sha256;
    let seq = store.status()?.last_event_seq;
    let first = store.recheck_page_stale_for_plan(hash, 1, None, "test", |c| {
        assert_eq!(c.id, "A");
        Ok(RecheckOutcome::Verified(Box::new(receipt(
            &c.id, T3_HEAD, 4,
        ))))
    })?;
    assert_eq!(first.examined_claims, ["A"]);
    assert_eq!(first.report.verified, ["A"]);
    assert_eq!(first.last_event_seq, store.status()?.last_event_seq);
    assert!(first.last_event_seq > seq);
    let cursor = first.next_after_claim.as_deref().unwrap();
    let err = store
        .recheck_page_stale_for_plan(hash, 1, Some((cursor, seq)), "test", |_| {
            panic!("pre-batch sequence must not authorize another check")
        })
        .unwrap_err();
    assert!(err.to_string().contains("cursor is stale"));
    let second = store.recheck_page_stale_for_plan(
        hash,
        1,
        Some((cursor, first.last_event_seq)),
        "test",
        |c| {
            assert_eq!(c.id, "B");
            Ok(RecheckOutcome::Verified(Box::new(receipt(
                &c.id, T3_HEAD, 5,
            ))))
        },
    )?;
    assert_eq!(second.report.verified, ["B"]);
    assert_eq!(second.last_event_seq, store.status()?.last_event_seq);

    // A new dependency can move an unexamined claim ahead of the old cursor.
    plan(&mut store, "new-dependency", "B", &["C"])?;
    let cursor = second.next_after_claim.as_deref().unwrap();
    let err = store
        .recheck_page_stale_for_plan(
            hash,
            1,
            Some((cursor, second.last_event_seq)),
            "test",
            |_| panic!("cursor from an old graph must not authorize another check"),
        )
        .unwrap_err();
    assert!(err.to_string().contains("cursor is stale"));
    let replay = store.recheck_page_stale_for_plan(hash, 8, None, "test", |c| {
        assert_eq!(c.id, "C");
        Ok(RecheckOutcome::Verified(Box::new(receipt(
            &c.id, T3_HEAD, 6,
        ))))
    })?;
    assert_eq!(replay.examined_claims, ["A", "C", "B"]);
    assert_eq!(replay.report.verified, ["C"]);
    assert!(replay.next_after_claim.is_none());
    assert_eq!(
        store.claim("A")?.unwrap().receipt_id.as_deref(),
        Some("receipt-4")
    );
    assert_eq!(
        store.claim("B")?.unwrap().receipt_id.as_deref(),
        Some("receipt-5")
    );
    Ok(())
}

#[test]
fn t3_batch_rejects_concurrent_graph_change_during_deferred_check() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let db = dir.path().join("state.sqlite");
    let mut store = StateStore::open(&db)?;
    let mut other = StateStore::open(&db)?;
    for id in ["A", "B"] {
        claim(&mut store, id, None, false)?;
    }
    store.record_verified("A", &receipt("A", BASE, 1), change(BASE))?;
    let approval = apply_reviewed_fixture_plan(&mut store, "Spec.lean")?;
    let hash = &approval.plan_sha256;
    let err = store
        .recheck_page_stale_for_plan(hash, 1, None, "test", |c| {
            assert_eq!(c.id, "A");
            plan(&mut other, "concurrent-dependency", "A", &["B"])?;
            Ok(RecheckOutcome::Deferred {
                reason: "fixture".into(),
            })
        })
        .unwrap_err();
    assert!(err.to_string().contains("state changed during T3 recheck"));
    assert!(store.claim("A")?.unwrap().stale);
    let seq = store.status()?.last_event_seq;
    let retry = store.recheck_page_stale_for_plan(hash, 1, None, "test", |_| {
        panic!("new first row was never L5")
    })?;
    assert_eq!(retry.examined_claims, ["B"]);
    assert_eq!(retry.next_after_claim.as_deref(), Some("B"));
    assert_eq!(retry.last_event_seq, seq);
    Ok(())
}

#[test]
fn t3_batch_keeps_prior_receipt_durable_after_mid_page_error() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let db = dir.path().join("state.sqlite");
    let mut store = StateStore::open(&db)?;
    for (id, nonce) in [("A", 1), ("B", 2), ("C", 3)] {
        claim(&mut store, id, None, false)?;
        store.record_verified(id, &receipt(id, BASE, nonce), change(BASE))?;
    }
    let approval = apply_reviewed_fixture_plan(&mut store, "Spec.lean")?;
    let hash = &approval.plan_sha256;
    let err = store
        .recheck_page_stale_for_plan(hash, 8, None, "test", |c| match c.id.as_str() {
            "A" => Ok(RecheckOutcome::Verified(Box::new(receipt(
                &c.id, T3_HEAD, 4,
            )))),
            "B" => Ok(RecheckOutcome::Verified(Box::new(receipt(
                &c.id, T3_OLD, 5,
            )))),
            _ => panic!("must stop after invalid receipt"),
        })
        .unwrap_err();
    assert!(
        err.to_string()
            .contains("receipt base does not match verification commit")
    );
    drop(store);

    let mut reopened = StateStore::open(&db)?;
    let a = reopened.claim("A")?.unwrap();
    assert!(!a.stale);
    assert_eq!(a.receipt_id.as_deref(), Some("receipt-4"));
    let b = reopened.claim("B")?.unwrap();
    assert!(b.stale);
    assert_eq!(b.receipt_id.as_deref(), Some("receipt-2"));
    assert!(reopened.claim("C")?.unwrap().stale);
    let replay =
        reopened.recheck_page_stale_for_plan(hash, 8, None, "test", |c| match c.id.as_str() {
            "B" => Ok(RecheckOutcome::Verified(Box::new(receipt(
                &c.id, T3_HEAD, 5,
            )))),
            "C" => Ok(RecheckOutcome::Failed {
                diagnostics: "fixture".into(),
            }),
            _ => panic!("committed receipt must not be replayed"),
        })?;
    assert_eq!(replay.examined_claims, ["A", "B", "C"]);
    assert_eq!(replay.report.verified, ["B"]);
    assert_eq!(replay.report.needs_reproof, ["C"]);
    assert!(replay.next_after_claim.is_none());
    assert_eq!(replay.last_event_seq, reopened.status()?.last_event_seq);
    Ok(())
}

const REVIEWED_REPO: &str = "/reviewed/project";
const PROOF_DECL: &str = "Candidate.proof";

fn reviewed_claim(store: &mut StateStore, id: &str) -> Result<()> {
    store.create_claim(
        NewClaim {
            id,
            title: id,
            informal_statement: "fixture",
            engine: "lean4",
            formal_target: Some("Spec.lean#Candidate.proof"),
            module_path: Some("Spec.lean"),
            critical: false,
        },
        change(BASE),
    )
}

fn reviewed_binding(id: &str) -> ReviewedLeanSpec {
    ReviewedLeanSpec {
        repo: REVIEWED_REPO.into(),
        claim_id: id.into(),
        approved_git_commit: BASE.into(),
        spec_file: "Reviewed.lean".into(),
        spec_decl: "Reviewed.statement".into(),
        spec_prefix_sha256: "d".repeat(64),
    }
}

fn reviewed_intent(id: &str) -> PromotionIntent {
    PromotionIntent {
        repo: REVIEWED_REPO.into(),
        claim_id: id.into(),
        base_commit: BASE.into(),
        new_commit: T3_OLD.into(),
        file_path: "Spec.lean".into(),
        target_decl: PROOF_DECL.into(),
        proof_body: "exact True.intro".into(),
    }
}

// Unit-test the transaction using a private receipt projection, as the existing
// ledger tests do. Broker/CLI integration tests cover the sealed constructor.
fn compared_receipt(binding: &ReviewedLeanSpec, nonce: u32) -> VerifiedReceipt {
    let mut r = receipt(PROOF_DECL, BASE, nonce);
    r.lean_comparison = Some(reviewed_lean::ComparedSpec {
        spec_file: binding.spec_file.clone(),
        spec_decl: binding.spec_decl.clone(),
        spec_prefix_sha256: binding.spec_prefix_sha256.clone(),
        candidate_receipt_sha256: r.receipt_sha256.clone(),
        receipt_sha256: format!("{:064x}", nonce + 100),
    });
    r
}

#[test]
fn reviewed_lean_binding_is_durable_idempotent_and_has_no_trust_effect() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let db = dir.path().join("state.sqlite");
    let mut store = StateStore::open(&db)?;
    reviewed_claim(&mut store, "C")?;
    reviewed_claim(&mut store, "other")?;
    store.set_local_trust("C", Trust::L3, change(BASE))?;
    let binding = reviewed_binding("C");
    let seq = store.status()?.last_event_seq;
    store.bind_lean_spec(&binding)?;
    assert_eq!(store.managed_repo()?.as_deref(), Some(REVIEWED_REPO));
    assert_eq!(store.claim("C")?.unwrap().local_trust, Trust::L3);
    assert_eq!(store.events_after(seq, 2)?.len(), 1);
    assert_eq!(store.events_after(seq, 2)?[0].event_type, "LEAN_SPEC_BOUND");
    drop(store);

    let mut store = StateStore::open(&db)?;
    assert_eq!(store.lean_spec("C")?, Some(binding.clone()));
    assert_eq!(store.lean_spec("other")?, None);
    store.bind_lean_spec(&binding)?;
    let conflicting = ReviewedLeanSpec {
        approved_git_commit: T3_HEAD.into(),
        ..binding.clone()
    };
    assert!(store.bind_lean_spec(&conflicting).is_err());
    let wrong_repo = ReviewedLeanSpec {
        repo: "/reviewed/different".into(),
        ..reviewed_binding("other")
    };
    assert!(store.bind_lean_spec(&wrong_repo).is_err());
    assert!(
        store
            .db
            .execute("DELETE FROM reviewed_lean_specs WHERE claim_id='C'", [])
            .is_err()
    );
    assert!(
        store
            .db
            .execute(
                "UPDATE reviewed_lean_specs SET spec_decl='Other' WHERE claim_id='C'",
                []
            )
            .is_err()
    );
    assert!(store.db.execute("INSERT OR REPLACE INTO reviewed_lean_specs SELECT * FROM reviewed_lean_specs WHERE claim_id='C'", []).is_err());
    assert_eq!(store.lean_spec("C")?, Some(binding));
    assert_eq!(store.status()?.last_event_seq, seq + 1);
    Ok(())
}

#[test]
fn reviewed_lean_binding_rejects_bad_claims_history_and_pending_journals() -> Result<()> {
    let mut store = StateStore::open(":memory:")?;
    claim(&mut store, "legacy", None, false)?;
    reviewed_claim(&mut store, "C")?;
    reviewed_claim(&mut store, "other")?;
    reviewed_claim(&mut store, "refuted")?;
    for (id, engine, module, target) in [
        ("stale", "lean4", "Stale.lean", "Stale.lean#Candidate.proof"),
        ("nonlean", "rocq", "Spec.lean", "Spec.lean#Candidate.proof"),
        (
            "different-module",
            "lean4",
            "Other.lean",
            "Spec.lean#Candidate.proof",
        ),
        (
            "malformed-target",
            "lean4",
            "Spec.lean",
            "Spec.lean#Candidate._",
        ),
    ] {
        store.create_claim(
            NewClaim {
                id,
                title: id,
                informal_statement: "fixture",
                engine,
                formal_target: Some(target),
                module_path: Some(module),
                critical: false,
            },
            change(BASE),
        )?;
    }
    store.hard_refute("refuted", "GRV-1", change(BASE))?;
    store.mark_t3(&["stale"], &[], change(T3_HEAD))?;
    for id in [
        "unknown",
        "legacy",
        "refuted",
        "stale",
        "nonlean",
        "different-module",
        "malformed-target",
    ] {
        assert!(
            store.bind_lean_spec(&reviewed_binding(id)).is_err(),
            "admitted {id}"
        );
    }
    let binding = reviewed_binding("C");
    for wrong in [
        ReviewedLeanSpec {
            spec_file: "SPEC.lean".into(),
            ..binding.clone()
        },
        ReviewedLeanSpec {
            spec_file: "Nested/Reviewed.lean".into(),
            ..binding.clone()
        },
        ReviewedLeanSpec {
            spec_decl: "Reviewed..statement".into(),
            ..binding.clone()
        },
        ReviewedLeanSpec {
            approved_git_commit: "a912".into(),
            ..binding.clone()
        },
        ReviewedLeanSpec {
            spec_prefix_sha256: "X".repeat(64),
            ..binding.clone()
        },
    ] {
        assert!(store.bind_lean_spec(&wrong).is_err());
    }
    assert!(store.managed_repo()?.is_none());
    let intent = reviewed_intent("C");
    let legacy = receipt(PROOF_DECL, BASE, 1);
    store.prepare_promotion(&intent, &legacy)?;
    assert!(store.bind_lean_spec(&reviewed_binding("other")).is_err());
    store.finish_promotion(&intent, &legacy)?;
    assert!(store.lean_spec_comparison(&intent.new_commit)?.is_none());
    // Even downgrading legacy L5 cannot make an old promotion eligible retroactively.
    store.set_local_trust("C", Trust::L3, change(BASE))?;
    assert!(store.bind_lean_spec(&binding).is_err());
    assert!(store.lean_spec("C")?.is_none());
    // A corrupt journal must also fail closed; this fixture need not construct an ADR.
    store.db.execute(
        "INSERT INTO pivot_resume_intent VALUES(1,'ADR','{}',?1)",
        [T3_HEAD],
    )?;
    assert!(store.bind_lean_spec(&reviewed_binding("other")).is_err());
    assert!(store.lean_spec("other")?.is_none());
    Ok(())
}

#[test]
fn reviewed_lean_promotion_requires_matching_comparison_and_records_it_atomically() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let db = dir.path().join("state.sqlite");
    let mut store = StateStore::open(&db)?;
    reviewed_claim(&mut store, "C")?;
    reviewed_claim(&mut store, "other")?;
    let binding = reviewed_binding("C");
    store.bind_lean_spec(&binding)?;
    let intent = reviewed_intent("C");
    let ordinary = receipt(PROOF_DECL, BASE, 1);
    let compared = compared_receipt(&binding, 2);
    assert!(store.prepare_promotion(&intent, &ordinary).is_err());
    assert!(store.bind_lean_spec(&reviewed_binding("other")).is_err());
    assert!(
        store
            .prepare_promotion(&reviewed_intent("other"), &ordinary)
            .is_err()
    );
    // A differently spelled legacy claim alias cannot bypass target protection.
    store.create_claim(
        NewClaim {
            id: "legacy-alias",
            title: "alias",
            informal_statement: "fixture",
            engine: "lean4",
            formal_target: Some(PROOF_DECL),
            module_path: Some("SPEC.lean"),
            critical: false,
        },
        change(BASE),
    )?;
    let mut legacy_alias = receipt(PROOF_DECL, BASE, 40);
    legacy_alias.file_path = "SPEC.lean".into();
    let alias_intent = PromotionIntent {
        file_path: "SPEC.lean".into(),
        ..reviewed_intent("legacy-alias")
    };
    assert!(
        store
            .prepare_promotion(&alias_intent, &legacy_alias)
            .is_err()
    );
    assert!(
        store
            .finish_promotion(&alias_intent, &legacy_alias)
            .is_err()
    );
    assert!(
        store
            .prepare_promotion(&reviewed_intent("other"), &compared)
            .is_err()
    );
    assert!(
        store
            .prepare_promotion(
                &PromotionIntent {
                    repo: "/elsewhere".into(),
                    ..intent.clone()
                },
                &compared
            )
            .is_err()
    );
    let mut wrong_candidate = compared_receipt(&binding, 3);
    wrong_candidate
        .lean_comparison
        .as_mut()
        .unwrap()
        .candidate_receipt_sha256 = ordinary.receipt_sha256.clone();
    assert!(store.prepare_promotion(&intent, &wrong_candidate).is_err());
    let wrong_contexts = [
        ReviewedLeanSpec {
            spec_file: "Other.lean".into(),
            ..binding.clone()
        },
        ReviewedLeanSpec {
            spec_decl: "Reviewed.other".into(),
            ..binding.clone()
        },
        ReviewedLeanSpec {
            spec_prefix_sha256: "f".repeat(64),
            ..binding.clone()
        },
    ];
    for (i, wrong) in wrong_contexts.iter().enumerate() {
        assert!(
            store
                .prepare_promotion(&intent, &compared_receipt(wrong, i as u32 + 10))
                .is_err()
        );
    }
    assert!(store.promotion_intent()?.is_none());
    assert!(store.finish_promotion(&intent, &compared).is_err());
    let valid_seq = store.status()?.last_event_seq;
    store.prepare_promotion(&intent, &compared)?;
    drop(store);
    let mut store = StateStore::open(&db)?;
    assert_eq!(store.promotion_intent()?, Some(intent.clone()));
    let wrong = compared_receipt(&wrong_contexts[2], 7);
    assert!(store.finish_promotion(&intent, &ordinary).is_err());
    assert!(store.finish_promotion(&intent, &wrong).is_err());
    assert!(store.lean_spec_comparison(&intent.new_commit)?.is_none());

    // Recovery re-verifies and can mint new candidate/comparison digests. Inject
    // a failed metadata write to prove trust and journal updates roll back too.
    let fresh = compared_receipt(&binding, 8);
    store.db.execute_batch(
        "CREATE TEMP TRIGGER stop_comparison BEFORE INSERT ON lean_spec_comparisons
        BEGIN SELECT RAISE(ABORT,'injected comparison write failure'); END;",
    )?;
    assert!(store.finish_promotion(&intent, &fresh).is_err());
    assert_eq!(store.promotion_intent()?, Some(intent.clone()));
    assert_eq!(store.claim("C")?.unwrap().local_trust, Trust::L0);
    assert_eq!(store.status()?.last_event_seq, valid_seq);
    store.db.execute_batch("DROP TRIGGER stop_comparison")?;

    let recorded = store.finish_promotion(&intent, &fresh)?;
    assert!(store.promotion_intent()?.is_none());
    assert_eq!(store.claim("C")?.unwrap().local_trust, Trust::L5);
    assert!(store.finish_promotion(&intent, &ordinary).is_err());
    assert_eq!(store.finish_promotion(&intent, &compared)?, recorded);
    store.bind_lean_spec(&binding)?;
    drop(store);
    let mut store = StateStore::open(&db)?;
    assert_eq!(
        store.lean_spec_comparison(&intent.new_commit)?,
        Some(LeanSpecComparison {
            promoted_commit: intent.new_commit,
            claim_id: intent.claim_id,
            base_commit: intent.base_commit,
            spec_file: binding.spec_file,
            spec_decl: binding.spec_decl,
            spec_prefix_sha256: binding.spec_prefix_sha256,
            candidate_receipt_sha256: fresh.receipt_sha256,
            receipt_sha256: fresh.lean_comparison.unwrap().receipt_sha256,
        })
    );
    store.mark_t3(&["C"], &[], change(T3_HEAD))?;
    let t3_seq = store.status()?.last_event_seq;
    let report = store.recheck_stale(T3_HEAD, "fixture", |_| panic!("bound T3 must defer"))?;
    assert!(report.deferred.contains_key("C"));
    assert!(report.verified.is_empty() && report.needs_reproof.is_empty());
    assert!(store.claim("C")?.unwrap().stale);
    assert_eq!(store.status()?.last_event_seq, t3_seq);
    Ok(())
}

fn promote_reviewed_fixture(store: &mut StateStore) -> Result<ReviewedLeanSpec> {
    reviewed_claim(store, "C")?;
    let binding = reviewed_binding("C");
    store.bind_lean_spec(&binding)?;
    let intent = reviewed_intent("C");
    let t1 = compared_receipt(&binding, 1);
    store.prepare_promotion(&intent, &t1)?;
    store.finish_promotion(&intent, &t1)?;
    Ok(binding)
}

fn apply_reviewed_fixture_plan(store: &mut StateStore, module: &str) -> Result<T3PlanApproval> {
    let preview = store.preview_t3(&[], &[module])?;
    let approval = T3PlanApproval {
        plan_sha256: "a".repeat(64),
        base_commit: T3_OLD.into(),
        reviewed_commit: T3_HEAD.into(),
        anchor_module: module.into(),
        preview_event_seq: preview.event_seq,
    };
    store.apply_t3_plan(&approval, &[module])?;
    Ok(approval)
}

fn compared_recheck(binding: &ReviewedLeanSpec, nonce: u32) -> VerifiedReceipt {
    let mut r = compared_receipt(binding, nonce);
    r.tier = "T3_TAINT_REBUILD".into();
    r.base_commit = T3_HEAD.into();
    r.regression_checked = false;
    r
}

#[test]
fn reviewed_t3_recheck_refuses_mismatch_and_generic_failure() -> Result<()> {
    let mut store = StateStore::open(":memory:")?;
    let binding = promote_reviewed_fixture(&mut store)?;
    let approval = apply_reviewed_fixture_plan(&mut store, "Spec.lean")?;
    let hash = &approval.plan_sha256;
    let seq = store.status()?.last_event_seq;
    assert!(
        store
            .recheck_stale_for_plan(&"b".repeat(64), "fixture", |_| panic!(
                "unapplied plan must not run"
            ))
            .is_err()
    );
    let failed = store.recheck_stale_for_plan(hash, "fixture", |_| {
        Ok(RecheckOutcome::Failed {
            diagnostics: "not enough evidence".into(),
        })
    })?;
    assert!(failed.deferred.contains_key("C"));
    assert!(failed.verified.is_empty() && failed.needs_reproof.is_empty());

    let patches: [(&str, fn(&mut VerifiedReceipt)); 9] = [
        ("comparison missing", |r| r.lean_comparison = None),
        ("wrong spec file", |r| {
            r.lean_comparison.as_mut().unwrap().spec_file = "Other.lean".into()
        }),
        ("wrong spec declaration", |r| {
            r.lean_comparison.as_mut().unwrap().spec_decl = "Reviewed.other".into()
        }),
        ("wrong spec prefix", |r| {
            r.lean_comparison.as_mut().unwrap().spec_prefix_sha256 = "e".repeat(64)
        }),
        ("wrong candidate receipt", |r| {
            r.lean_comparison.as_mut().unwrap().candidate_receipt_sha256 = "b".repeat(64)
        }),
        ("wrong taint snapshot", |r| r.base_commit = T3_OLD.into()),
        ("wrong target module", |r| r.file_path = "Other.lean".into()),
        ("wrong target body/source", |r| {
            r.patch_sha256 = "e".repeat(64)
        }),
        ("wrong receipt tier", |r| r.tier = "T1_PROMOTION".into()),
    ];
    for (label, patch) in patches {
        let result = store.recheck_stale_for_plan(hash, "fixture", |claim| {
            assert_eq!(claim.id, "C");
            let mut receipt = compared_recheck(&binding, 2);
            patch(&mut receipt);
            Ok(RecheckOutcome::Verified(Box::new(receipt)))
        });
        assert!(result.is_err(), "accepted {label}");
        assert!(store.lean_spec_recheck("C", hash)?.is_none());
        assert_eq!(store.status()?.last_event_seq, seq);
        let current = store.claim("C")?.unwrap();
        assert!(current.stale);
        assert_eq!(current.local_trust, Trust::L5);
        assert_eq!(current.receipt_id.as_deref(), Some("receipt-1"));
    }
    let receipts: i64 =
        store
            .db
            .query_row("SELECT count(*) FROM verification_receipts", [], |r| {
                r.get(0)
            })?;
    assert_eq!(receipts, 1);

    // Corrupt the retained provenance separately from the freshly verified candidate.
    // Rechecking must not replace the missing original comparison.
    store.db.execute_batch(
        "DROP TRIGGER lean_spec_comparisons_no_delete; DELETE FROM lean_spec_comparisons;",
    )?;
    let error = store
        .recheck_stale_for_plan(hash, "fixture", |_| {
            Ok(RecheckOutcome::Verified(Box::new(compared_recheck(
                &binding, 2,
            ))))
        })
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("missing its retained Lean comparison")
    );
    assert_eq!(store.status()?.last_event_seq, seq);
    Ok(())
}

#[test]
fn reviewed_t3_recheck_ledger_and_evidence_roll_back_and_replay_atomically() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("state.sqlite");
    let mut store = StateStore::open(&path)?;
    let binding = promote_reviewed_fixture(&mut store)?;
    let approval = apply_reviewed_fixture_plan(&mut store, "Spec.lean")?;
    let hash = &approval.plan_sha256;
    let ordinary = store.recheck_stale(T3_HEAD, "fixture", |_| {
        panic!("ordinary recheck ran bound claim")
    })?;
    assert!(ordinary.deferred.contains_key("C"));
    let check = |_: &ClaimState| {
        Ok(RecheckOutcome::Verified(Box::new(compared_recheck(
            &binding, 2,
        ))))
    };

    let mut other = StateStore::open(&path)?;
    let err = store
        .recheck_one_stale_for_plan(hash, "C", "fixture", |claim| {
            other.append_event(
                change(T3_HEAD),
                "CONCURRENT_EVENT",
                "claim",
                &claim.id,
                json!({}),
            )?;
            check(claim)
        })
        .unwrap_err();
    assert!(err.to_string().contains("discard result"));
    assert!(store.lean_spec_recheck("C", hash)?.is_none());
    let seq = store.status()?.last_event_seq;
    store.db.execute_batch(
        "CREATE TEMP TRIGGER stop_recheck BEFORE INSERT ON lean_spec_rechecks
        BEGIN SELECT RAISE(ABORT,'injected recheck write failure'); END;",
    )?;
    assert!(
        store
            .recheck_stale_for_plan(hash, "fixture", check)
            .is_err()
    );
    assert!(store.lean_spec_recheck("C", hash)?.is_none());
    assert_eq!(store.status()?.last_event_seq, seq);
    let stale = store.claim("C")?.unwrap();
    assert!(stale.stale && stale.effective_stale);
    assert_eq!(stale.receipt_id.as_deref(), Some("receipt-1"));
    store.db.execute_batch("DROP TRIGGER stop_recheck")?;

    // Reuse the same receipt identity after rollback, proving it was not persisted.
    let report = store.recheck_one_stale_for_plan(hash, "C", "fixture", check)?;
    assert_eq!(report.verified, ["C"]);
    assert_eq!(
        store.t3_worklist(hash, 1, None)?.entries[0].status,
        T3WorklistStatus::Resolved
    );
    assert!(report.deferred.is_empty() && report.needs_reproof.is_empty());
    let evidence = store.lean_spec_recheck("C", hash)?.unwrap();
    assert_eq!(
        evidence,
        LeanSpecRecheck {
            claim_id: "C".into(),
            plan_sha256: hash.clone(),
            reviewed_commit: T3_HEAD.into(),
            promoted_commit: T3_OLD.into(),
            spec_file: binding.spec_file.clone(),
            spec_decl: binding.spec_decl.clone(),
            spec_prefix_sha256: binding.spec_prefix_sha256.clone(),
            candidate_file: "Spec.lean".into(),
            candidate_decl: PROOF_DECL.into(),
            candidate_source_sha256: "f".repeat(64),
            candidate_receipt_sha256: format!("{:064x}", 2),
            comparison_receipt_sha256: format!("{:064x}", 102),
        }
    );
    let restored = store.claim("C")?.unwrap();
    assert!(!restored.stale && !restored.effective_stale);
    assert_eq!(restored.local_trust, Trust::L5);
    assert_eq!(restored.receipt_id.as_deref(), Some("receipt-2"));
    assert!(store.events_after(seq, 100)?.iter().any(|e| {
        e.event_type == "CLAIM_TRUST_CHANGED"
            && e.git_commit.as_deref() == Some(T3_HEAD)
            && e.payload["detail"]["lean_spec_recheck"] == json!(evidence)
    }));
    for sql in [
        "UPDATE lean_spec_rechecks SET spec_decl='Other'",
        "DELETE FROM lean_spec_rechecks",
        "INSERT OR REPLACE INTO lean_spec_rechecks SELECT * FROM lean_spec_rechecks",
    ] {
        assert!(store.db.execute(sql, []).is_err(), "accepted {sql}");
    }
    drop(store);
    let ro = StateStore::open_readonly(&path)?;
    assert_eq!(ro.lean_spec_recheck("C", hash)?, Some(evidence.clone()));
    assert!(ro.lean_spec_recheck("C", &"b".repeat(64))?.is_none());
    drop(ro);
    let mut store = StateStore::open(&path)?;
    let seq = store.status()?.last_event_seq;
    let report = store.recheck_stale_for_plan(hash, "fixture", |_| {
        panic!("completed claim must not replay")
    })?;
    assert!(
        report.verified.is_empty() && report.deferred.is_empty() && report.needs_reproof.is_empty()
    );
    assert_eq!(store.status()?.last_event_seq, seq);
    assert_eq!(store.lean_spec_recheck("C", hash)?, Some(evidence.clone()));

    store.mark_t3(&["C"], &[], change(T3_HEAD))?;
    let seq = store.status()?.last_event_seq;
    let report = store.recheck_stale_for_plan(hash, "fixture", |_| {
        panic!("same claim and plan must not replay")
    })?;
    assert!(report.deferred.contains_key("C"));
    assert_eq!(
        store.t3_worklist(hash, 1, None)?.entries[0].status,
        T3WorklistStatus::RetainedComparison
    );
    let one = store.recheck_one_stale_for_plan(hash, "C", "fixture", |_| {
        panic!("retained comparison must not replay")
    })?;
    assert_eq!(one.deferred, report.deferred);
    assert_eq!(store.status()?.last_event_seq, seq);
    assert_eq!(store.lean_spec_recheck("C", hash)?, Some(evidence));
    Ok(())
}

#[test]
fn reviewed_t3_recheck_does_not_accept_unbound_comparison_or_expand_plan_scope() -> Result<()> {
    let mut store = StateStore::open(":memory:")?;
    let binding = promote_reviewed_fixture(&mut store)?;
    store.create_claim(
        NewClaim {
            id: "legacy",
            title: "legacy",
            informal_statement: "fixture",
            engine: "lean4",
            formal_target: Some("Other.lean#Candidate.proof"),
            module_path: Some("Other.lean"),
            critical: false,
        },
        change(BASE),
    )?;
    let mut t1 = receipt(PROOF_DECL, BASE, 7);
    t1.file_path = "Other.lean".into();
    store.record_verified("legacy", &t1, change(BASE))?;
    let approval = apply_reviewed_fixture_plan(&mut store, "Other.lean")?;
    let hash = &approval.plan_sha256;
    store.mark_t3(&["C"], &[], change(T3_HEAD))?;
    let seq = store.status()?.last_event_seq;
    assert!(
        store
            .recheck_stale_for_plan(hash, "fixture", |claim| {
                assert_eq!(claim.id, "legacy");
                let mut compared = compared_recheck(&binding, 8);
                compared.file_path = "Other.lean".into();
                Ok(RecheckOutcome::Verified(Box::new(compared)))
            })
            .is_err()
    );
    assert_eq!(store.status()?.last_event_seq, seq);
    let report = store.recheck_stale_for_plan(hash, "fixture", |claim| {
        assert_eq!(claim.id, "legacy");
        let mut ordinary = receipt(PROOF_DECL, T3_HEAD, 8);
        ordinary.file_path = "Other.lean".into();
        Ok(RecheckOutcome::Verified(Box::new(ordinary)))
    })?;
    assert_eq!(report.verified, ["legacy"]);
    assert!(report.deferred.contains_key("C"));
    assert!(store.claim("C")?.unwrap().stale);
    assert!(store.lean_spec_recheck("C", hash)?.is_none());
    assert!(store.lean_spec_recheck("legacy", hash)?.is_none());
    Ok(())
}

#[test]
fn reviewed_lean_readonly_reads_legacy_ledger_without_migrating() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let db = dir.path().join("legacy.sqlite");
    let store = StateStore::open(&db)?;
    store
        .db
        .execute_batch("DROP TABLE lean_spec_rechecks; DROP TABLE lean_spec_comparisons; DROP TABLE reviewed_lean_specs;")?;
    drop(store);
    let ro = StateStore::open_readonly(&db)?;
    assert!(ro.lean_spec("older-claim")?.is_none());
    assert!(ro.lean_spec_comparison(BASE)?.is_none());
    assert!(
        ro.lean_spec_recheck("older-claim", &"f".repeat(64))?
            .is_none()
    );
    // Write-side gates never interpret a missing table as an unbound claim.
    assert!(reviewed_lean::read_binding(&ro.db, "older-claim").is_err());
    drop(ro);
    let mut migrated = StateStore::open(&db)?;
    reviewed_claim(&mut migrated, "C")?;
    migrated.bind_lean_spec(&reviewed_binding("C"))?;
    assert!(migrated.lean_spec("C")?.is_some());
    Ok(())
}
