use ai4math_store::{Change, NewClaim, StateStore, Trust};
use anyhow::Result;
use rusqlite::Connection;
use serde_json::json;

const CHANGE: Change<'static> = Change {
    actor: "fixture",
    git_commit: Some("abc0123"),
};

#[test]
fn semantic_evidence_survives_noisy_tail_and_respects_checkpoint() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let db = dir.path().join("evidence.sqlite");
    let mut store = StateStore::open(&db)?;
    store.create_claim(
        NewClaim {
            id: "main",
            title: "Critical",
            informal_statement: "Informal statement",
            engine: "lean4",
            formal_target: Some("Math.lean#main"),
            module_path: None,
            critical: true,
        },
        CHANGE,
    )?;
    let checkpoint = store.append_event(
        CHANGE,
        "CLAIM_TRUST_CHANGED",
        "claim",
        "excluded",
        json!({"local":"L5"}),
    )?;
    let raw = Connection::open(&db)?;
    raw.execute(
        "INSERT INTO human_checkpoints (cursor_id,updated_at,event_seq,git_commit,is_current)
        VALUES ('human','now',?1,'abc0123',1)",
        [checkpoint],
    )?;
    raw.execute(
        "UPDATE claims SET formal_signature='theorem main : True' WHERE claim_id='main'",
        [],
    )?;

    // The transition is valid even though its raw payload exceeds the event
    // payload budget. Its local level and witness are projected independently.
    let milestone = store.append_event(
        CHANGE,
        "CLAIM_TRUST_CHANGED",
        "claim",
        "main",
        json!({"local":"L5", "detail":{"proof_body":"redact".repeat(5000)}}),
    )?;
    let risk = store.append_event(
        CHANGE,
        "BREAKER_TRIPPED",
        "breaker",
        "a-breaker",
        json!({"state":"TRIPPED","logs":"do not distribute this log"}),
    )?;
    // Projection fixture only: the production API cannot append a raw refutation.
    for (kind, payload) in [
        (
            "CLAIM_TRUST_CHANGED",
            json!({"local":"L2b","detail":{"wiki_id":"GRV-real"}}),
        ),
        (
            "CRITICAL_CLAIM_REFUTED",
            json!({"critical_claim":"main","refuted_claim":"main","claim_itself_refuted":true}),
        ),
    ] {
        assert!(
            store
                .append_event(CHANGE, kind, "claim", "main", payload.clone())
                .is_err()
        );
        raw.execute("INSERT INTO events (occurred_at,event_type,entity_type,entity_id,git_commit,actor,payload_json)
            VALUES ('now',?1,'claim','main','abc0123','trusted-fixture',?2)",rusqlite::params![kind,payload.to_string()])?;
    }
    let latest_soft = store.append_event(
        CHANGE,
        "CLAIM_TRUST_CHANGED",
        "claim",
        "other",
        json!({"local":"L2a"}),
    )?;
    let debt = store.append_event(
        CHANGE,
        "CLAIM_TRUST_CHANGED",
        "claim",
        "debt",
        json!({"local":"L4a"}),
    )?;
    raw.execute(
        "INSERT INTO events (occurred_at,event_type,entity_type,entity_id,actor,payload_json)
        VALUES ('now','CLAIM_TRUST_CHANGED','claim','broken','fixture','{invalid')",
        [],
    )?;
    // A second bogus transition uses a valid document but not a trust enum string.
    store.append_event(
        CHANGE,
        "CLAIM_TRUST_CHANGED",
        "claim",
        "not-a-level",
        json!({"local":{"L5":true}}),
    )?;
    for n in 0..100 {
        store.append_event(
            CHANGE,
            "NOISE",
            "worker",
            &format!("worker-{n}"),
            json!({"proof_body":"do not distribute private text"}),
        )?;
    }
    let pack = store.briefing_for_alerts()?;
    assert_eq!(pack.event_from_exclusive, checkpoint);
    assert!(pack.events.truncated);
    assert_eq!(pack.events.items.len(), 80);
    assert!(
        pack.events
            .items
            .iter()
            .all(|e| e.payload == json!({"payload_omitted":true}))
    );
    assert!(pack.events.items.iter().all(|e| e.event_seq > risk));

    assert!(!pack.milestones.truncated);
    assert_eq!(
        pack.milestones
            .items
            .iter()
            .map(|t| t.event_seq)
            .collect::<Vec<_>>(),
        [milestone, debt]
    );
    let first = &pack.milestones.items[0];
    assert_eq!(first.new_local_trust, Trust::L5);
    assert_eq!(first.git_commit.as_deref(), CHANGE.git_commit);
    let claim = first.claim.as_ref().unwrap();
    assert_eq!(claim.informal_statement, "Informal statement");
    assert_eq!(
        claim.formal_signature.as_deref(),
        Some("theorem main : True")
    );
    assert!(!pack.refutations.truncated);
    assert_eq!(pack.refutations.items.len(), 1);
    assert_eq!(pack.refutations.items[0].new_local_trust, Trust::L2b);
    assert_eq!(
        pack.refutations.items[0].graveyard_id.as_deref(),
        Some("GRV-real")
    );

    let risks = &pack.risk_radar.events.items;
    assert_eq!(risks[0].event_type, "CRITICAL_CLAIM_REFUTED");
    assert!(risks.iter().any(|e| e.event_seq == latest_soft));
    let breaker = risks.iter().find(|e| e.event_seq == risk).unwrap();
    assert_eq!(
        breaker.payload,
        json!({"state":"TRIPPED","payload_omitted":true})
    );
    assert_eq!(pack.alert_sources[0].event_seq, risks[0].event_seq);
    assert_eq!(pack.alert_sources[0].payload["claim_itself_refuted"], true);
    assert!(!serde_json::to_string(&pack)?.contains("do not distribute"));
    Ok(())
}

