use super::*;
use ai4math_store::{Change, Event, NewClaim, StateStore};
use serde_json::json;

const CHANGE: Change<'static> = Change {
    actor: "test",
    git_commit: Some("base"),
};

#[test]
fn compact_budget_is_deterministic_preserves_evidence_and_fits_a_frame() -> Result<()> {
    let state_dir = tempfile::tempdir()?;
    let path = state_dir.path().join("fixture.sqlite");
    let mut store = StateStore::open(&path)?;
    let escaped = "\x01".repeat(2048);
    for n in 0..24 {
        store.create_claim(
            NewClaim {
                id: &format!("claim-{n:02}"),
                title: "Bounded claim",
                informal_statement: &escaped,
                engine: "lean4",
                formal_target: None,
                module_path: None,
                critical: true,
            },
            CHANGE,
        )?;
    }
    let milestone = store.append_event(
        CHANGE,
        "CLAIM_TRUST_CHANGED",
        "claim",
        "claim-00",
        json!({"local":"L5"}),
    )?;
    // Simulate a historical alert backlog without weakening sealed sources.
    let legacy = rusqlite::Connection::open(&path)?;
    for n in 0..32 {
        legacy.execute("INSERT INTO events(occurred_at,event_type,entity_type,entity_id,actor)
            VALUES('2026-01-01T00:00:00Z','CRITICAL_BREAKER_TRIPPED','breaker',?1,'legacy:fixture')",[format!("threat-{n}")])?;
    }

    let fixture = |store: &mut StateStore| -> Result<FactBody> {
        let mut body = store.briefing_for_alerts()?;
        body.refutations.items.push(ai4math_store::TrustTransition {
            event_seq: milestone + 1,
            git_commit: None,
            claim: None,
            claim_id: "claim-00".into(),
            new_local_trust: ai4math_store::Trust::L2b,
            graveyard_id: Some("GRAVEYARD-1".into()),
        });
        body.events.items = (1..=80)
            .map(|event_seq| Event {
                event_seq,
                occurred_at: "now".into(),
                event_type: "NOISE".into(),
                entity_type: "worker".into(),
                entity_id: "worker".into(),
                git_commit: None,
                actor: "fixture".into(),
                payload: json!({"escapes":escaped, "utf8":"数学".repeat(1000)}),
            })
            .collect();
        Ok(body)
    };
    let pack = FactPack::compile(fixture(&mut store)?)?;
    let repeat = FactPack::compile(fixture(&mut store)?)?;
    assert_eq!(pack.fact_sha256, repeat.fact_sha256);
    assert_eq!(serde_json::to_vec(&pack)?, serde_json::to_vec(&repeat)?);
    assert!(pack.body.events.truncated);
    assert_eq!(pack.body.events.items.last().unwrap().event_seq, 80);
    assert!(pack.body.frontier.claims.truncated);
    assert!(!pack.body.milestones.truncated);
    assert_eq!(pack.body.milestones.items[0].event_seq, milestone);
    assert_eq!(
        pack.body.refutations.items[0].graveyard_id.as_deref(),
        Some("GRAVEYARD-1")
    );
    assert_eq!(pack.body.alert_sources.len(), 32);

    let dir = tempfile::tempdir()?;
    pack.retain(dir.path())?;
    repeat.retain(dir.path())?;
    let file = dir.path().join(format!("{}.json", pack.fact_sha256));
    let bytes = fs::read(&file)?;
    assert_eq!(bytes, serde_json::to_vec(&pack)?);
    assert!(bytes.len() <= MAX_PACK_BYTES);
    let retained = read_retained(dir.path(), &pack.fact_sha256)?.unwrap();
    assert_eq!(retained, serde_json::to_value(&pack)?);
    // Include the complete RPC envelope and escaped client ID in the budget.
    let frame = serde_json::to_vec(&json!({"jsonrpc":"2.0", "id":"\x01".repeat(1024),
        "result":{"retained_id":pack.fact_sha256,"fact_pack":retained}}))?;
    assert!(frame.len() < MAX_FRAME_BYTES);
    Ok(())
}

#[test]
fn rejects_digest_valid_legacy_pack_that_cannot_be_served() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut body = json!({"legacy": "x".repeat(MAX_PACK_BYTES)});
    let id = hash_fact(&body)?;
    body["fact_sha256"] = json!(id);
    let bytes = serde_json::to_vec_pretty(&body)?;
    assert!((bytes.len() as u64) < MAX_LEGACY_FILE_BYTES);
    fs::write(dir.path().join(format!("{id}.json")), bytes)?;
    let err = read_retained(dir.path(), &id).unwrap_err();
    assert!(err.to_string().contains("compact byte budget"));
    Ok(())
}

#[test]
fn retained_pretty_legacy_pack_is_verified_and_links_are_refused() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut store = StateStore::open(":memory:")?;
    let pack = FactPack::compile(store.briefing_facts()?)?;
    let destination = dir.path().join(format!("{}.json", pack.fact_sha256));
    let other = dir.path().join("other.json");
    fs::write(&other, serde_json::to_vec_pretty(&pack)?)?;
    std::os::unix::fs::symlink(&other, &destination)?;
    assert!(read_retained(dir.path(), &pack.fact_sha256).is_err());
    assert!(pack.retain(dir.path()).is_err());
    fs::rename(other, destination)?;
    assert_eq!(
        read_retained(dir.path(), &pack.fact_sha256)?.unwrap(),
        serde_json::to_value(&pack)?
    );
    Ok(())
}
