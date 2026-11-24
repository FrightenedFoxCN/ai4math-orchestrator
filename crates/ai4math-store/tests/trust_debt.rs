use ai4math_store::{Change, NewClaim, NewPlan, PlanStatus, StateStore, Trust};
use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use serde_json::{Value, json};

const BASE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const TAINT_ROOT: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn change(commit: &str) -> Change<'_> {
    Change {
        actor: "trust-debt-fixture",
        git_commit: Some(commit),
    }
}

fn claim(store: &mut StateStore, id: &str, engine: &str, local: Trust) -> Result<()> {
    store.create_claim(
        NewClaim {
            id,
            title: "Private fixture title",
            informal_statement: "Private fixture statement about α.",
            engine,
            formal_target: None,
            module_path: None,
            critical: id == "goal",
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
            name: "Private fixture proof route",
            target,
            status: PlanStatus::Active,
            track: "exploration",
            dependencies: deps,
        },
        change(BASE),
    )
}

fn raw_claim_event(raw: &Connection, id: &str, kind: &str, payload: Value) -> Result<()> {
    assert_eq!(
        raw.execute(
            "INSERT INTO events (occurred_at,event_type,entity_type,entity_id,actor,git_commit,payload_json)
             VALUES ('now',?1,'claim',?2,'trust-debt-fixture',?3,?4)",
            params![kind, id, BASE, payload.to_string()],
        )?,
        1
    );
    Ok(())
}

fn seed_retained_rank(raw: &mut Connection, id: &str, rank: Trust) -> Result<()> {
    // L4b has no public admission, and a genuine L5 receipt needs a native
    // checker. Seed matching history and projection only in this fixture; this
    // is not proof evidence or an alternate admission path.
    assert!(matches!(rank, Trust::L4b | Trust::L5));
    let tx = raw.transaction()?;
    assert_eq!(
        tx.execute(
            "UPDATE claims SET local_trust_level=?2,effective_trust_level=?2
             WHERE claim_id=?1 AND local_trust_level='L0' AND effective_trust_level='L0'",
            params![id, rank.as_str()],
        )?,
        1
    );
    raw_claim_event(
        &tx,
        id,
        "CLAIM_TRUST_CHANGED",
        json!({"previous_local":"L0","local":rank,"was_stale":false,"detail":{}}),
    )?;
    raw_claim_event(
        &tx,
        id,
        "CLAIM_EFFECTIVE_CHANGED",
        json!({"previous":"L0","effective":rank,"effective_stale":false}),
    )?;
    tx.commit()?;
    Ok(())
}

struct Finding<'a> {
    id: &'a str,
    engine: &'a str,
    critical: bool,
    stale: bool,
    effective_stale: bool,
    local: Trust,
    effective: Trust,
    prior: Option<Trust>,
}

impl Finding<'_> {
    fn row(&self, kind: &str, role: &str) -> Value {
        let mut row = json!({
            "claim_id": self.id, "engine": self.engine, "critical": self.critical,
            "stale": self.stale, "effective_stale": self.effective_stale,
            "kind": kind, "role": role,
            "local_trust": self.local, "effective_trust": self.effective,
        });
        if let Some(prior) = self.prior {
            row["prior_trust"] = json!(prior);
        }
        row
    }
}

fn scan(store: &StateStore) -> Result<Value> {
    Ok(serde_json::to_value(store.scan_trust_debt()?)?)
}

fn expected_scan(head: i64, counts: [usize; 6], findings: &[Value]) -> Value {
    let [
        direct_oracle,
        direct_cross,
        prior_oracle,
        prior_cross,
        ceiling_oracle,
        ceiling_cross,
    ] = counts;
    let total = counts.into_iter().sum::<usize>();
    json!({
        "schema": "ai4math.t2.trust-debt-scan.v1",
        "advisory_only": true,
        "ledger_event_seq": head,
        "counts": {
            "direct_oracle": direct_oracle,
            "direct_cross_engine": direct_cross,
            "prior_tainted_oracle": prior_oracle,
            "prior_tainted_cross_engine": prior_cross,
            "effective_ceiling_oracle": ceiling_oracle,
            "effective_ceiling_cross_engine": ceiling_cross,
        },
        "finding_count": total,
        "truncated": total > findings.len(),
        "findings": findings,
    })
}

fn data_version(raw: &Connection) -> rusqlite::Result<i64> {
    raw.query_row("PRAGMA data_version", [], |row| row.get(0))
}

