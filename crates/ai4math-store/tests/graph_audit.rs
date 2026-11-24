use ai4math_store::{Change, NewClaim, NewPlan, PlanStatus, StateStore, Trust};
use anyhow::Result;
use rusqlite::Connection;
use serde_json::{Value, json};

const BASE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const TAINT_ROOT: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const DRIFT: &str = "field_mismatch";
const MISSING: &str = "missing_projection";
const EXTRA: &str = "unexpected_projection";

fn change(commit: &str) -> Change<'_> {
    Change {
        actor: "graph-audit-fixture",
        git_commit: Some(commit),
    }
}

fn claim(store: &mut StateStore, id: &str) -> Result<()> {
    store.create_claim(
        NewClaim {
            id,
            title: "Private retained title",
            informal_statement: "Private retained statement: α.",
            engine: "lean4",
            formal_target: Some("Private/Retained.lean#goal"),
            module_path: Some("Private/Retained.lean"),
            critical: id == "goal",
        },
        change(BASE),
    )
}

fn plan(store: &mut StateStore, id: &str, target: &str, deps: &[(&str, &str)]) -> Result<()> {
    store.create_plan(
        NewPlan {
            id,
            name: "Private retained plan name",
            target,
            status: PlanStatus::Active,
            track: "exploration",
            dependencies: deps,
        },
        change(BASE),
    )
}

fn report(store: &StateStore) -> Result<Value> {
    Ok(serde_json::to_value(store.audit_graph()?)?)
}

fn ledger(store: &StateStore) -> Result<Value> {
    Ok(serde_json::to_value(store.events_after(0, 100)?)?)
}

fn data_version(raw: &Connection) -> rusqlite::Result<i64> {
    raw.query_row("PRAGMA data_version", [], |row| row.get(0))
}

fn mismatch(entity_type: &str, entity_id: &str, field: &str, kind: &str) -> Value {
    json!({"entity_type": entity_type, "entity_id": entity_id, "field": field, "kind": kind})
}

fn assert_report(report: &Value, head: i64, expected: &[(&str, &str, &str, &str)]) {
    let rows: Vec<_> = expected
        .iter()
        .map(|&(ty, id, field, kind)| mismatch(ty, id, field, kind))
        .collect();
    // Compare whole objects: neither side's metadata, trust values, dependency IDs,
    // route cause IDs, nor timestamp/receipt fields may be attached to a mismatch.
    assert_eq!(
        *report,
        json!({
            "ledger_event_seq": head,
            "consistent": rows.is_empty(),
            "mismatch_count": rows.len(),
            "truncated": false,
            "mismatches": rows,
        })
    );
}

