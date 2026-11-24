use super::*;

const REPO: &str = "/fixture.git";
const BASE: &str = "1111111111111111111111111111111111111111";
const FACT: &str = "2222222222222222222222222222222222222222222222222222222222222222";
const ADR_COMMIT: &str = "3333333333333333333333333333333333333333";
const CHANGE: Change<'static> = Change {
    actor: "fixture",
    git_commit: Some(BASE),
};

fn claim(
    store: &mut StateStore,
    id: &str,
    engine: &str,
    module: &str,
    target: &str,
    critical: bool,
) -> Result<()> {
    store.create_claim(
        NewClaim {
            id,
            title: id,
            informal_statement: "Fixture claim",
            engine,
            formal_target: Some(target),
            module_path: Some(module),
            critical,
        },
        CHANGE,
    )
}

fn plan(
    store: &mut StateStore,
    id: &str,
    target: &str,
    status: PlanStatus,
    deps: &[(&str, &str)],
) -> Result<()> {
    store.create_plan(
        NewPlan {
            id,
            target,
            status,
            name: id,
            track: "exploration",
            dependencies: deps,
        },
        CHANGE,
    )
}

fn setup(path: impl AsRef<Path>) -> Result<StateStore> {
    let mut store = StateStore::open(path)?;
    claim(
        &mut store,
        "goal",
        "lean4",
        "Defs.lean",
        "Defs.lean#Watched",
        true,
    )?;
    for id in ["dep-a", "dep-b", "refuted"] {
        claim(
            &mut store,
            id,
            "lean4",
            "Deps.lean",
            &format!("Deps.lean#{id}"),
            false,
        )?;
    }
    for (id, engine, module, target) in [
        ("wrong-target", "lean4", "Defs.lean", "Defs.lean#Other"),
        ("wrong-engine", "rocq", "Defs.lean", "Defs.lean#Watched"),
        ("wrong-module", "lean4", "Other.lean", "Defs.lean#Watched"),
    ] {
        claim(&mut store, id, engine, module, target, false)?;
        plan(&mut store, id, id, PlanStatus::Active, &[])?;
    }
    plan(
        &mut store,
        "a-edges",
        "goal",
        PlanStatus::Active,
        &[("dep-b", "lemma"), ("dep-a", "definition")],
    )?;
    plan(&mut store, "z-leaf", "goal", PlanStatus::Active, &[])?;
    plan(
        &mut store,
        "invalid",
        "goal",
        PlanStatus::Active,
        &[("refuted", "lemma")],
    )?;
    plan(
        &mut store,
        "proved",
        "wrong-target",
        PlanStatus::Proved,
        &[],
    )?;
    store.hard_refute("refuted", "GRAVE-fixture", CHANGE)?;
    for id in ["goal", "dep-a", "dep-b"] {
        store.set_local_trust(id, Trust::L3, CHANGE)?;
    }
    store.register_tripped_breaker(
        REPO,
        BASE,
        "breaker",
        "Watched",
        "Defs.lean",
        "lean4",
        "Fixture incident",
        false,
    )?;
    Ok(store)
}

fn review(store: &mut StateStore) -> Result<RetainedReview> {
    let facts = store.briefing_facts()?;
    Ok(RetainedReview {
        event_from_exclusive: facts.event_from_exclusive,
        event_to_inclusive: facts.event_to_inclusive,
        to_git_commit: facts.to_git_commit,
        checkpoint: facts.checkpoint,
        presentation_truncated: facts.frontier.plans.truncated,
        breakers: facts.breakers.items,
        inbox: facts.inbox.items,
        frontier_edges: facts.frontier.plans.items,
    })
}

fn preview(store: &mut StateStore, review: &RetainedReview, ids: &[&str]) -> Result<PivotPlan> {
    store.preview_archive_routes(REPO, BASE, FACT, review, "breaker", "Reviewed routes", ids)
}