#[test]
fn classifies_current_tainted_and_effective_debt_across_real_or_routes() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("state.sqlite");
    let mut store = StateStore::open(&path)?;
    assert_eq!(scan(&store)?, expected_scan(0, [0; 6], &[]));
    for (id, engine, local) in [
        ("oracle", "lean4", Trust::L4a),
        ("weak", "lean4", Trust::L3),
        ("low-goal", "lean4", Trust::L4a),
        ("goal", "lean4", Trust::L0),
        ("strong", "lean4", Trust::L0),
        ("cross", "rocq", Trust::L0),
    ] {
        claim(&mut store, id, engine, local)?;
    }
    let mut raw = Connection::open(&path)?;
    seed_retained_rank(&mut raw, "goal", Trust::L5)?;
    seed_retained_rank(&mut raw, "strong", Trust::L5)?;
    seed_retained_rank(&mut raw, "cross", Trust::L4b)?;
    // Taint before installing routes: goal inherits effective staleness without
    // becoming locally stale. The L3 prior trust must not become a debt finding.
    store.mark_t3(&["oracle", "cross", "weak"], &[], change(TAINT_ROOT))?;
    plan(&mut store, "weak-route", "low-goal", &[("weak", "lemma")])?;
    plan(
        &mut store,
        "oracle-route",
        "goal",
        &[("strong", "definition"), ("oracle", "lemma")],
    )?;

    let cross = Finding {
        id: "cross",
        engine: "rocq",
        critical: false,
        stale: true,
        effective_stale: true,
        local: Trust::L4b,
        effective: Trust::L4b,
        prior: Some(Trust::L4b),
    };
    let mut goal = Finding {
        id: "goal",
        engine: "lean4",
        critical: true,
        stale: false,
        effective_stale: true,
        local: Trust::L5,
        effective: Trust::L4a,
        prior: None,
    };
    let low_goal = Finding {
        id: "low-goal",
        engine: "lean4",
        critical: false,
        stale: false,
        effective_stale: true,
        local: Trust::L4a,
        effective: Trust::L3,
        prior: None,
    };
    let oracle = Finding {
        id: "oracle",
        prior: Some(Trust::L4a),
        effective: Trust::L4a,
        stale: true,
        ..low_goal
    };
    let mut rows = vec![
        cross.row("cross_engine", "direct"),
        cross.row("cross_engine", "prior_tainted"),
        cross.row("cross_engine", "effective_ceiling"),
        goal.row("oracle", "effective_ceiling"),
        low_goal.row("oracle", "direct"),
        oracle.row("oracle", "direct"),
        oracle.row("oracle", "prior_tainted"),
        oracle.row("oracle", "effective_ceiling"),
    ];
    assert_eq!(
        scan(&store)?,
        expected_scan(store.status()?.last_event_seq, [2, 1, 1, 1, 2, 1], &rows)
    );

    // The best route, not the existence of an oracle edge, determines the
    // effective ceiling. An L4b alternate changes the debt kind on the L5 goal.
    plan(&mut store, "cross-route", "goal", &[("cross", "lemma")])?;
    goal.effective = Trust::L4b;
    rows[3] = goal.row("cross_engine", "effective_ceiling");
    assert_eq!(
        scan(&store)?,
        expected_scan(store.status()?.last_event_seq, [2, 1, 1, 1, 1, 2], &rows)
    );

    // A debt-free alternate removes only the goal's ceiling. The advisory
    // scan also tracks the global head even when the last event is not a graph row.
    plan(&mut store, "strong-route", "goal", &[("strong", "lemma")])?;
    let head = store.append_event(change(BASE), "NOTE_CREATED", "note", "ignored", json!({}))?;
    let goal = store.claim("goal")?.context("missing goal")?;
    assert_eq!(
        (goal.effective_trust, goal.effective_stale),
        (Trust::L5, true)
    );
    for (route, effective) in [
        ("oracle-route", Trust::L4a),
        ("cross-route", Trust::L4b),
        ("strong-route", Trust::L5),
    ] {
        assert_eq!(store.plan_state(route)?, Some(("ACTIVE".into(), effective)));
    }
    rows.remove(3);
    let expected = expected_scan(head, [2, 1, 1, 1, 1, 1], &rows);
    let reader = StateStore::open_readonly(&path)?;
    let before_events = serde_json::to_value(reader.events_after(0, 100)?)?;
    let before_graph = serde_json::to_value(reader.replay_graph(None)?)?;
    let version = data_version(&raw)?;
    assert_eq!(scan(&reader)?, expected);
    assert_eq!(scan(&store)?, expected);
    assert_eq!(scan(&reader)?, expected);
    assert_eq!(
        serde_json::to_value(reader.events_after(0, 100)?)?,
        before_events
    );
    assert_eq!(
        serde_json::to_value(reader.replay_graph(None)?)?,
        before_graph
    );
    assert!(reader.audit_graph()?.consistent);
    assert_eq!(data_version(&raw)?, version);
    Ok(())
}

