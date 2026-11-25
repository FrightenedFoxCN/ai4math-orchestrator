use super::*;
use ai4math_store::{Change, Event, NewClaim, StateStore, retained::MAX_LEGACY_FILE_BYTES};
use serde_json::json;
use std::{
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt, symlink},
};

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
    let pretty = serde_json::to_vec_pretty(&pack)?;
    fs::write(&other, &pretty)?;
    symlink(&other, &destination)?;
    assert!(read_retained(dir.path(), &pack.fact_sha256).is_err());
    assert!(pack.retain(dir.path()).is_err());
    assert_eq!(fs::read(&other)?, pretty);
    fs::remove_file(&destination)?;
    fs::hard_link(&other, &destination)?;
    assert!(read_retained(dir.path(), &pack.fact_sha256).is_err());
    assert!(pack.retain(dir.path()).is_err());
    fs::remove_file(&other)?;
    let inode = fs::metadata(&destination)?.ino();
    pack.retain(dir.path())?;
    assert_eq!(fs::read(&destination)?, pretty);
    assert_eq!(fs::metadata(&destination)?.ino(), inode);
    assert!(
        dir.path()
            .join(format!("{}.md", pack.fact_sha256))
            .is_file()
    );
    assert_eq!(
        read_retained(dir.path(), &pack.fact_sha256)?.unwrap(),
        serde_json::to_value(&pack)?
    );
    Ok(())
}

#[test]
fn markdown_conflict_keeps_verified_json_and_retry_fills_missing_summary() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut store = StateStore::open(":memory:")?;
    let pack = FactPack::compile(store.briefing_facts()?)?;
    let json_path = dir.path().join(format!("{}.json", pack.fact_sha256));
    let md_path = dir.path().join(format!("{}.md", pack.fact_sha256));
    fs::write(&md_path, b"existing nonmatching summary")?;

    let err = pack.retain(dir.path()).unwrap_err();
    assert!(err.to_string().contains("Markdown summary"));
    assert_eq!(fs::read(&md_path)?, b"existing nonmatching summary");
    assert_eq!(
        read_retained(dir.path(), &pack.fact_sha256)?,
        Some(serde_json::to_value(&pack)?)
    );
    let json_ino = fs::metadata(&json_path)?.ino();

    fs::remove_file(&md_path)?;
    pack.retain(dir.path())?;
    assert_eq!(fs::metadata(&json_path)?.ino(), json_ino);
    assert!(fs::read_to_string(&md_path)?.contains(&pack.fact_sha256));
    let md_ino = fs::metadata(&md_path)?.ino();
    pack.retain(dir.path())?;
    assert_eq!(fs::metadata(&md_path)?.ino(), md_ino);
    assert_eq!(fs::read_dir(dir.path())?.count(), 2);
    Ok(())
}

#[test]
fn unsafe_directory_and_oversized_json_are_not_written() -> Result<()> {
    let root = tempfile::tempdir()?;
    let dir = root.path().join("retained");
    fs::create_dir(&dir)?;
    let mut store = StateStore::open(":memory:")?;
    let pack = FactPack::compile(store.briefing_facts()?)?;
    let alias = root.path().join("alias");
    symlink(&dir, &alias)?;
    assert!(pack.retain(&alias.join(".")).is_err());
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o770))?;
    assert!(pack.retain(&dir).is_err());
    assert!(fs::read_dir(&dir)?.next().is_none());

    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
    let json_path = dir.join(format!("{}.json", pack.fact_sha256));
    fs::File::create(&json_path)?.set_len(MAX_LEGACY_FILE_BYTES + 1)?;
    assert!(pack.retain(&dir).is_err());
    assert!(read_retained(&dir, &pack.fact_sha256).is_err());
    assert_eq!(fs::metadata(json_path)?.len(), MAX_LEGACY_FILE_BYTES + 1);
    assert_eq!(fs::read_dir(&dir)?.count(), 1);
    Ok(())
}

#[test]
fn json_digest_conflict_is_not_replaced_and_blocks_summary() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut store = StateStore::open(":memory:")?;
    let pack = FactPack::compile(store.briefing_facts()?)?;
    let json_path = dir.path().join(format!("{}.json", pack.fact_sha256));
    let mut conflict = serde_json::to_value(&pack)?;
    conflict["event_to_inclusive"] = json!(pack.to_seq() + 1);
    let conflicting_bytes = serde_json::to_vec_pretty(&conflict)?;
    fs::write(&json_path, &conflicting_bytes)?;

    let err = pack.retain(dir.path()).unwrap_err();
    assert!(format!("{err:#}").contains("retained digest mismatch"));
    assert_eq!(fs::read(json_path)?, conflicting_bytes);
    assert_eq!(fs::read_dir(dir.path())?.count(), 1);
    Ok(())
}

#[test]
fn serialization_rejects_oversized_pack_before_publishing() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut store = StateStore::open(":memory:")?;
    let mut pack = FactPack::compile(store.briefing_facts()?)?;
    pack.body.to_git_commit = Some("\x01".repeat(MAX_PACK_BYTES / 6 + 1));
    let err = pack.retain(dir.path()).unwrap_err();
    assert!(err.to_string().contains("compact byte budget"));
    assert!(fs::read_dir(dir.path())?.next().is_none());
    Ok(())
}

#[test]
fn simultaneous_matching_publications_converge_without_staging_files() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut store = StateStore::open(":memory:")?;
    let pack = FactPack::compile(store.briefing_facts()?)?;
    let start = std::sync::Barrier::new(4);
    std::thread::scope(|scope| -> Result<()> {
        let attempts: Vec<_> = (0..4)
            .map(|_| {
                scope.spawn(|| {
                    start.wait();
                    pack.retain(dir.path())
                })
            })
            .collect();
        for attempt in attempts {
            attempt.join().expect("retention thread panicked")?;
        }
        Ok(())
    })?;
    assert_eq!(
        read_retained(dir.path(), &pack.fact_sha256)?,
        Some(serde_json::to_value(&pack)?)
    );
    assert_eq!(fs::read_dir(dir.path())?.count(), 2);
    Ok(())
}