#[test]
fn preview_requires_exact_complete_edges_for_each_selected_plan() -> Result<()> {
    let mut store = setup(":memory:")?;
    let mut review = review(&mut store)?;
    let a = preview(&mut store, &review, &["z-leaf", "a-edges"])?;
    assert_eq!(
        a.archive_plan_ids().collect::<Vec<_>>(),
        ["a-edges", "z-leaf"]
    );
    assert_eq!(a.archive_plans[0].target_claim_id, "goal");
    assert_eq!(a.archive_plans[0].frontier_edges.len(), 2);
    assert!(a.archive_plans[1].frontier_edges[0].dep_claim_id.is_none());
    assert_ne!(
        a.adr_id()?,
        preview(&mut store, &review, &["a-edges"])?.adr_id()?
    );
    review.frontier_edges.reverse();
    assert_eq!(a, preview(&mut store, &review, &["a-edges", "z-leaf"])?);
    assert!(
        store
            .preview_pivot(
                REPO,
                BASE,
                FACT,
                &review,
                "breaker",
                "Reviewed routes",
                PivotAction::ArchiveRoutes
            )
            .is_err()
    );
    assert!(PivotPlan::normalize_archive_plan_ids(&[]).is_err());
    assert!(
        PivotPlan::normalize_archive_plan_ids(&[
            "a0", "a1", "a2", "a3", "a4", "a5", "a6", "a7", "a8"
        ])
        .is_err()
    );
    assert!(preview(&mut store, &review, &["z-leaf", "z-leaf"]).is_err());
    for id in [
        "missing",
        "wrong-target",
        "wrong-engine",
        "wrong-module",
        "invalid",
    ] {
        assert!(preview(&mut store, &review, &[id]).is_err(), "{id}");
    }
    let mut bad = review.clone();
    let edge = bad
        .frontier_edges
        .iter_mut()
        .find(|e| e.plan_id == "a-edges")
        .unwrap();
    edge.dep_kind = Some("oracle_placeholder".into());
    assert!(preview(&mut store, &bad, &["a-edges"]).is_err());
    let mut bad = review.clone();
    bad.frontier_edges
        .push(a.archive_plans[0].frontier_edges[0].clone());
    assert!(preview(&mut store, &bad, &["a-edges"]).is_err());
    review.presentation_truncated = true;
    review
        .frontier_edges
        .retain(|e| e.plan_id == "z-leaf" || e.dep_claim_id.as_deref() == Some("dep-a"));
    assert!(preview(&mut store, &review, &["z-leaf"])?.presentation_truncated);
    assert!(
        preview(&mut store, &review, &["a-edges"])
            .unwrap_err()
            .to_string()
            .contains("exact complete edge set")
    );
    Ok(())
}

#[test]
fn preview_refuses_proved_blocked_and_protected_target_claims() -> Result<()> {
    let mut store = setup(":memory:")?;
    plan(&mut store, "proved-exact", "goal", PlanStatus::Proved, &[])?;
    let reviewed = review(&mut store)?;
    assert!(preview(&mut store, &reviewed, &["proved-exact"]).is_err());
    // Simulate nonjournaled projection changes to check the route's independent gates.
    for sql in [
        "UPDATE claims SET stale=1 WHERE claim_id='goal'",
        "UPDATE claims SET effective_stale=1 WHERE claim_id='goal'",
        "UPDATE claims SET local_trust_level='L2a' WHERE claim_id='goal'",
        "UPDATE claims SET local_trust_level='L2b' WHERE claim_id='goal'",
        "UPDATE claims SET local_trust_level='L5' WHERE claim_id='goal'",
        "INSERT INTO proof_plan_route_blocks VALUES('a-edges','refuted')",
        "UPDATE proof_plans SET status='SUSPENDED_SOFT_PRUNE' WHERE plan_id='a-edges'",
    ] {
        let sp = store.db.savepoint()?;
        sp.execute(sql, [])?;
        assert!(
            super::preview(
                &sp,
                &reviewed.breakers[0],
                "unused",
                &reviewed,
                &["a-edges"]
            )
            .is_err(),
            "allowed {sql}"
        );
        sp.finish()?;
    }
    Ok(())
}

