use ai4math_store::{Change, NewClaim, NewPlan, PlanStatus, StateStore, Trust};
use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use serde_json::{Value, json};

const BASE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const TRACK: &str = "exploration";

fn change() -> Change<'static> {
    Change {
        actor: "plan-replay-fixture",
        git_commit: Some(BASE),
    }
}

fn claim(store: &mut StateStore, id: &str) -> Result<()> {
    store.create_claim(
        NewClaim {
            id,
            title: id,
            informal_statement: "A fixture premise.",
            engine: "lean4",
            formal_target: None,
            module_path: None,
            critical: false,
        },
        change(),
    )
}

fn plan(
    store: &mut StateStore,
    id: &str,
    target: &str,
    name: &str,
    status: PlanStatus,
    dependencies: &[(&str, &str)],
) -> Result<()> {
    store.create_plan(
        NewPlan {
            id,
            name,
            target,
            status,
            track: TRACK,
            dependencies,
        },
        change(),
    )
}

fn head(store: &StateStore) -> Result<i64> {
    Ok(store.status()?.last_event_seq)
}

fn plan_event(store: &StateStore, after: i64, id: &str, kind: &str) -> Result<i64> {
    let events = store.events_after(after, 100)?;
    let mut matches = events.iter().filter(|event| {
        event.entity_type == "proof_plan" && event.entity_id == id && event.event_type == kind
    });
    let seq = matches
        .next()
        .with_context(|| format!("missing {id} {kind} after {after}"))?
        .event_seq;
    assert!(
        matches.next().is_none(),
        "duplicate {id} {kind} after {after}"
    );
    Ok(seq)
}

fn replay(store: &StateStore, id: &str, through: Option<i64>) -> Result<Value> {
    Ok(serde_json::to_value(store.replay_plan(id, through)?)?)
}

fn assert_snapshot(
    store: &StateStore,
    id: &str,
    through: i64,
    ledger_head: i64,
    applied: u32,
    plan: &Value,
) -> Result<()> {
    assert_eq!(
        replay(store, id, Some(through))?,
        json!({
            "plan_id": id,
            "through_event_seq": through,
            "ledger_head_event_seq": ledger_head,
            "applied_events": applied,
            "plan": plan,
        }),
        "{id} at event {through}"
    );
    Ok(())
}

