use ai4math_store::{Change, NewClaim, NewPlan, PlanStatus, StateStore, Trust};
use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use serde_json::{Value, json};

const BASE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const TAINT: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn change(commit: &str) -> Change<'_> {
    Change {
        actor: "graph-check-fixture",
        git_commit: Some(commit),
    }
}

fn claim(store: &mut StateStore, id: &str, local: Trust) -> Result<()> {
    store.create_claim(
        NewClaim {
            id,
            title: "Fixture claim title",
            informal_statement: "Private statement about α.",
            engine: "lean4",
            formal_target: None,
            module_path: None,
            critical: false,
        },
        change(BASE),
    )?;
    if local != Trust::L0 {
        store.set_local_trust(id, local, change(BASE))?;
    }
    Ok(())
}

fn plan(store: &mut StateStore, id: &str, target: &str, deps: &[(&str, &str)]) -> Result<()> {
    store.create_plan(
        NewPlan {
            id,
            name: "Fixture proof route",
            target,
            status: if deps.is_empty() {
                PlanStatus::Proved
            } else {
                PlanStatus::Active
            },
            track: "exploration",
            dependencies: deps,
        },
        change(BASE),
    )
}

fn event(db: &Connection, kind: &str, ty: &str, id: &str, payload: Value) -> Result<i64> {
    // Deliberate fixture bypass: emulate retained records, not a witness or proof authority.
    db.execute(
        "INSERT INTO events (occurred_at,event_type,entity_type,entity_id,actor,git_commit,payload_json)
         VALUES ('now',?1,?2,?3,'graph-check-fixture',?4,?5)",
        params![kind, ty, id, BASE, payload.to_string()],
    )?;
    Ok(db.last_insert_rowid())
}

fn seed_hard_refutation(db: &Connection, id: &str) -> Result<()> {
    assert_eq!(
        db.execute(
            "UPDATE claims SET local_trust_level='L2b',effective_trust_level='L2b'
             WHERE claim_id=?1 AND local_trust_level='L0' AND effective_trust_level='L0'",
            [id],
        )?,
        1
    );
    event(
        db,
        "CLAIM_TRUST_CHANGED",
        "claim",
        id,
        json!({"previous_local":"L0","local":"L2b","was_stale":false,"detail":{}}),
    )?;
    event(
        db,
        "CLAIM_EFFECTIVE_CHANGED",
        "claim",
        id,
        json!({"previous":"L0","effective":"L2b","effective_stale":false}),
    )?;
    Ok(())
}

fn claim_exhausted(db: &Connection, id: &str, cause: &str) -> Result<()> {
    db.execute(
        "INSERT INTO claim_route_exhaustion VALUES (?1,?2)",
        [id, cause],
    )?;
    event(
        db,
        "CLAIM_ROUTES_EXHAUSTED",
        "claim",
        id,
        json!({"refuted_claim":cause}),
    )?;
    Ok(())
}

fn route_blocked(db: &Connection, id: &str, cause: &str) -> Result<()> {
    db.execute(
        "INSERT INTO proof_plan_route_blocks VALUES (?1,?2)
         ON CONFLICT(plan_id) DO UPDATE SET refuted_claim_id=excluded.refuted_claim_id",
        [id, cause],
    )?;
    event(
        db,
        "PLAN_ROUTE_BLOCKED",
        "proof_plan",
        id,
        json!({"refuted_claim":cause}),
    )?;
    Ok(())
}

fn check(store: &StateStore) -> Result<Value> {
    Ok(serde_json::to_value(store.check_graph()?)?)
}

fn expected_report(head: i64, total: usize, rows: &[(&str, &str, &str)]) -> Value {
    let mismatches: Vec<_> = rows
        .iter()
        .map(|&(ty, id, field)| json!({"entity_type":ty,"entity_id":id,"field":field}))
        .collect();
    // Compare the entire sanitized result: no old/new values, causes, or receipts.
    json!({
        "ledger_event_seq": head,
        "consistent": total == 0,
        "mismatch_count": total,
        "truncated": total > rows.len(),
        "mismatches": mismatches,
    })
}

fn assert_clean_audit(store: &StateStore, head: i64) -> Result<()> {
    assert_eq!(
        serde_json::to_value(store.audit_graph()?)?,
        expected_report(head, 0, &[])
    );
    Ok(())
}

