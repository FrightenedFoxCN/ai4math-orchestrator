use super::*;
use crate::{NewClaim, PromotionIntent, Trust, VerifiedReceipt};

const REPO: &str = "/managed-math";
const CLAIM: &str = "claim-a";
const BASE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const NEXT_BASE: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const COMPILER: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const OTHER_COMPILER: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const T: i64 = 1_000_000;

fn enroll(store: &mut StateStore, id: &str) -> Result<()> {
    store.create_claim(
        NewClaim {
            id,
            title: "Fixture",
            informal_statement: "fixture",
            engine: "lean4",
            formal_target: Some("Target.lean#proof"),
            module_path: Some("Target.lean"),
            critical: false,
        },
        Change {
            actor: "fixture",
            git_commit: Some(BASE),
        },
    )
}

fn setup(store: &mut StateStore) -> Result<()> {
    enroll(store, CLAIM)?;
    store.bind_task_repository(REPO, BASE)
}

fn fields(candidate: &str) -> FailureFields<'_> {
    FailureFields {
        base_commit: BASE,
        engine: Engine::Lean4,
        target_module: "Target.lean",
        target_decl: "proof",
        failed_module: "Dependent.lean",
        candidate_sha256: candidate,
        compiler_sha256: COMPILER,
    }
}

// A private scalar fixture runs the actual transaction; no test constructor
// for the public broker value is needed or exposed.
fn record(store: &mut StateStore, claim: &str, candidate: u32, time: i64) -> Result<Option<i64>> {
    let candidate = format!("{candidate:064x}");
    store.write(|tx| record_failure_at(tx, REPO, claim, fields(&candidate), time))
}

fn incident_count(store: &StateStore) -> Result<i64> {
    Ok(store.db.query_row(
        "SELECT count(*) FROM managed_regression_incidents",
        [],
        |r| r.get(0),
    )?)
}

#[test]
fn distinct_candidates_and_retries_emit_one_safe_event_across_restarts() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("state.sqlite");
    let mut store = StateStore::open(&path)?;
    setup(&mut store)?;
    let before = store.status()?.last_event_seq;
    assert_eq!(record(&mut store, CLAIM, 1, T)?, None);
    assert_eq!(record(&mut store, CLAIM, 1, T + 1)?, None);
    assert_eq!(record(&mut store, CLAIM, 2, T + 2)?, None);
    assert_eq!(record(&mut store, CLAIM, 2, T + 3)?, None);
    assert_eq!(store.status()?.last_event_seq, before);
    assert!(!store.has_alert_candidates(before)?);
    drop(store);

    let mut store = StateStore::open(&path)?;
    let seq = record(&mut store, CLAIM, 3, T + WINDOW_MS - 1)?.unwrap();
    assert_eq!(seq, before + 1);
    assert!(store.has_alert_candidates(seq)?);
    let events = store.events_after(before, 5)?;
    let emitted = &events[0];
    assert_eq!(emitted.event_type, MELTDOWN);
    assert_eq!(emitted.entity_type, "claim");
    assert_eq!(emitted.entity_id, CLAIM);
    assert_eq!(emitted.git_commit.as_deref(), Some(BASE));
    assert_eq!(emitted.actor, "broker");
    assert_eq!(
        emitted.payload,
        json!({"failed_module":"Dependent.lean","distinct_candidates":3,"window_seconds":900})
    );

    let facts = store.briefing_for_alerts()?;
    assert_eq!(facts.alert_sources[0].payload, emitted.payload);
    assert_eq!(facts.risk_radar.events.items[0].event_seq, seq);
    let claim = store.claim(CLAIM)?.unwrap();
    assert_eq!(claim.local_trust, Trust::L0);
    assert_eq!(claim.effective_trust, Trust::L0);
    assert_eq!(claim.receipt_id, None);
    assert!(!claim.stale && !claim.effective_stale);
    drop(store);

    let mut store = StateStore::open(&path)?;
    for candidate in [1, 4, 5, 6] {
        assert_eq!(
            record(&mut store, CLAIM, candidate, T + 3 * WINDOW_MS)?,
            None
        );
    }
    assert!(store.events_after(seq, 5)?.is_empty());
    assert_eq!(incident_count(&store)?, 1);
    let pending: bool = store.db.query_row(
        "SELECT first_sha256 IS NOT NULL OR second_sha256 IS NOT NULL FROM managed_regression_incidents", [], |r| r.get(0),
    )?;
    assert!(
        !pending,
        "an emitted latch no longer needs candidate hashes"
    );
    Ok(())
}