#[test]
fn replay_uses_inclusive_plan_event_cutoffs_and_not_the_live_projection() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("state.sqlite");
    let mut store = StateStore::open(&path)?;
    let id = "main";
    assert_snapshot(&store, id, 0, 0, 0, &Value::Null)?;
    assert_eq!(replay(&store, id, None)?, replay(&store, id, Some(0))?);

    for claim_id in ["goal", "a", "z"] {
        claim(&mut store, claim_id)?;
        store.set_local_trust(claim_id, Trust::L4a, change())?;
    }
    store.set_local_trust("a", Trust::L3, change())?;
    let before_create = store.append_event(
        change(),
        "PLAN_FUTURE_MUTATION",
        "note", // A colliding ID and event prefix on another entity are irrelevant.
        id,
        json!({}),
    )?;
    plan(
        &mut store,
        id,
        "goal",
        "A route: α",
        PlanStatus::Proved,
        &[("z", "definition"), ("a", "lemma")],
    )?;
    let created = plan_event(&store, before_create, id, "PLAN_CREATED")?;
    let first_trust = plan_event(&store, before_create, id, "PLAN_TRUST_CHANGED")?;
    assert!(created < first_trust);
    let before_upgrade = head(&store)?;
    store.set_local_trust("a", Trust::L4a, change())?;
    let second_trust = plan_event(&store, before_upgrade, id, "PLAN_TRUST_CHANGED")?;
    let ledger_head = store.append_event(change(), "NOTE_CREATED", "proof_plan", id, json!({}))?;
    assert!(second_trust < ledger_head);
    let raw = Connection::open(&path)?;
    let data_version =
        || -> rusqlite::Result<i64> { raw.query_row("PRAGMA data_version", [], |r| r.get(0)) };
    let version_before_reads = data_version()?;
    let ledger = serde_json::to_value(store.events_after(0, 100)?)?;

    assert_snapshot(&store, id, before_create, ledger_head, 0, &Value::Null)?;
    assert_snapshot(&store, "missing", ledger_head, ledger_head, 0, &Value::Null)?;
    let mut expected = json!({
        "plan_id": id,
        "target_claim_id": "goal",
        "name": "A route: α",
        "track": "exploration",
        "dependencies": [
            {"claim_id": "z", "kind": "definition"},
            {"claim_id": "a", "kind": "lemma"},
        ],
        "durable_status": "PROVED",
        "status": "PROVED",
        "effective_trust": "L0",
        "blocking_claim_id": null,
        "refuted_claim_id": null,
        "last_event_seq": created,
    });
    assert_snapshot(&store, id, created, ledger_head, 1, &expected)?;
    expected["effective_trust"] = json!("L3");
    expected["last_event_seq"] = json!(first_trust);
    assert_snapshot(&store, id, first_trust, ledger_head, 2, &expected)?;
    expected["effective_trust"] = json!("L4a");
    expected["last_event_seq"] = json!(second_trust);
    assert_snapshot(&store, id, second_trust, ledger_head, 3, &expected)?;
    assert_snapshot(&store, id, ledger_head, ledger_head, 3, &expected)?;
    let latest = replay(&store, id, None)?;
    assert_eq!(latest, replay(&store, id, Some(ledger_head))?);
    assert_eq!(store.plan_state(id)?, Some(("PROVED".into(), Trust::L4a)));
    assert_eq!(data_version()?, version_before_reads);
    drop(store);

    // Simulate live projection drift without touching the ledger. Reopening in
    // read-only mode and replaying must neither consult nor repair that drift.
    assert_eq!(
        raw.execute(
            "UPDATE proof_plans SET name='drift',effective_trust_level='L1' WHERE plan_id=?1",
            [id],
        )?,
        1
    );
    assert_eq!(
        raw.execute("DELETE FROM plan_dependencies WHERE plan_id=?1", [id])?,
        2
    );
    let version_before_reopen = data_version()?;
    let reader = StateStore::open_readonly(&path)?;
    assert_eq!(replay(&reader, id, None)?, latest);
    assert_eq!(reader.plan_state(id)?, Some(("PROVED".into(), Trust::L1)));
    assert_eq!(serde_json::to_value(reader.events_after(0, 100)?)?, ledger);
    assert_eq!(data_version()?, version_before_reopen);
    Ok(())
}

