use ai4math_store::{Change, NewClaim, NewPlan, PlanStatus, StateStore, Trust};
use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use serde_json::{Value, json};

const BASE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const TAINT_ROOT: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn change(commit: &str) -> Change<'_> {
    Change {
        actor: "graph-replay-fixture",
        git_commit: Some(commit),
    }
}

fn claim(store: &mut StateStore, id: &str) -> Result<()> {
    store.create_claim(
        NewClaim {
            id,
            title: id,
            informal_statement: "Replay a premise: α.",
            engine: "lean4",
            formal_target: None,
            module_path: None,
            critical: false,
        },
        change(BASE),
    )
}

fn plan(store: &mut StateStore, id: &str, target: &str, deps: &[(&str, &str)]) -> Result<()> {
    store.create_plan(
        NewPlan {
            id,
            name: id,
            target,
            status: PlanStatus::Active,
            track: "exploration",
            dependencies: deps,
        },
        change(BASE),
    )
}

fn head(store: &StateStore) -> Result<i64> {
    Ok(store.status()?.last_event_seq)
}

fn event_seq(store: &StateStore, after: i64, id: &str, kind: &str) -> Result<i64> {
    let events = store.events_after(after, 100)?;
    let mut matching = events
        .iter()
        .filter(|event| event.entity_id == id && event.event_type == kind);
    let seq = matching
        .next()
        .with_context(|| format!("missing {kind} for {id} after {after}"))?
        .event_seq;
    assert!(matching.next().is_none(), "duplicate {kind} for {id}");
    Ok(seq)
}

fn replay(store: &StateStore, through: Option<i64>) -> Result<Value> {
    Ok(serde_json::to_value(store.replay_graph(through)?)?)
}

fn row<'a>(graph: &'a Value, collection: &str, key: &str, id: &str) -> &'a Value {
    graph[collection]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry[key] == id)
        .unwrap_or_else(|| panic!("no {collection} row for {id}"))
}

fn assert_matches_entity_replays(
    store: &StateStore,
    through: i64,
    ledger_head: i64,
    claim_ids: &[&str],
    plan_ids: &[&str],
    dependency_edge_count: usize,
) -> Result<Value> {
    let mut claims = Vec::new();
    let mut plans = Vec::new();
    let mut applied_claim_events = 0;
    let mut applied_plan_events = 0;
    for &id in claim_ids {
        let single = store.replay_claim(id, Some(through))?;
        applied_claim_events += single.applied_events;
        claims.push(single.claim.context("expected a retained claim")?);
    }
    for &id in plan_ids {
        let single = store.replay_plan(id, Some(through))?;
        applied_plan_events += single.applied_events;
        plans.push(single.plan.context("expected a retained plan")?);
    }
    let graph = replay(store, Some(through))?;
    assert_eq!(
        graph,
        json!({
            "through_event_seq": through,
            "ledger_head_event_seq": ledger_head,
            "applied_claim_events": applied_claim_events,
            "applied_plan_events": applied_plan_events,
            "dependency_edge_count": dependency_edge_count,
            "claims": claims,
            "plans": plans,
        }),
        "graph at event {through}"
    );
    Ok(graph)
}

