use ai4math_store::{Change, NewClaim, NewPlan, PlanStatus, StateStore, Trust};
use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use serde_json::{Value, json};

const BASE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const FIRST_ROOT: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const NEXT_ROOT: &str = "cccccccccccccccccccccccccccccccccccccccc";

fn change(commit: &str) -> Change<'_> {
    Change {
        actor: "claim-replay-fixture",
        git_commit: Some(commit),
    }
}

fn create(store: &mut StateStore, id: &str, critical: bool) -> Result<()> {
    store.create_claim(
        NewClaim {
            id,
            title: "Replay: α",
            informal_statement: "First premise.\nSecond premise.",
            engine: "lean4",
            formal_target: critical.then_some("Math.lean#result"),
            module_path: critical.then_some("Math.lean"),
            critical,
        },
        change(BASE),
    )
}

fn plan(store: &mut StateStore, id: &str, target: &str, dependency: &str) -> Result<()> {
    store.create_plan(
        NewPlan {
            id,
            name: id,
            target,
            status: PlanStatus::Active,
            track: "exploration",
            dependencies: &[(dependency, "lemma")],
        },
        change(BASE),
    )
}

fn head(store: &StateStore) -> Result<i64> {
    Ok(store.status()?.last_event_seq)
}

fn claim_event_seq(store: &StateStore, after: i64, id: &str, kind: &str) -> Result<i64> {
    let events = store.events_after(after, 100)?;
    let mut matches = events.iter().filter(|event| {
        event.entity_type == "claim" && event.entity_id == id && event.event_type == kind
    });
    let seq = matches
        .next()
        .with_context(|| format!("missing {id} {kind}"))?
        .event_seq;
    assert!(matches.next().is_none(), "duplicate {id} {kind}");
    Ok(seq)
}

fn replay(store: &StateStore, id: &str, through: Option<i64>) -> Result<Value> {
    Ok(serde_json::to_value(store.replay_claim(id, through)?)?)
}

fn assert_snapshot(
    store: &StateStore,
    id: &str,
    through: i64,
    head: i64,
    applied: u32,
    claim: &Value,
) -> Result<()> {
    assert_eq!(
        replay(store, id, Some(through))?,
        json!({
            "claim_id": id,
            "through_event_seq": through,
            "ledger_head_event_seq": head,
            "applied_events": applied,
            "claim": claim,
        }),
        "{id} at event {through}"
    );
    Ok(())
}

fn assert_current_projection(
    store: &StateStore,
    db: &Connection,
    id: &str,
    replayed: &Value,
) -> Result<()> {
    let row = store.claim(id)?.context("missing current claim")?;
    // Public ClaimState omits metadata and route exhaustion. Check those against
    // their live tables too, not against another replay of the same ledger.
    let (title, statement, routes_exhausted): (String, String, bool) = db.query_row(
        "SELECT title,informal_statement,
         EXISTS(SELECT 1 FROM claim_route_exhaustion r WHERE r.claim_id=c.claim_id)
         FROM claims c WHERE c.claim_id=?1",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    assert_eq!(
        *replayed,
        json!({
            "claim_id": row.id,
            "title": title,
            "engine": row.engine,
            "informal_statement": statement,
            "formal_target": row.formal_target,
            "module_path": row.module_path,
            "critical": row.critical,
            "local_trust": row.local_trust,
            "effective_trust": row.effective_trust,
            "stale": row.stale,
            "effective_stale": row.effective_stale,
            "prior_trust": row.prior_trust,
            "taint_root_commit": row.taint_root_commit,
            "routes_exhausted": routes_exhausted,
            "last_event_seq": replayed["last_event_seq"],
        })
    );
    Ok(())
}