#[test]
fn archives_atomically_and_recovery_fails_closed_on_selected_route_drift() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("state.sqlite");
    let mut store = setup(&path)?;
    let review = review(&mut store)?;
    let plan = preview(&mut store, &review, &["z-leaf", "a-edges"])?;
    let intent = PivotIntent {
        adr_id: plan.adr_id()?,
        plan,
        new_commit: ADR_COMMIT.into(),
    };
    let active = Some(("ACTIVE".into(), Trust::L3));
    assert_eq!(store.plan_state("a-edges")?, active);
    assert_eq!(store.plan_state("z-leaf")?, active);
    store.prepare_pivot(&intent)?;
    store.db.execute_batch(
        "CREATE TRIGGER interrupted BEFORE INSERT ON recorded_pivots
        BEGIN SELECT RAISE(ABORT, 'simulated post-CAS failure'); END",
    )?;
    let mut adopted = false;
    assert!(
        store
            .land_pivot(&intent, false, || {
                adopted = true;
                Ok(())
            })
            .is_err()
    );
    assert!(adopted);
    assert_eq!(store.plan_state("a-edges")?, active);
    assert_eq!(store.plan_state("z-leaf")?, active);
    assert_eq!(read_target(&store.db, "breaker")?.0.state, "TRIPPED");
    assert_eq!(last_seq(&store.db)?, review.event_to_inclusive);
    store.db.execute_batch("DROP TRIGGER interrupted")?;
    drop(store);

    let mut store = StateStore::open(&path)?;
    store.db.execute("UPDATE plan_dependencies SET dep_kind='lemma' WHERE plan_id='a-edges' AND dep_claim_id='dep-a'", [])?;
    assert!(
        store
            .land_pivot(&intent, true, || panic!(
                "route drift must fail before adoption"
            ))
            .is_err()
    );
    assert_eq!(read_target(&store.db, "breaker")?.0.state, "TRIPPED");
    assert_eq!(current_checkpoint(&store.db)?, review.checkpoint);
    assert_eq!(store.pivot_intent()?.as_ref(), Some(&intent));
    assert!(store.recorded_pivot(&intent.adr_id)?.is_none());
    store.db.execute("UPDATE plan_dependencies SET dep_kind='definition' WHERE plan_id='a-edges' AND dep_claim_id='dep-a'", [])?;
    store.append_event(CHANGE, "UNRELATED_TEST_EVENT", "test", "other", json!({}))?;
    let applied = store.land_pivot(&intent, true, || Ok(()))?;
    assert_eq!(applied.breaker_state, "ARCHIVED");
    assert!(store.module_breaker_gated("lean4", "Defs.lean")?);
    assert!(store.pivot_intent()?.is_none());
    for id in ["a-edges", "z-leaf"] {
        assert_eq!(
            store.plan_state(id)?,
            Some(("ARCHIVED_BY_ADR".into(), Trust::L0))
        );
    }
    assert_eq!(
        store.plan_state("wrong-target")?,
        Some(("ACTIVE".into(), Trust::L0))
    );
    assert_eq!(
        store.plan_state("invalid")?.unwrap().0,
        "INVALIDATED_BY_REFUTATION"
    );
    let goal = store.claim("goal")?.unwrap();
    assert_eq!(goal.local_trust, Trust::L3);
    assert_eq!(goal.effective_trust, Trust::L0);
    let deps: i64 = store.db.query_row(
        "SELECT count(*) FROM plan_dependencies WHERE plan_id='a-edges'",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(deps, 2);
    let resolved: (String, String) = store.db.query_row(
        "SELECT status,resolved_adr FROM decision_inbox WHERE inbox_id='DEF-breaker'",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    assert_eq!(resolved, ("RESOLVED".into(), intent.adr_id.clone()));
    let facts = store.briefing_for_alerts()?;
    assert_eq!(
        facts.checkpoint.unwrap().event_seq,
        review.event_to_inclusive
    );
    let archived: Vec<_> = facts
        .events
        .items
        .iter()
        .filter(|e| e.event_type == "PLAN_ARCHIVED")
        .collect();
    assert_eq!(
        archived
            .iter()
            .map(|e| e.entity_id.as_str())
            .collect::<Vec<_>>(),
        ["a-edges", "z-leaf"]
    );
    for event in archived {
        assert_eq!(event.entity_type, "proof_plan");
        assert_eq!(
            event.payload,
            json!({"target":"goal","adr_id":intent.adr_id})
        );
    }
    assert!(
        facts
            .events
            .items
            .iter()
            .any(|e| e.event_type == "CLAIM_ROUTES_EXHAUSTED" && e.entity_id == "goal")
    );
    assert!(facts.alert_sources.is_empty());
    assert!(facts.inbox.items.is_empty());
    Ok(())
}