#[test]
fn typed_graph_snapshots_follow_inclusive_ledger_events_without_live_fallback() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("state.sqlite");
    let mut store = StateStore::open(&path)?;
    assert_eq!(
        replay(&store, None)?,
        assert_matches_entity_replays(&store, 0, 0, &[], &[], 0)?
    );
    for (id, trust) in [
        ("z-goal", Trust::L4a),
        ("m-premise", Trust::L3),
        ("a-extra", Trust::L4a),
    ] {
        claim(&mut store, id)?;
        store.set_local_trust(id, trust, change(BASE))?;
    }
    let before_plan =
        store.append_event(change(BASE), "NOTE_CREATED", "note", "z-route", json!({}))?;
    plan(
        &mut store,
        "z-route",
        "z-goal",
        &[("m-premise", "lemma"), ("a-extra", "definition")],
    )?;
    let created = event_seq(&store, before_plan, "z-route", "PLAN_CREATED")?;
    let first_plan_change = event_seq(&store, before_plan, "z-route", "PLAN_TRUST_CHANGED")?;
    let first_goal_change = event_seq(&store, before_plan, "z-goal", "CLAIM_EFFECTIVE_CHANGED")?;
    assert!(created < first_plan_change && first_plan_change < first_goal_change);

    // These plans share an edge. Both retained plan dependency records count.
    plan(
        &mut store,
        "a-fallback",
        "z-goal",
        &[("m-premise", "lemma")],
    )?;
    let before_upgrade = head(&store)?;
    store.set_local_trust("m-premise", Trust::L4a, change(BASE))?;
    let upgraded = event_seq(&store, before_upgrade, "m-premise", "CLAIM_TRUST_CHANGED")?;
    let after_upgrade = head(&store)?;
    store.mark_t3(&["m-premise"], &[], change(TAINT_ROOT))?;
    let tainted = event_seq(&store, after_upgrade, "m-premise", "CLAIM_TAINTED")?;
    let ledger_head = store.append_event(
        change(BASE),
        "CLAIM_REPROOF_REQUESTED",
        "claim",
        "z-goal",
        json!({"advisory": true}),
    )?;
    let ledger = serde_json::to_value(store.events_after(0, 100)?)?;
    let raw = Connection::open(&path)?;
    let data_version =
        || -> rusqlite::Result<i64> { raw.query_row("PRAGMA data_version", [], |r| r.get(0)) };
    let initial_version = data_version()?;

    let all_claims = &["a-extra", "m-premise", "z-goal"];
    let one_plan = &["z-route"];
    let all_plans = &["a-fallback", "z-route"];
    let at = |through, claims: &[&str], plans: &[&str], edges| {
        assert_matches_entity_replays(&store, through, ledger_head, claims, plans, edges)
    };
    at(0, &[], &[], 0)?;
    let before = at(before_plan, all_claims, &[], 0)?;
    assert_eq!(before["applied_claim_events"], 9);
    let creation = at(created, all_claims, one_plan, 2)?;
    assert_eq!(creation["applied_plan_events"], 1);
    let route = row(&creation, "plans", "plan_id", "z-route");
    assert_eq!(route["last_event_seq"], created);
    assert_eq!(route["effective_trust"], "L0");
    assert_eq!(
        route["dependencies"],
        json!([
            {"claim_id": "m-premise", "kind": "lemma"},
            {"claim_id": "a-extra", "kind": "definition"},
        ])
    );
    let first_plan_effective = at(first_plan_change, all_claims, one_plan, 2)?;
    assert_eq!(first_plan_effective["plans"][0]["effective_trust"], "L3");
    assert_eq!(
        row(&first_plan_effective, "claims", "claim_id", "z-goal")["effective_trust"],
        "L4a",
        "a cutoff inside a write transaction must not recompute the target early"
    );
    let first_goal_effective = at(first_goal_change, all_claims, one_plan, 2)?;
    assert_eq!(first_goal_effective["applied_claim_events"], 10);
    assert_eq!(first_goal_effective["claims"][2]["effective_trust"], "L3");

    let at_local_change = at(upgraded, all_claims, all_plans, 3)?;
    let premise = row(&at_local_change, "claims", "claim_id", "m-premise");
    assert_eq!(premise["local_trust"], "L4a");
    assert_eq!(premise["effective_trust"], "L3");
    assert_eq!(at_local_change["plans"][0]["effective_trust"], "L3");
    let after_propagation = at(after_upgrade, all_claims, all_plans, 3)?;
    assert_eq!(after_propagation["claims"][2]["effective_trust"], "L4a");
    assert_eq!(after_propagation["plans"][0]["effective_trust"], "L4a");
    assert_eq!(after_propagation["plans"][1]["effective_trust"], "L4a");
    let taint_prefix = at(tainted, all_claims, all_plans, 3)?;
    let premise = row(&taint_prefix, "claims", "claim_id", "m-premise");
    assert_eq!(premise["taint_root_commit"], TAINT_ROOT);
    assert_eq!(premise["stale"], true);
    assert_eq!(premise["effective_stale"], false);
    let latest = at(ledger_head, all_claims, all_plans, 3)?;
    assert_eq!(latest["claims"][2]["effective_stale"], true);
    assert_eq!(replay(&store, None)?, latest);
    assert_eq!(serde_json::to_value(store.events_after(0, 100)?)?, ledger);
    assert_eq!(data_version()?, initial_version);
    drop(store);

    // Diverge every live graph projection without changing the ledger.
    assert_eq!(
        raw.execute(
            "UPDATE claims SET title='drift',local_trust_level='L1' WHERE claim_id='z-goal'",
            [],
        )?,
        1
    );
    assert_eq!(
        raw.execute("DELETE FROM plan_dependencies WHERE plan_id='z-route'", [])?,
        2
    );
    assert_eq!(
        raw.execute("DELETE FROM proof_plans WHERE plan_id='z-route'", [])?,
        1
    );
    raw.execute(
        "INSERT INTO claims (claim_id,title,informal_statement,engine,local_trust_level,
         effective_trust_level,created_at,updated_at)
         VALUES ('live-only','Live only','No creation event','informal','L0','L0','now','now')",
        [],
    )?;
    let drift_version = data_version()?;
    let reader = StateStore::open_readonly(&path)?;
    assert_eq!(reader.claim("z-goal")?.unwrap().local_trust, Trust::L1);
    assert!(reader.claim("live-only")?.is_some());
    assert!(reader.plan_state("z-route")?.is_none());
    assert_eq!(replay(&reader, None)?, latest);
    assert_eq!(replay(&reader, Some(tainted))?, taint_prefix);
    assert_eq!(serde_json::to_value(reader.events_after(0, 100)?)?, ledger);
    assert_eq!(data_version()?, drift_version);
    Ok(())
}

