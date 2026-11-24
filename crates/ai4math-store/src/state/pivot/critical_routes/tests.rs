use super::*;

const CHANGE: Change<'static> = Change {
    actor: "test",
    git_commit: Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
};

fn claim(
    store: &mut StateStore,
    id: &str,
    critical: bool,
    binding: Option<(&str, &str, &str)>,
) -> Result<()> {
    let (engine, formal_target, module_path) = match binding {
        Some((engine, module, def)) => (engine, Some(format!("{module}#{def}")), Some(module)),
        None => ("informal", None, None),
    };
    store.create_claim(
        NewClaim {
            id,
            title: id,
            informal_statement: "fixture",
            critical,
            engine,
            formal_target: formal_target.as_deref(),
            module_path,
        },
        CHANGE,
    )
}

fn plan(store: &mut StateStore, id: &str, target: &str, deps: &[&str]) -> Result<()> {
    store.create_plan(
        NewPlan {
            id,
            target,
            name: id,
            track: "exploration",
            status: PlanStatus::Active,
            dependencies: &deps
                .iter()
                .map(|&dep| (dep, "definition"))
                .collect::<Vec<_>>(),
        },
        CHANGE,
    )
}

fn trip(
    store: &mut StateStore,
    id: &str,
    (engine, module, def): (&str, &str, &str),
    critical: bool,
) -> Result<BreakerTripReport> {
    store.register_tripped_breaker_report(
        "/fixture.git",
        CHANGE.git_commit.unwrap(),
        id,
        def,
        module,
        engine,
        "fixture issue",
        critical,
    )
}

fn inbox_priority(store: &StateStore, id: &str) -> Result<String> {
    Ok(store.db.query_row(
        "SELECT priority FROM decision_inbox WHERE inbox_id=?1",
        [format!("DEF-{id}")],
        |r| r.get(0),
    )?)
}

const WATCH: (&str, &str, &str) = ("lean4", "Defs.lean", "Watched");
const OTHER: (&str, &str, &str) = ("lean4", "Other.lean", "Other");

#[test]
fn newly_cut_chain_and_routes_report_distinct_claims_and_keep_graph_intact() -> Result<()> {
    let mut store = StateStore::open(":memory:")?;
    claim(&mut store, "seed", false, Some(WATCH))?;
    for (id, critical) in [
        ("helper", false),
        ("z-side", true),
        ("a-top", true),
        ("and-input", false),
        ("stale", true),
        ("unrouted", true),
    ] {
        claim(&mut store, id, critical, None)?;
    }
    plan(&mut store, "helper-plan", "helper", &["seed", "and-input"])?;
    // z-side comes before a-top topologically, but not lexicographically.
    plan(&mut store, "side-plan", "z-side", &["helper"])?;
    plan(&mut store, "top-plan", "a-top", &["z-side"])?;
    plan(&mut store, "top-alt", "a-top", &["seed"])?;
    plan(&mut store, "stale-plan", "stale", &["seed"])?;
    store.db.execute(
        "UPDATE claims SET effective_stale=1 WHERE claim_id='stale'",
        [],
    )?;
    let old_claim = serde_json::to_value(store.claim("a-top")?)?;
    let old_plan = store.plan_state("helper-plan")?;

    let report = trip(&mut store, "trip", WATCH, true)?;
    assert_eq!(report.critical_claim_count, 2);
    let facts = store.briefing_for_alerts()?;
    let [alert] = facts.alert_sources.as_slice() else {
        panic!("expected one critical alert source")
    };
    assert_eq!(alert.event_type, CRITICAL_BREAKER_TRIPPED);
    assert_eq!(alert.entity_id, "trip");
    assert_eq!(
        alert.payload,
        json!({"state":"TRIPPED","inbox_id":"DEF-trip",
        "trip_event_seq":report.event_seq,"critical_claim_id":"a-top","critical_claim_count":2})
    );
    let [inbox] = facts.inbox.items.as_slice() else {
        panic!("expected one Inbox item")
    };
    assert_eq!(inbox.priority, "P0");
    assert_eq!(inbox.payload.as_ref().unwrap()["critical_claim_count"], 2);
    assert_eq!(serde_json::to_value(store.claim("a-top")?)?, old_claim);
    assert_eq!(store.plan_state("helper-plan")?, old_plan);
    assert_eq!(
        store.events_after(report.event_seq - 1, 2)?[0].event_type,
        "BREAKER_TRIPPED"
    );
    assert_eq!(
        trip(&mut store, "repeat", WATCH, true)?.critical_claim_count,
        0
    );
    assert_eq!(inbox_priority(&store, "repeat")?, "P1");
    Ok(())
}