#[test]
fn typed_graph_audits_clean_and_sql_drift_is_reported_without_repair() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("state.sqlite");
    let mut store = StateStore::open(&path)?;
    assert_report(&report(&store)?, 0, &[]);

    for (id, trust) in [
        ("z-premise", Trust::L3),
        ("goal", Trust::L4a),
        ("a-support", Trust::L4a),
        ("spare", Trust::L0),
        ("a-removed-claim", Trust::L0),
    ] {
        claim(&mut store, id)?;
        store.set_local_trust(id, trust, change(BASE))?;
    }
    // Dependency creation order differs from SQL key order.
    plan(
        &mut store,
        "z-route",
        "goal",
        &[("z-premise", "lemma"), ("a-support", "definition")],
    )?;
    plan(&mut store, "gone-plan", "spare", &[])?;
    plan(
        &mut store,
        "dep-remove",
        "goal",
        &[("a-support", "definition")],
    )?;
    plan(
        &mut store,
        "dep-add",
        "goal",
        &[("a-support", "definition")],
    )?;
    store.mark_t3(&["z-premise"], &[], change(TAINT_ROOT))?;
    let head = store.append_event(change(BASE), "NOTE_CREATED", "note", "note", json!({}))?;
    let original_ledger = ledger(&store)?;
    let raw = Connection::open(&path)?;
    let reader = StateStore::open_readonly(&path)?;
    let version = data_version(&raw)?;
    assert_report(&report(&store)?, head, &[]);
    assert_report(&report(&reader)?, head, &[]);
    assert_eq!(ledger(&reader)?, original_ledger);
    assert_eq!(data_version(&raw)?, version);

    raw.execute_batch(
        "BEGIN;
         UPDATE claims SET title='Private live title', informal_statement='Private live statement',
            engine='rocq', formal_target=NULL, module_path='Private/Live.v', is_critical_path=0,
            local_trust_level='L1', effective_trust_level='L1' WHERE claim_id='goal';
         UPDATE claims SET stale=0, effective_stale=0, prior_trust_level='L2a',
            taint_root_commit='Private live commit' WHERE claim_id='z-premise';
         UPDATE claims SET updated_at='ignored timestamp', formal_signature='unreplayed metadata',
            oracle_debt=1 WHERE claim_id='a-support';
         DELETE FROM claims WHERE claim_id='a-removed-claim';
         INSERT INTO claims (claim_id,title,informal_statement,engine,local_trust_level,
             effective_trust_level,created_at,updated_at)
            VALUES ('live-only','Private extra title','Private extra statement','informal','L0','L0','now','now');
         UPDATE proof_plans SET target_claim_id='spare', name='Private live plan name',
            track='infrastructure', status='PROVED', effective_trust_level='L1' WHERE plan_id='z-route';
         DELETE FROM proof_plans WHERE plan_id='gone-plan';
         INSERT INTO proof_plans (plan_id,target_claim_id,name,track,status,effective_trust_level,created_at,updated_at)
            VALUES ('a-live-plan','goal','Private extra plan','exploration','ACTIVE','L0','now','now');
         UPDATE plan_dependencies SET dep_kind='oracle_placeholder' WHERE plan_id='z-route' AND dep_claim_id='z-premise';
         DELETE FROM plan_dependencies WHERE plan_id='dep-remove' AND dep_claim_id='a-support';
         INSERT INTO plan_dependencies VALUES ('dep-add','spare','lemma');
         INSERT INTO proof_plan_route_blocks VALUES ('z-route','z-premise');
         INSERT INTO claim_route_exhaustion VALUES ('goal','z-premise');
         COMMIT;",
    )?;
    let drifted_version = data_version(&raw)?;
    let first = report(&reader)?;
    assert_report(
        &first,
        head,
        &[
            ("claim", "a-removed-claim", "presence", MISSING),
            ("claim", "goal", "critical", DRIFT),
            ("claim", "goal", "effective_trust", DRIFT),
            ("claim", "goal", "engine", DRIFT),
            ("claim", "goal", "formal_target", DRIFT),
            ("claim", "goal", "informal_statement", DRIFT),
            ("claim", "goal", "local_trust", DRIFT),
            ("claim", "goal", "module_path", DRIFT),
            ("claim", "goal", "routes_exhausted", DRIFT),
            ("claim", "goal", "title", DRIFT),
            ("claim", "live-only", "presence", EXTRA),
            ("claim", "z-premise", "effective_stale", DRIFT),
            ("claim", "z-premise", "prior_trust", DRIFT),
            ("claim", "z-premise", "stale", DRIFT),
            ("claim", "z-premise", "taint_root_commit", DRIFT),
            ("proof_plan", "a-live-plan", "presence", EXTRA),
            ("proof_plan", "dep-add", "dependencies", DRIFT),
            ("proof_plan", "dep-remove", "dependencies", DRIFT),
            ("proof_plan", "gone-plan", "presence", MISSING),
            ("proof_plan", "z-route", "blocking_claim_id", DRIFT),
            ("proof_plan", "z-route", "dependencies", DRIFT),
            ("proof_plan", "z-route", "durable_status", DRIFT),
            ("proof_plan", "z-route", "effective_trust", DRIFT),
            ("proof_plan", "z-route", "name", DRIFT),
            ("proof_plan", "z-route", "target_claim_id", DRIFT),
            ("proof_plan", "z-route", "track", DRIFT),
        ],
    );
    // Auditing on a writable handle is read-only too. The projection stays
    // drifted and the entire event ledger (not just its latest sequence) is stable.
    assert_eq!(report(&store)?, first);
    assert_eq!(report(&reader)?, first);
    assert_eq!(reader.claim("goal")?.unwrap().local_trust, Trust::L1);
    assert_eq!(
        reader.plan_state("z-route")?.unwrap().0,
        "BLOCKED_BY_REFUTATION"
    );
    assert!(reader.plan_state("gone-plan")?.is_none());
    assert_eq!(ledger(&reader)?, original_ledger);
    assert_eq!(data_version(&raw)?, drifted_version);
    Ok(())
}

#[test]
fn retained_route_blocks_can_be_clean_and_are_compared_by_blocking_cause() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("state.sqlite");
    let mut store = StateStore::open(&path)?;
    for id in ["goal", "middle", "root", "alternative"] {
        claim(&mut store, id)?;
    }
    let raw = Connection::open(&path)?;
    // Seed one matching terminal Claim projection and history without needing a
    // native Lean witness. The actual plan invalidation, blocking and exhaustion
    // events and their projections below are produced by create_plan transactions.
    raw.execute_batch(
        r#"BEGIN;
        UPDATE claims SET local_trust_level='L2b',effective_trust_level='L2b' WHERE claim_id='root';
        INSERT INTO events (occurred_at,event_type,entity_type,entity_id,actor,payload_json)
        VALUES ('now','CLAIM_TRUST_CHANGED','claim','root','graph-audit-fixture',
                '{"previous_local":"L0","local":"L2b","was_stale":false,"detail":{}}'),
               ('now','CLAIM_EFFECTIVE_CHANGED','claim','root','graph-audit-fixture',
                '{"previous":"L0","effective":"L2b","effective_stale":false}');
        COMMIT;"#,
    )?;
    plan(&mut store, "direct", "middle", &[("root", "lemma")])?;
    plan(&mut store, "inherited", "goal", &[("middle", "lemma")])?;
    let head = store.status()?.last_event_seq;
    assert_eq!(
        store.plan_state("direct")?.unwrap().0,
        "INVALIDATED_BY_REFUTATION"
    );
    assert_eq!(
        store.plan_state("inherited")?.unwrap().0,
        "BLOCKED_BY_REFUTATION"
    );
    let reader = StateStore::open_readonly(&path)?;
    assert_report(&report(&reader)?, head, &[]);

    // Keep the block present but replace its cause, and remove a Claim's
    // exhaustion overlay. Claim comparison is presence-only, unlike a plan block.
    raw.execute_batch(
        "UPDATE proof_plan_route_blocks SET refuted_claim_id='alternative' WHERE plan_id='inherited';
         UPDATE claim_route_exhaustion SET refuted_claim_id='alternative' WHERE claim_id='middle';
         DELETE FROM claim_route_exhaustion WHERE claim_id='goal';",
    )?;
    assert_report(
        &report(&reader)?,
        head,
        &[
            ("claim", "goal", "routes_exhausted", DRIFT),
            ("proof_plan", "inherited", "blocking_claim_id", DRIFT),
        ],
    );
    Ok(())
}