#[test]
fn replays_inclusive_creation_trust_and_taint_cutoffs_without_writes() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("state.sqlite");
    let mut store = StateStore::open(&path)?;
    let id = "result";
    assert_snapshot(&store, id, 0, 0, 0, &Value::Null)?;
    assert_eq!(replay(&store, id, None)?, replay(&store, id, Some(0))?);

    let before_create = store.append_event(change(BASE), "NOTE_CREATED", "note", id, json!({}))?;
    create(&mut store, id, true)?;
    let created = head(&store)?;
    store.set_local_trust(id, Trust::L4a, change(BASE))?;
    let local_change = claim_event_seq(&store, created, id, "CLAIM_TRUST_CHANGED")?;
    let trusted = head(&store)?;
    assert_eq!(
        trusted,
        claim_event_seq(&store, created, id, "CLAIM_EFFECTIVE_CHANGED")?
    );

    store.mark_t3(&[id], &[], change(FIRST_ROOT))?;
    let tainted = claim_event_seq(&store, trusted, id, "CLAIM_TAINTED")?;
    let effective_taint = head(&store)?;
    assert_eq!(
        effective_taint,
        claim_event_seq(&store, trusted, id, "CLAIM_EFFECTIVE_CHANGED")?
    );
    store.mark_t3(&[id], &[], change(NEXT_ROOT))?;
    let repeated_taint = claim_event_seq(&store, effective_taint, id, "CLAIM_TAINTED")?;
    let ledger_head = store.append_event(
        change(NEXT_ROOT),
        "CLAIM_REPROOF_REQUESTED",
        "claim",
        id,
        json!({"diagnostics": "advisory only"}),
    )?;
    let initial_ledger = serde_json::to_value(store.events_after(0, 100)?)?;

    assert_snapshot(&store, id, 0, ledger_head, 0, &Value::Null)?;
    assert_snapshot(&store, id, before_create, ledger_head, 0, &Value::Null)?;
    assert_snapshot(&store, "unknown", ledger_head, ledger_head, 0, &Value::Null)?;
    let mut expected = json!({
        "claim_id": id,
        "title": "Replay: α",
        "engine": "lean4",
        "informal_statement": "First premise.\nSecond premise.",
        "formal_target": "Math.lean#result",
        "module_path": "Math.lean",
        "critical": true,
        "local_trust": "L0",
        "effective_trust": "L0",
        "stale": false,
        "effective_stale": false,
        "prior_trust": null,
        "taint_root_commit": null,
        "routes_exhausted": false,
        "last_event_seq": created,
    });
    assert_snapshot(&store, id, created, ledger_head, 1, &expected)?;
    expected["local_trust"] = json!("L4a");
    expected["last_event_seq"] = json!(local_change);
    assert_snapshot(&store, id, local_change, ledger_head, 2, &expected)?;
    expected["effective_trust"] = json!("L4a");
    expected["last_event_seq"] = json!(trusted);
    assert_snapshot(&store, id, trusted, ledger_head, 3, &expected)?;
    expected["stale"] = json!(true);
    expected["prior_trust"] = json!("L4a");
    expected["taint_root_commit"] = json!(FIRST_ROOT);
    expected["last_event_seq"] = json!(tainted);
    assert_snapshot(&store, id, tainted, ledger_head, 4, &expected)?;
    expected["effective_stale"] = json!(true);
    expected["last_event_seq"] = json!(effective_taint);
    assert_snapshot(&store, id, effective_taint, ledger_head, 5, &expected)?;
    expected["taint_root_commit"] = json!(NEXT_ROOT);
    expected["last_event_seq"] = json!(repeated_taint);
    assert_snapshot(&store, id, repeated_taint, ledger_head, 6, &expected)?;
    assert_snapshot(&store, id, ledger_head, ledger_head, 6, &expected)?;

    let latest = replay(&store, id, None)?;
    assert_eq!(latest, replay(&store, id, Some(ledger_head))?);
    assert_eq!(
        serde_json::to_value(store.events_after(0, 100)?)?,
        initial_ledger
    );
    drop(store);

    let reopened = StateStore::open(&path)?;
    let reader = StateStore::open_readonly(&path)?;
    let raw = Connection::open(&path)?;
    assert_eq!(replay(&reopened, id, None)?, latest);
    assert_eq!(replay(&reader, id, None)?, latest);
    assert_current_projection(&reopened, &raw, id, &latest["claim"])?;
    assert!(
        raw.execute(
            "UPDATE events SET actor='rewritten' WHERE event_seq=?1",
            [created]
        )
        .is_err()
    );
    assert!(
        raw.execute("DELETE FROM events WHERE event_seq=?1", [created])
            .is_err()
    );
    assert_eq!(
        serde_json::to_value(reader.events_after(0, 100)?)?,
        initial_ledger
    );

    // Simulate a divergent live projection. Replay must still come from the
    // ledger and must not attempt to repair the projection as a side effect.
    raw.execute(
        "UPDATE claims SET title='drift', local_trust_level='L1' WHERE claim_id=?1",
        [id],
    )?;
    assert_eq!(replay(&reopened, id, None)?, latest);
    assert_eq!(reopened.claim(id)?.unwrap().local_trust, Trust::L1);
    assert_eq!(
        serde_json::to_value(reader.events_after(0, 100)?)?,
        initial_ledger
    );
    Ok(())
}