fn raw_event(db: &Connection, entity: &str, id: &str, kind: &str, payload: &Value) -> Result<i64> {
    db.execute(
        "INSERT INTO events (occurred_at,event_type,entity_type,entity_id,git_commit,actor,payload_json)
         VALUES ('2026-01-01T00:00:00Z',?1,?2,?3,?4,'fixture:graph-replay',?5)",
        params![kind, entity, id, BASE, payload.to_string()],
    )?;
    Ok(db.last_insert_rowid())
}

fn raw_plan(db: &Connection, id: &str, target: &str, deps: &[(&str, &str)]) -> Result<i64> {
    raw_event(
        db,
        "proof_plan",
        id,
        "PLAN_CREATED",
        &json!({"target":target,"name":id,"track":"exploration","status":"ACTIVE","dependencies":deps}),
    )
}

fn assert_rejected(store: &StateStore, through: Option<i64>, reason: &str) {
    let err = store
        .replay_graph(through)
        .err()
        .unwrap_or_else(|| panic!("expected {reason} at {through:?}"));
    assert!(format!("{err:#}").contains(reason), "{err:#}");
}

fn assert_prefix_preserved(store: &StateStore, mut before: Value, new_head: i64) -> Result<()> {
    let prefix = before["through_event_seq"].as_i64().unwrap();
    before["ledger_head_event_seq"] = json!(new_head);
    assert_eq!(replay(store, Some(prefix))?, before, "prefix at {prefix}");
    Ok(())
}

#[test]
fn graph_rejects_missing_targets_and_forward_dependency_references() -> Result<()> {
    for (target, dependency, later_created, reason) in [
        ("missing-target", "known", None, "target Claim"),
        ("known", "later", Some("later"), "dependency Claim"),
    ] {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("state.sqlite");
        let mut store = StateStore::open(&path)?;
        claim(&mut store, "known")?;
        let prefix = replay(&store, None)?;
        let raw = Connection::open(&path)?;
        let invalid = raw_plan(&raw, "dangling", target, &[(dependency, "lemma")])?;
        assert!(store.replay_plan("dangling", Some(invalid))?.plan.is_some());
        if let Some(id) = later_created {
            claim(&mut store, id)?;
            assert!(store.claim(id)?.is_some());
        }
        let ledger_head = head(&store)?;
        drop(store);

        let reader = StateStore::open_readonly(&path)?;
        assert_rejected(&reader, Some(invalid), reason);
        assert_rejected(&reader, None, reason);
        assert_prefix_preserved(&reader, prefix, ledger_head)?;
    }
    Ok(())
}