#[test]
fn incomplete_legacy_creation_fails_closed_even_with_live_row() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("state.sqlite");
    let mut store = StateStore::open(&path)?;
    let legacy = store.append_event(
        change(BASE),
        "CLAIM_CREATED",
        "claim",
        "legacy",
        json!({"title":"Legacy title","engine":"informal","formal_target":null,
               "module_path":null,"critical":false}),
    )?;
    claim(&mut store, "recent")?;
    let raw = Connection::open(&path)?;
    raw.execute(
        "INSERT INTO claims (claim_id,title,informal_statement,engine,local_trust_level,
             effective_trust_level,created_at,updated_at)
         VALUES ('legacy','Legacy title','Only in the live row','informal','L0','L0','now','now')",
        [],
    )?;
    let head = store.status()?.last_event_seq;
    assert!(head > legacy);
    let reader = StateStore::open_readonly(&path)?;
    let before = ledger(&reader)?;
    let version = data_version(&raw)?;
    let err = reader
        .audit_graph()
        .err()
        .expect("legacy replay must fail closed");
    assert!(format!("{err:#}").contains("informal_statement"), "{err:#}");
    assert!(reader.claim("legacy")?.is_some());
    assert_eq!(reader.status()?.last_event_seq, head);
    assert_eq!(ledger(&reader)?, before);
    assert_eq!(data_version(&raw)?, version);
    Ok(())
}

fn extra_claims(raw: &Connection, count: usize) -> Result<()> {
    raw.execute(
        "WITH RECURSIVE n(i) AS (SELECT ?1 - 1 UNION ALL SELECT i-1 FROM n WHERE i>0)
         INSERT INTO claims (claim_id,title,informal_statement,engine,local_trust_level,
             effective_trust_level,created_at,updated_at)
         SELECT printf('extra-%04d', i), 'Hidden title', 'Hidden statement', 'informal',
             'L0', 'L0', 'now', 'now' FROM n",
        [count],
    )?;
    Ok(())
}

#[test]
fn mismatch_report_keeps_total_and_a_sorted_bounded_prefix() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("state.sqlite");
    let store = StateStore::open(&path)?;
    let raw = Connection::open(&path)?;
    let reader = StateStore::open_readonly(&path)?;
    // Insert 130 extra rows in reverse ID order without consuming the ledger cap.
    extra_claims(&raw, 130)?;
    let first = report(&reader)?;
    let prefix: Vec<_> = (0..128)
        .map(|id| mismatch("claim", &format!("extra-{id:04}"), "presence", EXTRA))
        .collect();
    assert_eq!(
        first,
        json!({
            "ledger_event_seq": 0,
            "consistent": false,
            "mismatch_count": 130,
            "truncated": true,
            "mismatches": prefix,
        })
    );
    assert_eq!(report(&store)?, first);
    Ok(())
}

#[test]
fn oversized_live_projection_fails_closed_instead_of_certifying_a_prefix() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("state.sqlite");
    let _store = StateStore::open(&path)?;
    let raw = Connection::open(&path)?;
    let reader = StateStore::open_readonly(&path)?;
    extra_claims(&raw, 1023)?;
    let insert_plan = "INSERT INTO proof_plans (plan_id,target_claim_id,name,track,status,
        effective_trust_level,created_at,updated_at)
        VALUES (?1,'extra-0000','Hidden plan','exploration','ACTIVE','L0','now','now')";
    raw.execute(insert_plan, ["extra-plan-a"])?;
    assert_eq!(report(&reader)?["mismatch_count"], 1024);

    // The live cap is shared by Claims and ProofPlans and is independent of the
    // empty ledger and the bounded number of mismatches serialized above.
    raw.execute(insert_plan, ["extra-plan-b"])?;
    let version = data_version(&raw)?;
    let err = reader
        .audit_graph()
        .err()
        .expect("above the live node cap must not return a report");
    assert!(format!("{err:#}").contains("node cap"), "{err:#}");
    assert_eq!(reader.status()?.last_event_seq, 0);
    assert_eq!(data_version(&raw)?, version);
    Ok(())
}