fn assert_clean_check(store: &StateStore) -> Result<()> {
    let head = store.status()?.last_event_seq;
    assert_clean_audit(store, head)?;
    assert_eq!(check(store)?, expected_report(head, 0, &[]));
    Ok(())
}

#[test]
fn matching_ledger_and_sql_can_misstate_trust_and_inherited_staleness() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("state.sqlite");
    let mut store = StateStore::open(&path)?;
    assert_clean_check(&store)?;
    for (id, trust) in [
        ("goal", Trust::L4a),
        ("consumer", Trust::L4a),
        ("z-low", Trust::L3),
        ("a-high", Trust::L4a),
        ("soft", Trust::L2a),
        ("soft-goal", Trust::L4a),
    ] {
        claim(&mut store, id, trust)?;
    }
    // Taint before creating any dependent: only the premise gets local staleness.
    store.mark_t3(&["z-low"], &[], change(TAINT))?;
    plan(
        &mut store,
        "weak-route",
        "goal",
        &[("z-low", "lemma"), ("a-high", "definition")],
    )?;
    plan(&mut store, "strong-route", "goal", &[("a-high", "lemma")])?;
    plan(
        &mut store,
        "consumer-route",
        "consumer",
        &[("goal", "lemma")],
    )?;
    // Soft refutation denies proof rank, not route viability. The later stale
    // premise still contributes effective staleness to this unblocked route.
    plan(
        &mut store,
        "soft-route",
        "soft-goal",
        &[("soft", "lemma"), ("z-low", "definition")],
    )?;
    let soft_goal = store.claim("soft-goal")?.context("soft goal missing")?;
    assert_eq!(
        (
            soft_goal.effective_trust,
            soft_goal.stale,
            soft_goal.effective_stale
        ),
        (Trust::L0, false, true)
    );
    let goal = store.claim("goal")?.context("goal missing")?;
    assert_eq!(
        (goal.effective_trust, goal.stale, goal.effective_stale),
        (Trust::L4a, false, true)
    );
    assert_eq!(
        store.plan_state("weak-route")?,
        Some(("ACTIVE".into(), Trust::L3))
    );
    assert_clean_check(&store)?;

    let mut raw = Connection::open(&path)?;
    let tx = raw.transaction()?;
    // False AND result, false OR result, and a false effective stale bit. Leave
    // consumer untouched: recomputation must not use the forged parent outputs.
    tx.execute_batch(
        "UPDATE proof_plans SET effective_trust_level='L4a' WHERE plan_id='weak-route';
         UPDATE claims SET effective_trust_level='L3',effective_stale=0 WHERE claim_id='goal';",
    )?;
    event(
        &tx,
        "PLAN_TRUST_CHANGED",
        "proof_plan",
        "weak-route",
        json!({"previous":"L3","effective":"L4a"}),
    )?;
    event(
        &tx,
        "CLAIM_EFFECTIVE_CHANGED",
        "claim",
        "goal",
        json!({"previous":"L4a","effective":"L3","effective_stale":false}),
    )?;
    tx.commit()?;
    let head = store.append_event(change(BASE), "NOTE_CREATED", "note", "ignored", json!({}))?;
    let reader = StateStore::open_readonly(&path)?;
    let before = serde_json::to_value(reader.events_after(0, 100)?)?;
    let version =
        || -> rusqlite::Result<i64> { raw.query_row("PRAGMA data_version", [], |r| r.get(0)) };
    let before_version = version()?;
    let expected = expected_report(
        head,
        3,
        &[
            ("claim", "goal", "effective_stale"),
            ("claim", "goal", "effective_trust"),
            ("proof_plan", "weak-route", "effective_trust"),
        ],
    );
    assert_clean_audit(&reader, head)?;
    assert_eq!(check(&reader)?, expected);
    assert_eq!(check(&store)?, expected);
    assert_eq!(check(&reader)?, expected);
    assert_eq!(version()?, before_version);
    assert_eq!(serde_json::to_value(reader.events_after(0, 100)?)?, before);
    let goal = reader.claim("goal")?.context("goal missing")?;
    assert_eq!(
        (goal.effective_trust, goal.effective_stale),
        (Trust::L3, false)
    );
    assert_eq!(
        reader.plan_state("weak-route")?,
        Some(("ACTIVE".into(), Trust::L4a))
    );

    // A live-only edit changes audit, but not the ledger semantic check.
    raw.execute(
        "UPDATE proof_plans SET effective_trust_level='L3' WHERE plan_id='weak-route'",
        [],
    )?;
    assert!(!reader.audit_graph()?.consistent);
    assert_eq!(check(&reader)?, expected);
    Ok(())
}