#[test]
fn replays_route_exhaustion_and_dependency_state_after_reopen() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("state.sqlite");
    let mut store = StateStore::open(&path)?;
    for id in ["goal", "refuted", "backup"] {
        create(&mut store, id, id == "goal")?;
    }
    store.set_local_trust("goal", Trust::L4a, change(BASE))?;
    store.set_local_trust("backup", Trust::L3, change(BASE))?;
    let raw = Connection::open(&path)?;
    // Seed an already-refuted prerequisite (as in an older database), without
    // requiring a native Lean witness. All state/events of "goal" below are
    // generated by real plan transactions.
    raw.execute(
        "UPDATE claims SET local_trust_level='L2b', effective_trust_level='L2b'
         WHERE claim_id='refuted'",
        [],
    )?;
    let before_exhausted = head(&store)?;
    plan(&mut store, "only-route", "goal", "refuted")?;
    let exhausted_head = head(&store)?;
    let exhausted_seq =
        claim_event_seq(&store, before_exhausted, "goal", "CLAIM_ROUTES_EXHAUSTED")?;
    let at_exhaustion = replay(&store, "goal", Some(exhausted_seq))?;
    assert_eq!(at_exhaustion["claim"]["routes_exhausted"], true);
    assert_eq!(at_exhaustion["claim"]["effective_trust"], "L4a");
    let exhausted = replay(&store, "goal", None)?;
    assert_eq!(exhausted["applied_events"], 5);
    assert_eq!(exhausted["claim"]["last_event_seq"], exhausted_head);
    assert_eq!(exhausted["claim"]["local_trust"], "L4a");
    assert_eq!(exhausted["claim"]["effective_trust"], "L0");
    assert_eq!(exhausted["claim"]["routes_exhausted"], true);
    assert_current_projection(&store, &raw, "goal", &exhausted["claim"])?;
    drop(store);

    let mut store = StateStore::open(&path)?;
    assert_eq!(replay(&store, "goal", None)?, exhausted);
    // Stale this prerequisite before connecting it, so goal inherits effective
    // staleness and lower trust without receiving a local taint event.
    store.mark_t3(&["backup"], &[], change(FIRST_ROOT))?;
    let before_alternative = head(&store)?;
    plan(&mut store, "alternative", "goal", "backup")?;
    let reopened_seq =
        claim_event_seq(&store, before_alternative, "goal", "CLAIM_ROUTES_REOPENED")?;
    let at_reopen = replay(&store, "goal", Some(reopened_seq))?;
    assert_eq!(at_reopen["claim"]["routes_exhausted"], false);
    assert_eq!(at_reopen["claim"]["effective_trust"], "L0");
    let current = replay(&store, "goal", None)?;
    assert_eq!(current["applied_events"], 7);
    assert_eq!(
        current["claim"]["last_event_seq"],
        claim_event_seq(
            &store,
            before_alternative,
            "goal",
            "CLAIM_EFFECTIVE_CHANGED"
        )?
    );
    assert_eq!(current["claim"]["local_trust"], "L4a");
    assert_eq!(current["claim"]["effective_trust"], "L3");
    assert_eq!(current["claim"]["stale"], false);
    assert_eq!(current["claim"]["effective_stale"], true);
    assert_eq!(current["claim"]["routes_exhausted"], false);
    assert_current_projection(&store, &raw, "goal", &current["claim"])?;
    let backup = replay(&store, "backup", None)?;
    assert_eq!(backup["claim"]["formal_target"], Value::Null);
    assert_eq!(backup["claim"]["module_path"], Value::Null);
    assert_snapshot(
        &store,
        "goal",
        exhausted_head,
        head(&store)?,
        5,
        &exhausted["claim"],
    )?;
    Ok(())
}