#[test]
fn graph_rejects_cycles_through_retained_invalidated_routes() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("state.sqlite");
    let mut store = StateStore::open(&path)?;
    for id in ["a", "b", "c"] {
        claim(&mut store, id)?;
    }
    plan(&mut store, "ab", "b", &[("a", "lemma")])?;
    plan(&mut store, "bc", "c", &[("b", "lemma")])?;
    let raw = Connection::open(&path)?;
    // Insert a structurally valid invalidation with its referenced refutation.
    // The typed L2b path requires a sealed native witness, beyond this fixture.
    raw_event(
        &raw,
        "claim",
        "a",
        "CLAIM_TRUST_CHANGED",
        &json!({"previous_local":"L0","local":"L2b","was_stale":false,"detail":{}}),
    )?;
    raw_event(
        &raw,
        "proof_plan",
        "ab",
        "PLAN_INVALIDATED",
        &json!({"refuted_claim":"a","target":"b"}),
    )?;
    let prefix = replay(&store, None)?;
    assert_eq!(prefix["dependency_edge_count"], 2);
    assert_eq!(prefix["plans"][0]["status"], "INVALIDATED_BY_REFUTATION");
    let cycle = raw_plan(&raw, "ca", "a", &[("c", "definition")])?;
    assert!(store.replay_plan("ca", Some(cycle))?.plan.is_some());
    assert_rejected(&store, Some(cycle), "dependency graph contains a cycle");
    assert_rejected(&store, None, "dependency graph contains a cycle");
    assert_prefix_preserved(&store, prefix, cycle)?;
    Ok(())
}

#[test]
fn graph_validates_route_cause_references_even_after_they_are_cleared() -> Result<()> {
    for (entity, id, transition, cleared, cause) in [
        (
            "proof_plan",
            "route",
            "PLAN_ROUTE_BLOCKED",
            "PLAN_ROUTE_UNBLOCKED",
            "missing",
        ),
        (
            "proof_plan",
            "route",
            "PLAN_ROUTE_BLOCKED",
            "PLAN_ROUTE_UNBLOCKED",
            "goal",
        ),
        (
            "claim",
            "goal",
            "CLAIM_ROUTES_EXHAUSTED",
            "CLAIM_ROUTES_REOPENED",
            "missing",
        ),
    ] {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("state.sqlite");
        let mut store = StateStore::open(&path)?;
        claim(&mut store, "goal")?;
        plan(&mut store, "route", "goal", &[])?;
        let prefix = replay(&store, None)?;
        let raw = Connection::open(&path)?;
        let invalid = raw_event(
            &raw,
            entity,
            id,
            transition,
            &json!({"refuted_claim":cause}),
        )?;
        let cleared = raw_event(&raw, entity, id, cleared, &json!({}))?;
        // These are legal per-entity transitions; the invalidity is cross-entity.
        if entity == "claim" {
            assert!(store.replay_claim(id, None)?.claim.is_some());
        } else {
            assert!(store.replay_plan(id, None)?.plan.is_some());
        }
        let reason = if entity == "claim" {
            "route exhaustion cause"
        } else {
            "blocking cause"
        };
        assert_rejected(&store, Some(invalid), reason);
        assert_rejected(&store, None, reason);
        assert_prefix_preserved(&store, prefix, cleared)?;
    }
    Ok(())
}