#[test]
fn typed_routes_change_blockers_and_reopen_but_forged_overlays_fail() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("state.sqlite");
    let mut store = StateStore::open(&path)?;
    for id in [
        "root-z", "root-a", "mid-z", "mid-a", "goal", "backup", "branch", "side",
    ] {
        let local = if matches!(id, "mid-z" | "mid-a" | "goal" | "backup") {
            Trust::L3
        } else {
            Trust::L0
        };
        claim(&mut store, id, local)?;
    }
    let mut raw = Connection::open(&path)?;
    let tx = raw.transaction()?;
    seed_hard_refutation(&tx, "root-z")?;
    seed_hard_refutation(&tx, "root-a")?;
    tx.commit()?;
    // Use actual typed creation for the refuted and inherited route projections.
    plan(&mut store, "direct-z", "mid-z", &[("root-z", "lemma")])?;
    plan(&mut store, "direct-a", "mid-a", &[("root-a", "lemma")])?;
    plan(
        &mut store,
        "branch-direct",
        "branch",
        &[("root-z", "lemma")],
    )?;
    plan(&mut store, "side-route", "side", &[("branch", "lemma")])?;
    plan(
        &mut store,
        "inherited",
        "goal",
        &[("mid-z", "lemma"), ("mid-a", "definition")],
    )?;
    let blocker = |store: &StateStore| -> Result<Option<String>> {
        Ok(store
            .replay_plan("inherited", None)?
            .plan
            .context("inherited missing")?
            .blocking_claim_id)
    };
    assert_eq!(blocker(&store)?.as_deref(), Some("root-a"));
    assert_clean_check(&store)?;

    plan(&mut store, "backup-a", "mid-a", &[("backup", "lemma")])?;
    assert_eq!(blocker(&store)?.as_deref(), Some("root-z"));
    assert_clean_check(&store)?;
    plan(&mut store, "backup-z", "mid-z", &[("backup", "lemma")])?;
    assert_eq!(blocker(&store)?, None);
    assert_eq!(
        store.plan_state("inherited")?,
        Some(("ACTIVE".into(), Trust::L3))
    );
    assert_clean_check(&store)?;

    let tx = raw.transaction()?;
    // One old cause can no longer block this route; on another route, the still
    // meaningful blocker is replaced by a real but unrelated refutation.
    route_blocked(&tx, "inherited", "root-a")?;
    claim_exhausted(&tx, "goal", "root-a")?;
    route_blocked(&tx, "side-route", "root-a")?;
    tx.execute(
        "DELETE FROM claim_route_exhaustion WHERE claim_id='branch'",
        [],
    )?;
    let head = event(&tx, "CLAIM_ROUTES_REOPENED", "claim", "branch", json!({}))?;
    tx.commit()?;
    let reader = StateStore::open_readonly(&path)?;
    assert_clean_audit(&reader, head)?;
    assert_eq!(
        check(&reader)?,
        expected_report(
            head,
            4,
            &[
                ("claim", "branch", "routes_exhausted"),
                ("claim", "goal", "routes_exhausted"),
                ("proof_plan", "inherited", "blocking_claim_id"),
                ("proof_plan", "side-route", "blocking_claim_id"),
            ]
        )
    );
    assert_eq!(
        reader.plan_state("inherited")?,
        Some(("BLOCKED_BY_REFUTATION".into(), Trust::L3))
    );
    Ok(())
}