fn raw_claim_event(db: &Connection, id: &str, kind: &str, payload: Value) -> Result<i64> {
    db.execute(
        "INSERT INTO events (occurred_at,event_type,entity_type,entity_id,git_commit,actor,payload_json)
         VALUES ('2026-01-01T00:00:00Z',?1,'claim',?2,?3,'fixture:malformed-ledger',?4)",
        params![kind, id, BASE, payload.to_string()],
    )?;
    Ok(db.last_insert_rowid())
}

#[test]
fn rejects_invalid_cursors_and_claim_transitions_without_poisoning_valid_prefixes() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("state.sqlite");
    let mut store = StateStore::open(&path)?;
    create(&mut store, "bad-transition", false)?;
    create(&mut store, "unsupported", false)?;
    create(&mut store, "valid", false)?;
    let prefix = head(&store)?;
    let raw = Connection::open(&path)?;

    // A valid-looking transition with no creation must not invent a Claim.
    let orphan = raw_claim_event(
        &raw,
        "orphan",
        "CLAIM_TRUST_CHANGED",
        json!({
            "previous_local": "L0", "local": "L3", "was_stale": false, "detail": {},
        }),
    )?;
    assert!(store.replay_claim("orphan", Some(orphan)).is_err());
    let invalid = raw_claim_event(
        &raw,
        "bad-transition",
        "CLAIM_TRUST_CHANGED",
        json!({
            "previous_local": "L4a", "local": "L3", "was_stale": false, "detail": {},
        }),
    )?;
    assert!(store.replay_claim("bad-transition", Some(invalid)).is_err());
    let unknown = raw_claim_event(&raw, "unsupported", "CLAIM_FUTURE_MUTATION", json!({}))?;
    assert!(store.replay_claim("unsupported", Some(unknown)).is_err());
    assert!(store.replay_claim("unsupported", None).is_err());
    assert_eq!(replay(&store, "valid", None)?["applied_events"], 1);
    assert!(store.replay_claim("valid", Some(-1)).is_err());
    assert!(store.replay_claim("valid", Some(unknown + 1)).is_err());

    for id in ["bad-transition", "unsupported"] {
        let early = replay(&store, id, Some(prefix))?;
        assert_eq!(early["ledger_head_event_seq"], unknown);
        assert_eq!(early["applied_events"], 1);
        assert_eq!(early["claim"]["local_trust"], "L0");
    }
    // The bad transition of a different Claim does not affect a valid prefix.
    assert_eq!(
        replay(&store, "unsupported", Some(invalid))?["applied_events"],
        1
    );
    assert_snapshot(&store, "orphan", prefix, unknown, 0, &Value::Null)?;
    assert_eq!(head(&store)?, unknown);
    Ok(())
}