#[test]
fn replay_tracks_inherited_block_changes_reopening_and_durable_invalidation() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("state.sqlite");
    let mut store = StateStore::open(&path)?;
    for (id, trust) in [
        ("goal", Trust::L4a),
        ("middle-z", Trust::L3),
        ("middle-a", Trust::L4a),
        ("z-refuted", Trust::L0),
        ("a-refuted", Trust::L0),
        ("backup", Trust::L4a),
    ] {
        claim(&mut store, id)?;
        store.set_local_trust(id, trust, change())?;
    }
    // Seed *prerequisite claims only*, as though hard refutations had been
    // retained previously. Producing a sealed witness needs native Lean; all
    // plan events (including direct invalidation) below instead come from the
    // real create_plan transaction and its refutation/recomputation paths.
    let raw = Connection::open(&path)?;
    assert_eq!(
        raw.execute(
            "UPDATE claims SET local_trust_level='L2b',effective_trust_level='L2b'
         WHERE claim_id IN ('a-refuted','z-refuted')",
            [],
        )?,
        2
    );

    let id = "top";
    let before_top = head(&store)?;
    plan(
        &mut store,
        id,
        "goal",
        "Top",
        PlanStatus::Active,
        &[("middle-z", "lemma"), ("middle-a", "definition")],
    )?;
    let top_create = plan_event(&store, before_top, id, "PLAN_CREATED")?;
    let top_trust = plan_event(&store, before_top, id, "PLAN_TRUST_CHANGED")?;

    let before_z = head(&store)?;
    plan(
        &mut store,
        "direct-z",
        "middle-z",
        "Direct Z",
        PlanStatus::Active,
        &[("z-refuted", "lemma")],
    )?;
    let direct_create = plan_event(&store, before_z, "direct-z", "PLAN_CREATED")?;
    let invalidated = plan_event(&store, before_z, "direct-z", "PLAN_INVALIDATED")?;
    let blocked_z = plan_event(&store, before_z, id, "PLAN_ROUTE_BLOCKED")?;
    let lost_trust = plan_event(&store, before_z, id, "PLAN_TRUST_CHANGED")?;
    assert!(direct_create < invalidated && invalidated < blocked_z && blocked_z < lost_trust);
    assert_eq!(
        store.plan_state(id)?,
        Some(("BLOCKED_BY_REFUTATION".into(), Trust::L0))
    );

    let before_a = head(&store)?;
    plan(
        &mut store,
        "direct-a",
        "middle-a",
        "Direct A",
        PlanStatus::Proved,
        &[("a-refuted", "lemma")],
    )?;
    let blocked_a = plan_event(&store, before_a, id, "PLAN_ROUTE_BLOCKED")?;

    let before_backup_a = head(&store)?;
    plan(
        &mut store,
        "backup-a",
        "middle-a",
        "Backup A",
        PlanStatus::Active,
        &[("backup", "lemma")],
    )?;
    let blocked_z_again = plan_event(&store, before_backup_a, id, "PLAN_ROUTE_BLOCKED")?;

    let before_backup_z = head(&store)?;
    plan(
        &mut store,
        "backup-z",
        "middle-z",
        "Backup Z",
        PlanStatus::Active,
        &[("backup", "lemma")],
    )?;
    let unblocked = plan_event(&store, before_backup_z, id, "PLAN_ROUTE_UNBLOCKED")?;
    let restored_trust = plan_event(&store, before_backup_z, id, "PLAN_TRUST_CHANGED")?;
    assert!(unblocked < restored_trust);
    let ledger_head = head(&store)?;
    assert_eq!(store.plan_state(id)?, Some(("ACTIVE".into(), Trust::L3)));
    assert_eq!(
        store.plan_state("direct-z")?,
        Some(("INVALIDATED_BY_REFUTATION".into(), Trust::L0))
    );
    assert_eq!(
        store.plan_state("direct-a")?,
        Some(("INVALIDATED_BY_REFUTATION".into(), Trust::L0))
    );
    drop(store);

    let reader = StateStore::open_readonly(&path)?;
    let active = "ACTIVE";
    let blocked = "BLOCKED_BY_REFUTATION";
    for (seq, applied, status, trust, blocker) in [
        (top_create, 1, active, "L0", None),
        (top_trust, 2, active, "L3", None),
        // Block overlays status immediately but does not imply a trust event.
        (blocked_z, 3, blocked, "L3", Some("z-refuted")),
        (lost_trust, 4, blocked, "L0", Some("z-refuted")),
        (blocked_a, 5, blocked, "L0", Some("a-refuted")),
        (blocked_z_again, 6, blocked, "L0", Some("z-refuted")),
        (unblocked, 7, active, "L0", None),
        (restored_trust, 8, active, "L3", None),
    ] {
        assert_snapshot(
            &reader,
            id,
            seq,
            ledger_head,
            applied,
            &json!({
                "plan_id": id,
                "target_claim_id": "goal",
                "name": "Top",
                "track": "exploration",
                "dependencies": [
                    {"claim_id": "middle-z", "kind": "lemma"},
                    {"claim_id": "middle-a", "kind": "definition"},
                ],
                "durable_status": "ACTIVE",
                "status": status,
                "effective_trust": trust,
                "blocking_claim_id": blocker,
                "refuted_claim_id": null,
                "last_event_seq": seq,
            }),
        )?;
    }
    let mut direct = json!({
        "plan_id": "direct-z",
        "target_claim_id": "middle-z",
        "name": "Direct Z",
        "track": "exploration",
        "dependencies": [{"claim_id": "z-refuted", "kind": "lemma"}],
        "durable_status": "ACTIVE",
        "status": "ACTIVE",
        "effective_trust": "L0",
        "blocking_claim_id": null,
        "refuted_claim_id": null,
        "last_event_seq": direct_create,
    });
    assert_snapshot(&reader, "direct-z", direct_create, ledger_head, 1, &direct)?;
    direct["durable_status"] = json!("INVALIDATED_BY_REFUTATION");
    direct["status"] = json!("INVALIDATED_BY_REFUTATION");
    direct["refuted_claim_id"] = json!("z-refuted");
    direct["last_event_seq"] = json!(invalidated);
    assert_snapshot(&reader, "direct-z", invalidated, ledger_head, 2, &direct)?;
    assert_snapshot(&reader, "direct-z", ledger_head, ledger_head, 2, &direct)?;
    assert_eq!(
        replay(&reader, id, None)?,
        replay(&reader, id, Some(ledger_head))?
    );
    assert_eq!(head(&reader)?, ledger_head);
    Ok(())
}