#[test]
fn semantic_page_marks_only_its_own_overflow_and_preserves_identifiers() -> Result<()> {
    let mut store = StateStore::open(":memory:")?;
    for n in 0..9 {
        store.append_event(
            CHANGE,
            "CLAIM_TRUST_CHANGED",
            "claim",
            &format!("proof-{n}"),
            json!({"local":"L5"}),
        )?;
    }
    let wide_id = "界".repeat(200);
    let latest = store.append_event(
        CHANGE,
        "PROMOTION_COMMITTED",
        "claim",
        &wide_id,
        json!({"base_commit":"base", "receipt_id":"r", "diagnostics":{"private":true}}),
    )?;
    let pack = store.briefing_facts()?;
    assert!(!pack.events.truncated);
    assert!(pack.milestones.truncated);
    assert_eq!(pack.milestones.items.len(), 8);
    assert_eq!(pack.milestones.items.first().unwrap().claim_id, "proof-8");
    assert_eq!(pack.milestones.items.last().unwrap().claim_id, "proof-1");
    assert!(!pack.refutations.truncated);
    let event = pack.events.items.last().unwrap();
    assert_eq!(event.event_seq, latest);
    assert!(event.entity_id.is_empty());
    assert_eq!(
        event.payload,
        json!({"base_commit":"base","receipt_id":"r","metadata_omitted":true,"payload_omitted":true})
    );
    Ok(())
}

#[test]
fn breaker_page_filters_before_limit_with_a_safe_payload() -> Result<()> {
    let mut store = StateStore::open(":memory:")?;
    let cursor = store.append_event(
        CHANGE,
        "BREAKER_STATE_CHANGED",
        "breaker",
        "excluded",
        json!({}),
    )?;
    for _ in 0..150 {
        store.append_event(
            CHANGE,
            "BREAKER_STATE_CHANGED",
            "worker",
            "wrong",
            json!({}),
        )?;
    }
    let first = store.append_event(
        CHANGE,
        "BREAKER_TRIPPED",
        "breaker",
        "first",
        json!({"state":"TRIPPED","proof_body":"never publish this"}),
    )?;
    let second = store.append_event(
        CHANGE,
        "BREAKER_CLOSED",
        "breaker",
        "second",
        json!({"state":"CLOSED","probe_status":"PASSED"}),
    )?;
    let through = store.append_event(CHANGE, "NOISE", "worker", "end", json!({}))?;
    store.append_event(CHANGE, "BREAKER_TRIPPED", "breaker", "future", json!({}))?;
    let page = store.breaker_events_through(cursor, through, 1)?;
    assert!(page.truncated);
    assert_eq!(page.items[0].event_seq, first);
    assert_eq!(
        page.items[0].payload,
        json!({"state":"TRIPPED","payload_omitted":true})
    );
    let next = store.breaker_events_through(first, through, 1)?;
    assert!(!next.truncated);
    assert_eq!(next.items[0].event_seq, second);
    assert_eq!(
        next.items[0].payload,
        json!({"state":"CLOSED","payload_omitted":true})
    );
    assert!(store.breaker_events_through(through, cursor, 1).is_err());
    Ok(())
}