#[test]
fn direct_invalidation_requires_a_hard_refuted_dependency_and_is_not_a_route_block() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("state.sqlite");
    let mut store = StateStore::open(&path)?;
    for (id, local) in [
        ("hard-z", Trust::L0),
        ("hard-a", Trust::L0),
        ("late-root", Trust::L0),
        ("soft", Trust::L2a),
        ("good-target", Trust::L0),
        ("false-target", Trust::L4a),
        ("missing-target", Trust::L0),
        ("wrong-cause-target", Trust::L0),
    ] {
        claim(&mut store, id, local)?;
    }
    let mut raw = Connection::open(&path)?;
    let tx = raw.transaction()?;
    seed_hard_refutation(&tx, "hard-z")?;
    tx.commit()?;
    plan(
        &mut store,
        "good",
        "good-target",
        &[("hard-a", "lemma"), ("hard-z", "lemma")],
    )?;
    plan(&mut store, "false", "false-target", &[("soft", "lemma")])?;
    plan(&mut store, "false-alternate", "false-target", &[])?;
    plan(
        &mut store,
        "missing",
        "missing-target",
        &[("late-root", "lemma")],
    )?;
    plan(
        &mut store,
        "wrong-cause",
        "wrong-cause-target",
        &[("soft", "lemma"), ("late-root", "lemma")],
    )?;
    assert_eq!(
        store.plan_state("good")?,
        Some(("INVALIDATED_BY_REFUTATION".into(), Trust::L0))
    );
    assert_clean_check(&store)?;

    let tx = raw.transaction()?;
    // The old valid invalidation named hard-z. Newly hard hard-a sorts first,
    // but cannot replace the cause of an already terminal, historical event.
    seed_hard_refutation(&tx, "hard-a")?;
    tx.execute(
        "UPDATE claim_route_exhaustion SET refuted_claim_id='hard-a' WHERE claim_id='good-target'",
        [],
    )?;
    seed_hard_refutation(&tx, "late-root")?;
    claim_exhausted(&tx, "missing-target", "late-root")?;
    claim_exhausted(&tx, "wrong-cause-target", "late-root")?;
    // Omit PLAN_INVALIDATED on missing. Emit it on false and wrong-cause,
    // naming a direct dependency for structural replay, but one that is only L2a.
    let mut head = 0;
    for (id, target) in [
        ("false", "false-target"),
        ("wrong-cause", "wrong-cause-target"),
    ] {
        tx.execute(
            "UPDATE proof_plans SET status='INVALIDATED_BY_REFUTATION' WHERE plan_id=?1",
            [id],
        )?;
        head = event(
            &tx,
            "PLAN_INVALIDATED",
            "proof_plan",
            id,
            json!({"refuted_claim":"soft","target":target}),
        )?;
    }
    tx.commit()?;
    assert_clean_audit(&store, head)?;
    assert_eq!(
        check(&store)?,
        expected_report(
            head,
            3,
            &[
                ("proof_plan", "false", "durable_status"),
                ("proof_plan", "missing", "durable_status"),
                ("proof_plan", "wrong-cause", "durable_status"),
            ]
        )
    );

    // An ordinary reversible overlay cannot substitute for terminal invalidation.
    let tx = raw.transaction()?;
    route_blocked(&tx, "missing", "late-root")?;
    tx.commit()?;
    let head = store.status()?.last_event_seq;
    assert_clean_audit(&store, head)?;
    assert_eq!(
        check(&store)?,
        expected_report(
            head,
            4,
            &[
                ("proof_plan", "false", "durable_status"),
                ("proof_plan", "missing", "blocking_claim_id"),
                ("proof_plan", "missing", "durable_status"),
                ("proof_plan", "wrong-cause", "durable_status"),
            ]
        )
    );
    Ok(())
}

#[test]
fn semantic_report_keeps_total_and_a_deterministic_bounded_prefix() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("state.sqlite");
    let mut store = StateStore::open(&path)?;
    let ids: Vec<_> = (0..130).map(|i| format!("claim-{i:03}")).collect();
    for id in ids.iter().rev() {
        claim(&mut store, id, Trust::L0)?;
    }
    let mut raw = Connection::open(&path)?;
    let tx = raw.transaction()?;
    let mut head = 0;
    for id in ids.iter().rev() {
        tx.execute(
            "UPDATE claims SET effective_trust_level='L3' WHERE claim_id=?1",
            [id],
        )?;
        head = event(
            &tx,
            "CLAIM_EFFECTIVE_CHANGED",
            "claim",
            id,
            json!({"previous":"L0","effective":"L3","effective_stale":false}),
        )?;
    }
    tx.commit()?;
    let reader = StateStore::open_readonly(&path)?;
    assert_clean_audit(&reader, head)?;
    let rows: Vec<_> = ids[..128]
        .iter()
        .map(|id| ("claim", id.as_str(), "effective_trust"))
        .collect();
    let expected = expected_report(head, ids.len(), &rows);
    assert_eq!(check(&reader)?, expected);
    assert_eq!(check(&store)?, expected);
    Ok(())
}