fn raw_plan_event(db: &Connection, id: &str, kind: &str, payload: Value) -> Result<i64> {
    db.execute(
        "INSERT INTO events (occurred_at,event_type,entity_type,entity_id,git_commit,actor,payload_json)
         VALUES ('2026-01-01T00:00:00Z',?1,'proof_plan',?2,?3,'fixture:malformed-ledger',?4)",
        params![kind, id, BASE, payload.to_string()],
    )?;
    Ok(db.last_insert_rowid())
}

#[test]
fn replay_rejects_corrupt_histories_and_bad_cursors_without_poisoning_valid_prefixes() -> Result<()>
{
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("state.sqlite");
    let mut store = StateStore::open(&path)?;
    claim(&mut store, "goal")?;
    for id in ["bad-transition", "unsupported", "valid"] {
        plan(&mut store, id, "goal", id, PlanStatus::Active, &[])?;
    }
    let prefix = head(&store)?;
    let raw = Connection::open(&path)?;
    let orphan = raw_plan_event(
        &raw,
        "orphan",
        "PLAN_TRUST_CHANGED",
        json!({"previous":"L0","effective":"L3"}),
    )?;
    let invalid = raw_plan_event(&raw, "bad-transition", "PLAN_ROUTE_UNBLOCKED", json!({}))?;
    let unsupported = raw_plan_event(&raw, "unsupported", "PLAN_FUTURE_MUTATION", json!({}))?;
    drop(store);

    let reader = StateStore::open_readonly(&path)?;
    assert!(reader.replay_plan("orphan", Some(orphan)).is_err());
    assert!(reader.replay_plan("bad-transition", Some(invalid)).is_err());
    assert!(
        reader
            .replay_plan("unsupported", Some(unsupported))
            .is_err()
    );
    assert!(reader.replay_plan("unsupported", None).is_err());
    assert!(reader.replay_plan("valid", Some(-1)).is_err());
    assert!(reader.replay_plan("valid", Some(unsupported + 1)).is_err());
    assert_snapshot(&reader, "orphan", prefix, unsupported, 0, &Value::Null)?;
    for (id, through) in [
        ("bad-transition", prefix),
        ("unsupported", invalid),
        ("valid", unsupported),
    ] {
        let at_prefix = replay(&reader, id, Some(through))?;
        assert_eq!(at_prefix["ledger_head_event_seq"], unsupported);
        assert_eq!(at_prefix["applied_events"], 1);
        assert_eq!(at_prefix["plan"]["effective_trust"], "L0");
    }
    assert_eq!(head(&reader)?, unsupported);
    Ok(())
}