#[test]
fn conservative_or_alternatives_and_already_blocked_and_routes_do_not_alert() -> Result<()> {
    let mut store = StateStore::open(":memory:")?;
    claim(&mut store, "seed", false, Some(WATCH))?;
    claim(&mut store, "prior", false, Some(OTHER))?;
    for (id, critical) in [
        ("leaf", false),
        ("soft-goal", true),
        ("prior-goal", true),
        ("dead", false),
        ("refuted-and", true),
        ("empty-goal", true),
    ] {
        claim(&mut store, id, critical, None)?;
    }
    plan(&mut store, "soft-watched", "soft-goal", &["seed"])?;
    plan(&mut store, "soft-alt", "soft-goal", &["leaf"])?;
    plan(&mut store, "prior-and", "prior-goal", &["seed", "prior"])?;
    plan(&mut store, "refuted-and", "refuted-and", &["seed", "dead"])?;
    plan(&mut store, "empty-watched", "empty-goal", &["seed"])?;
    plan(&mut store, "empty-alt", "empty-goal", &[])?;
    store.db.execute(
        "UPDATE proof_plans SET status='SUSPENDED_SOFT_PRUNE' WHERE plan_id='soft-alt'",
        [],
    )?;
    store.db.execute(
        "UPDATE claims SET stale=1,effective_stale=1 WHERE claim_id='leaf'",
        [],
    )?;
    // Seed an already known kernel refutation without creating another P0 fixture event.
    store.db.execute(
        "UPDATE claims SET local_trust_level='L2b' WHERE claim_id='dead'",
        [],
    )?;
    assert_eq!(
        trip(&mut store, "earlier", OTHER, false)?.critical_claim_count,
        0
    );
    assert_eq!(
        trip(&mut store, "current", WATCH, true)?.critical_claim_count,
        0
    );
    assert_eq!(inbox_priority(&store, "current")?, "P1");
    assert!(store.briefing_for_alerts()?.alert_sources.is_empty());
    assert!(store.module_breaker_gated(WATCH.0, WATCH.1)?);
    Ok(())
}

#[test]
fn archived_invalidated_and_graph_blocked_plans_are_not_alternatives() -> Result<()> {
    let mut store = StateStore::open(":memory:")?;
    claim(&mut store, "seed", false, Some(WATCH))?;
    claim(&mut store, "goal", true, None)?;
    claim(&mut store, "dead", false, None)?;
    for (id, deps) in [
        ("live", ["seed"].as_slice()),
        ("archive", [].as_slice()),
        ("invalid", [].as_slice()),
        ("graph-blocked", [].as_slice()),
    ] {
        plan(&mut store, id, "goal", deps)?;
    }
    store.db.execute(
        "UPDATE proof_plans SET status='ARCHIVED_BY_ADR' WHERE plan_id='archive'",
        [],
    )?;
    store.db.execute(
        "UPDATE proof_plans SET status='INVALIDATED_BY_REFUTATION' WHERE plan_id='invalid'",
        [],
    )?;
    store.db.execute(
        "UPDATE claims SET local_trust_level='L2b' WHERE claim_id='dead'",
        [],
    )?;
    store.db.execute(
        "INSERT INTO proof_plan_route_blocks VALUES('graph-blocked','dead')",
        [],
    )?;
    assert_eq!(
        trip(&mut store, "trip", WATCH, true)?.critical_claim_count,
        1
    );
    assert_eq!(inbox_priority(&store, "trip")?, "P0");
    Ok(())
}