#[test]
fn fails_closed_on_live_drift_and_on_matching_but_semantically_wrong_history() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("state.sqlite");
    let mut store = StateStore::open(&path)?;
    claim(&mut store, "oracle", "lean4", Trust::L4a)?;
    claim(&mut store, "unrelated", "lean4", Trust::L3)?;
    assert_eq!(scan(&store)?["finding_count"], 2);
    let raw = Connection::open(&path)?;
    let reader = StateStore::open_readonly(&path)?;
    // Guard the whole graph, even an unrelated Claim without debt.
    assert_eq!(
        raw.execute(
            "UPDATE claims SET effective_trust_level='L1' WHERE claim_id='unrelated'",
            [],
        )?,
        1
    );
    assert!(reader.check_graph()?.consistent);
    assert_eq!(reader.audit_graph()?.mismatch_count, 1);
    let version = data_version(&raw)?;
    let err = scan(&reader).expect_err("projection drift must fail closed");
    assert!(format!("{err:#}").contains("live graph differs"), "{err:#}");
    assert_eq!(reader.audit_graph()?.mismatch_count, 1);
    assert_eq!(data_version(&raw)?, version);

    // Bring ledger and projection into agreement at the same wrong L1 rank.
    // With no plans, effective trust must equal the unchanged local L3.
    raw_claim_event(
        &raw,
        "unrelated",
        "CLAIM_EFFECTIVE_CHANGED",
        json!({"previous":"L3","effective":"L1","effective_stale":false}),
    )?;
    assert!(reader.audit_graph()?.consistent);
    assert_eq!(reader.check_graph()?.mismatch_count, 1);
    let version = data_version(&raw)?;
    let before_events = serde_json::to_value(reader.events_after(0, 100)?)?;
    let err = scan(&store).expect_err("ledger semantic drift must fail closed");
    assert!(
        format!("{err:#}").contains("ledger graph semantic check failed"),
        "{err:#}"
    );
    assert_eq!(reader.check_graph()?.mismatch_count, 1);
    assert_eq!(
        serde_json::to_value(reader.events_after(0, 100)?)?,
        before_events
    );
    assert_eq!(data_version(&raw)?, version);
    Ok(())
}

#[test]
fn keeps_exact_totals_and_only_the_sorted_128_row_prefix() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("state.sqlite");
    let mut store = StateStore::open(&path)?;
    // Write in reverse order; the report must order by Claim ID, then role.
    for index in (0..64).rev() {
        claim(
            &mut store,
            &format!("oracle-{index:02}"),
            "lean4",
            Trust::L4a,
        )?;
    }
    let mut prefix = Vec::new();
    for index in 0..64 {
        let id = format!("oracle-{index:02}");
        let finding = Finding {
            id: &id,
            engine: "lean4",
            critical: false,
            stale: false,
            effective_stale: false,
            local: Trust::L4a,
            effective: Trust::L4a,
            prior: None,
        };
        prefix.push(finding.row("oracle", "direct"));
        prefix.push(finding.row("oracle", "effective_ceiling"));
    }
    assert_eq!(
        scan(&store)?,
        expected_scan(
            store.status()?.last_event_seq,
            [64, 0, 0, 0, 64, 0],
            &prefix
        )
    );

    claim(&mut store, "z-cross", "rocq", Trust::L0)?;
    seed_retained_rank(&mut Connection::open(&path)?, "z-cross", Trust::L4b)?;
    // The cross-engine findings fall entirely outside the returned prefix, but
    // must still be counted in both the per-category totals and finding_count.
    let reader = StateStore::open_readonly(&path)?;
    assert_eq!(
        scan(&reader)?,
        expected_scan(
            reader.status()?.last_event_seq,
            [64, 1, 0, 0, 64, 1],
            &prefix
        )
    );
    Ok(())
}