#[test]
fn pending_episode_expires_without_replay_extending_the_window() -> Result<()> {
    let mut store = StateStore::open(":memory:")?;
    setup(&mut store)?;
    let before = store.status()?.last_event_seq;
    assert_eq!(record(&mut store, CLAIM, 1, T)?, None);
    assert_eq!(record(&mut store, CLAIM, 2, T + 1)?, None);
    assert_eq!(record(&mut store, CLAIM, 1, T + WINDOW_MS - 1)?, None);
    assert_eq!(record(&mut store, CLAIM, 3, T + WINDOW_MS)?, None);
    assert_eq!(record(&mut store, CLAIM, 4, T + WINDOW_MS + 1)?, None);
    assert_eq!(store.status()?.last_event_seq, before);
    assert_eq!(
        record(&mut store, CLAIM, 5, T + WINDOW_MS + 2)?,
        Some(before + 1)
    );
    Ok(())
}

#[test]
fn exact_contexts_do_not_mix_and_only_last_observed_base_is_retained() -> Result<()> {
    let mut store = StateStore::open(":memory:")?;
    setup(&mut store)?;
    enroll(&mut store, "claim-b")?;
    let before = store.status()?.last_event_seq;
    let one = format!("{:064x}", 1);
    let two = format!("{:064x}", 2);
    let three = format!("{:064x}", 3);
    assert_eq!(record(&mut store, CLAIM, 1, T)?, None);
    assert_eq!(record(&mut store, "claim-b", 2, T + 1)?, None);
    for variant in [
        FailureFields {
            compiler_sha256: OTHER_COMPILER,
            ..fields(&two)
        },
        FailureFields {
            failed_module: "Other.lean",
            ..fields(&two)
        },
    ] {
        assert_eq!(
            store.write(|tx| record_failure_at(tx, REPO, CLAIM, variant, T + 2))?,
            None
        );
    }
    store.db.execute(
        "UPDATE claims SET formal_target='Target.lean#other' WHERE claim_id=?1",
        [CLAIM],
    )?;
    assert_eq!(
        store.write(|tx| record_failure_at(
            tx,
            REPO,
            CLAIM,
            FailureFields {
                target_decl: "other",
                ..fields(&two)
            },
            T + 3
        ))?,
        None
    );
    store.db.execute(
        "UPDATE claims SET formal_target='Target.lean#proof' WHERE claim_id=?1",
        [CLAIM],
    )?;
    assert_eq!(record(&mut store, CLAIM, 3, T + 4)?, None);
    assert_eq!(store.status()?.last_event_seq, before);
    assert_eq!(record(&mut store, CLAIM, 2, T + 5)?, Some(before + 1));
    assert_eq!(incident_count(&store)?, 5);

    for candidate in [&one, &two, &three] {
        let result = store.write(|tx| {
            record_failure_at(
                tx,
                REPO,
                CLAIM,
                FailureFields {
                    base_commit: NEXT_BASE,
                    ..fields(candidate)
                },
                T + 6,
            )
        })?;
        assert_eq!(result, (candidate == &three).then_some(before + 2));
    }
    assert_eq!(incident_count(&store)?, 1);
    assert_eq!(
        record(&mut store, CLAIM, 1, T + 7)?,
        None,
        "an observed base transition begins a new generation"
    );
    assert_eq!(store.status()?.last_event_seq, before + 2);
    Ok(())
}