#[test]
fn graph_rejects_unknown_state_events_and_invalid_cursors() -> Result<()> {
    for (entity, id, kind, reason) in [
        (
            "claim",
            "goal",
            "CLAIM_FUTURE_MUTATION",
            "unsupported Claim state event",
        ),
        (
            "proof_plan",
            "route",
            "PLAN_FUTURE_MUTATION",
            "unsupported ProofPlan state event",
        ),
        (
            "note",
            "route",
            "PLAN_CREATED",
            "expected entity type proof_plan",
        ),
    ] {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("state.sqlite");
        let mut store = StateStore::open(&path)?;
        claim(&mut store, "goal")?;
        plan(&mut store, "route", "goal", &[])?;
        let prefix = replay(&store, None)?;
        let raw = Connection::open(&path)?;
        let unknown = raw_event(&raw, entity, id, kind, &json!({}))?;
        assert_rejected(&store, Some(unknown), reason);
        assert_rejected(&store, None, reason);
        assert_rejected(&store, Some(-1), "outside");
        assert_rejected(&store, Some(unknown + 1), "outside");
        assert_prefix_preserved(&store, prefix, unknown)?;
    }
    Ok(())
}

fn raw_claim(db: &Connection, id: &str) -> Result<i64> {
    raw_event(
        db,
        "claim",
        id,
        "CLAIM_CREATED",
        &json!({"title":id,"engine":"lean4","informal_statement":"True",
                "formal_target":null,"module_path":null,"critical":false}),
    )
}

#[test]
fn graph_source_cap_includes_ignored_global_events() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("state.sqlite");
    let mut store = StateStore::open(&path)?;
    claim(&mut store, "goal")?;
    let raw = Connection::open(&path)?;
    raw.execute(
        "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<32767)
         INSERT INTO events (occurred_at,event_type,entity_type,entity_id,actor,payload_json)
         SELECT '2026-01-01T00:00:00Z','NOTE_CREATED','note','note','graph-fixture','{}' FROM n",
        [],
    )?;
    let at_cap = raw.last_insert_rowid();
    assert_eq!(at_cap, 32768);
    let over_cap = raw_event(&raw, "note", "note", "NOTE_CREATED", &json!({}))?;

    assert_rejected(&store, None, "32768-event cap");
    assert_matches_entity_replays(&store, at_cap, over_cap, &["goal"], &[], 0)?;
    Ok(())
}

#[test]
fn graph_applies_combined_entity_cap_at_the_selected_prefix() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("state.sqlite");
    let store = StateStore::open(&path)?;
    let mut raw = Connection::open(&path)?;
    let tx = raw.transaction()?;
    for i in 0..512 {
        raw_claim(&tx, &format!("claim-{i:04}"))?;
        raw_plan(&tx, &format!("plan-{i:04}"), "claim-0000", &[])?;
    }
    let at_cap = tx.last_insert_rowid();
    let over_cap = raw_claim(&tx, "one-more-claim")?;
    tx.commit()?;

    assert_rejected(&store, None, "1024-node cap");
    let graph = replay(&store, Some(at_cap))?;
    assert_eq!(graph["ledger_head_event_seq"], over_cap);
    assert_eq!(graph["claims"].as_array().unwrap().len(), 512);
    assert_eq!(graph["plans"].as_array().unwrap().len(), 512);
    assert_eq!(graph["dependency_edge_count"], 0);
    Ok(())
}

#[test]
fn graph_caps_retained_dependency_records_including_parallel_edges() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("state.sqlite");
    let store = StateStore::open(&path)?;
    let mut raw = Connection::open(&path)?;
    let tx = raw.transaction()?;
    raw_claim(&tx, "target")?;
    let dep_ids: Vec<_> = (0..128).map(|i| format!("dep-{i:03}")).collect();
    for id in &dep_ids {
        raw_claim(&tx, id)?;
    }
    let deps: Vec<_> = dep_ids.iter().map(|id| (id.as_str(), "lemma")).collect();
    for i in 0..64 {
        raw_plan(&tx, &format!("plan-{i:03}"), "target", &deps)?;
    }
    let at_cap = tx.last_insert_rowid();
    let over_cap = raw_plan(&tx, "one-more-plan", "target", &deps[..1])?;
    tx.commit()?;

    assert_rejected(&store, None, "8192-edge cap");
    let graph = replay(&store, Some(at_cap))?;
    assert_eq!(graph["ledger_head_event_seq"], over_cap);
    assert_eq!(graph["dependency_edge_count"], 8192);
    assert_eq!(graph["applied_plan_events"], 64);
    Ok(())
}