#[test]
fn exact_engine_target_critical_enrollment_and_explicit_consent_are_required() -> Result<()> {
    let mut store = StateStore::open(":memory:")?;
    claim(&mut store, "direct", true, Some(WATCH))?;
    // Misnamed or misleading metadata are not graph bindings.
    claim(&mut store, "Unregistered", true, None)?;
    assert_eq!(
        trip(&mut store, "wrong-engine", ("rocq", WATCH.1, WATCH.2), true)?.critical_claim_count,
        0
    );
    assert_eq!(
        trip(&mut store, "direct", WATCH, true)?.critical_claim_count,
        1
    );
    assert_eq!(
        trip(
            &mut store,
            "wrong-target",
            (WATCH.0, WATCH.1, "Unregistered"),
            true
        )?
        .critical_claim_count,
        0
    );
    assert_eq!(
        trip(
            &mut store,
            "wrong-module",
            (WATCH.0, OTHER.1, WATCH.2),
            true
        )?
        .critical_claim_count,
        0
    );

    let mut store = StateStore::open(":memory:")?;
    claim(&mut store, "direct", true, Some(WATCH))?;
    assert_eq!(
        trip(
            &mut store,
            "prior-same-module",
            (WATCH.0, WATCH.1, "AnotherDecl"),
            false
        )?
        .critical_claim_count,
        0
    );
    assert_eq!(
        trip(&mut store, "gated-direct", WATCH, true)?.critical_claim_count,
        0
    );
    claim(&mut store, "other", true, Some(OTHER))?;
    assert_eq!(
        trip(&mut store, "not-authorized", OTHER, false)?.critical_claim_count,
        0
    );
    assert!(store.briefing_for_alerts()?.alert_sources.is_empty());
    Ok(())
}

#[test]
fn breaker_does_not_cut_a_current_established_premise() -> Result<()> {
    let mut store = StateStore::open(":memory:")?;
    claim(&mut store, "verified", true, Some(WATCH))?;
    claim(&mut store, "open", true, None)?;
    plan(&mut store, "uses-verified", "open", &["verified"])?;
    store.db.execute("UPDATE claims SET local_trust_level='L5',effective_trust_level='L5' WHERE claim_id='verified'", [])?;
    assert_eq!(
        trip(&mut store, "verified", WATCH, true)?.critical_claim_count,
        0
    );
    assert_eq!(inbox_priority(&store, "verified")?, "P1");
    assert!(store.briefing_for_alerts()?.alert_sources.is_empty());
    Ok(())
}

#[test]
fn reserved_append_cannot_forge_an_alert_and_historical_payload_still_projects() -> Result<()> {
    let mut store = StateStore::open(":memory:")?;
    assert!(
        store
            .append_event(
                CHANGE,
                CRITICAL_BREAKER_TRIPPED,
                "breaker",
                "spoof",
                json!({})
            )
            .is_err()
    );
    assert_eq!(store.status()?.last_event_seq, 0);
    // Only fixture SQL bypasses the typed API, to model existing historical rows.
    let payload = json!({"state":"TRIPPED","inbox_id":"DEF-old","trip_event_seq":1});
    store.write(|tx| {
        event(
            tx,
            CHANGE,
            CRITICAL_BREAKER_TRIPPED,
            "breaker",
            "old",
            payload.clone(),
        )
    })?;
    assert_eq!(
        store.briefing_for_alerts()?.alert_sources[0].payload,
        payload
    );
    Ok(())
}