#[test]
fn invalid_contexts_and_generic_append_cannot_record_failures() -> Result<()> {
    let mut store = StateStore::open(":memory:")?;
    enroll(&mut store, CLAIM)?;
    let hash = format!("{:064x}", 1);
    let failure = fields(&hash);
    assert!(
        store
            .write(|tx| record_failure_at(tx, REPO, CLAIM, failure, T))
            .is_err()
    );
    store.bind_task_repository(REPO, BASE)?;
    let before = store.status()?.last_event_seq;
    assert!(
        store
            .append_event(
                Change {
                    actor: "broker",
                    git_commit: Some(BASE)
                },
                MELTDOWN,
                "claim",
                CLAIM,
                json!({})
            )
            .is_err()
    );
    assert!(
        store
            .write(|tx| record_failure_at(tx, "/other-repository", CLAIM, failure, T))
            .is_err()
    );
    assert!(
        store
            .write(|tx| record_failure_at(tx, REPO, "not-a-claim", failure, T))
            .is_err()
    );
    for invalid in [
        FailureFields {
            target_decl: "other",
            ..failure
        },
        FailureFields {
            target_module: "Other.lean",
            ..failure
        },
        FailureFields {
            engine: Engine::Agda,
            target_module: "Target.agda",
            failed_module: "Dependent.agda",
            ..failure
        },
        FailureFields {
            failed_module: "Target.lean",
            ..failure
        },
        FailureFields {
            failed_module: "../unsafe.lean",
            ..failure
        },
        FailureFields {
            candidate_sha256: "not-a-fingerprint",
            ..failure
        },
    ] {
        assert!(
            store
                .write(|tx| record_failure_at(tx, REPO, CLAIM, invalid, T))
                .is_err()
        );
    }
    for update in [
        "stale=1",
        "effective_stale=1",
        "local_trust_level='L2b'",
        "effective_trust_level='L5'",
        "formal_target='proof'",
        "module_path=NULL",
    ] {
        let tx = store.db.transaction()?;
        tx.execute(
            &format!("UPDATE claims SET {update} WHERE claim_id=?1"),
            [CLAIM],
        )?;
        assert!(
            record_failure_at(&tx, REPO, CLAIM, failure, T).is_err(),
            "{update}"
        );
        tx.rollback()?;
    }
    for (sql, args) in [
        (
            "INSERT INTO promotion_intent VALUES(1,?1,?2,?3,?4,'Target.lean','proof','')",
            &[REPO, CLAIM, BASE, NEXT_BASE][..],
        ),
        (
            "INSERT INTO pivot_resume_intent VALUES(1,'ADR','{}',?1)",
            &[NEXT_BASE][..],
        ),
    ] {
        let tx = store.db.transaction()?;
        tx.execute(sql, rusqlite::params_from_iter(args))?;
        assert!(record_failure_at(&tx, REPO, CLAIM, failure, T).is_err());
        tx.rollback()?;
    }
    assert_eq!(incident_count(&store)?, 0);
    assert_eq!(store.status()?.last_event_seq, before);
    Ok(())
}

#[test]
fn legacy_event_still_projects_but_untrusted_payload_fields_are_omitted() -> Result<()> {
    let mut store = StateStore::open(":memory:")?;
    setup(&mut store)?;
    let seq = store.write(|tx| event(
        tx, Change { actor: "historical", git_commit: Some(BASE) }, MELTDOWN, "claim", CLAIM,
        json!({"failed_module":"/home/private/proof.lean","distinct_candidates":4,"window_seconds":"900","log":"secret","proof_body":"secret"}),
    ))?;
    let facts = store.briefing_for_alerts()?;
    assert_eq!(facts.alert_sources[0].event_seq, seq);
    assert_eq!(facts.alert_sources[0].event_type, MELTDOWN);
    assert_eq!(
        facts.alert_sources[0].payload,
        json!({"payload_omitted":true})
    );
    Ok(())
}

#[test]
fn successful_finalization_atomically_advances_incident_base() -> Result<()> {
    let mut store = StateStore::open(":memory:")?;
    setup(&mut store)?;
    assert_eq!(record(&mut store, CLAIM, 1, T)?, None);
    assert_eq!(incident_count(&store)?, 1);
    let intent = PromotionIntent {
        repo: REPO.into(),
        claim_id: CLAIM.into(),
        base_commit: BASE.into(),
        new_commit: NEXT_BASE.into(),
        file_path: "Target.lean".into(),
        target_decl: "proof".into(),
        proof_body: "trivial".into(),
    };
    let receipt = VerifiedReceipt {
        id: "receipt".into(),
        issued_at: "2026-01-01T00:00:00Z".into(),
        tier: "T1_PROMOTION".into(),
        engine: "lean4".into(),
        base_commit: BASE.into(),
        patch_sha256: COMPILER.into(),
        target_decl: "proof".into(),
        file_path: "Target.lean".into(),
        allowlist_passed: true,
        axioms_used: vec![],
        external_checker_passed: None,
        compile_seconds: 0.0,
        max_heartbeats: None,
        receipt_sha256: OTHER_COMPILER.into(),
        raw: json!({}),
        regression_checked: true,
        broker_assessed_t3: false,
        lean_comparison: None,
    };
    store.prepare_promotion(&intent, &receipt)?;
    assert_eq!(incident_count(&store)?, 1);
    let seq = store.finish_promotion(&intent, &receipt)?;
    assert_eq!(store.claim(CLAIM)?.unwrap().local_trust, Trust::L5);
    assert_eq!(incident_count(&store)?, 0);
    let base: String =
        store
            .db
            .query_row("SELECT base_commit FROM managed_regression_base", [], |r| {
                r.get(0)
            })?;
    assert_eq!(base, NEXT_BASE);
    assert_eq!(store.finish_promotion(&intent, &receipt)?, seq);
    Ok(())
}
