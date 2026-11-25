use super::*;
const REPO: &str = "/managed/rfc.git";
const BASE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const MODULE: &str = "Basis.lean";

fn draft(id: &str) -> InfraRFC {
    InfraRFC {
        rfc_id: id.into(),
        requested_by_claim: "Consumer".into(),
        engine: Engine::Lean4,
        motivation: "Need a reusable boundary API.".into(),
        proposed_definitions: vec!["Basis.bound".into()],
        required_api_lemmas: vec!["Basis.bound_spec".into()],
        consumer_usage_sketch: "exact Basis.bound_spec\n-- sketch, not executed".into(),
        smoke_test_criteria: vec!["Check an actual boundary case".into()],
        status: InfraStatus::Draft,
    }
}
fn enroll(store: &mut StateStore) -> Result<()> {
    let change = Change {
        actor: "fixture",
        git_commit: Some(BASE),
    };
    store.bind_task_repository(REPO, BASE)?;
    store.create_claim(
        NewClaim {
            id: "Consumer",
            title: "Consumer",
            informal_statement: "uses the new basis",
            engine: "lean4",
            formal_target: Some("Consumer.lean#N.theorem"),
            module_path: Some("Consumer.lean"),
            critical: false,
        },
        change,
    )?;
    Ok(())
}

#[test]
fn git_callback_failure_rolls_back_events_and_approval_and_preserves_immutable_draft() -> Result<()>
{
    let temp = tempfile::tempdir()?;
    let db = temp.path().join("ledger.sqlite");
    let mut store = StateStore::open(&db)?;
    enroll(&mut store)?;
    let before = store.status()?.last_event_seq;
    let d = draft("RFC-draft");
    assert!(
        store
            .submit_infra_rfc(REPO, BASE, MODULE, &d, || anyhow::bail!("moved Git"))
            .is_err()
    );
    assert_eq!(store.status()?.last_event_seq, before);
    assert!(store.infra_rfc(REPO, &d.rfc_id)?.is_none());
    let r = store.submit_infra_rfc(REPO, BASE, MODULE, &d, || Ok(()))?;
    assert_eq!(r.status, InfraRfcStatus::Draft);
    assert_eq!(r.submitted_event_seq, before + 1);
    assert!(
        store
            .approve_infra_rfc(REPO, BASE, &d.rfc_id, &r.draft_sha256, || anyhow::bail!(
                "moved Git"
            ))
            .is_err()
    );
    assert_eq!(store.status()?.last_event_seq, before + 1);
    assert_eq!(store.infra_rfc(REPO, &d.rfc_id)?, Some(r.clone()));
    let mut changed = d.clone();
    changed.consumer_usage_sketch.push('!');
    assert!(
        store
            .submit_infra_rfc(REPO, BASE, MODULE, &changed, || Ok(()))
            .is_err()
    );
    assert!(
        store
            .append_event(
                Change {
                    actor: "bad",
                    git_commit: Some(BASE)
                },
                APPROVED,
                ENTITY,
                &d.rfc_id,
                json!({})
            )
            .is_err()
    );
    let a = store.approve_infra_rfc(REPO, BASE, &d.rfc_id, &r.draft_sha256, || Ok(()))?;
    assert_eq!(a.status, InfraRfcStatus::Approved);
    assert_eq!(a.draft, d);
    assert_eq!(a.approved_event_seq, Some(before + 2));
    assert_eq!(
        store.submit_infra_rfc(REPO, BASE, MODULE, &d, || Ok(()))?,
        a
    );
    assert_eq!(
        store.approve_infra_rfc(REPO, BASE, &d.rfc_id, &r.draft_sha256, || Ok(()))?,
        a
    );
    assert!(
        store
            .db
            .execute("UPDATE infra_rfcs SET draft_json='{}'", [])
            .is_err()
    );
    assert!(store.db.execute("DELETE FROM infra_rfcs", []).is_err());
    drop(store);
    let reopened = StateStore::open_readonly(&db)?;
    assert_eq!(reopened.infra_rfc(REPO, &d.rfc_id)?, Some(a));
    assert_eq!(reopened.claim("Consumer")?.unwrap().local_trust, Trust::L0);
    assert_eq!(reopened.status()?.last_event_seq, before + 2);
    assert!(reopened.infra_rfc("/unbound", &d.rfc_id).is_err());
    Ok(())
}

#[test]
fn concurrent_approval_obtains_one_sequence_and_same_exact_record() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let db = tmp.path().join("state.sqlite");
    let mut store = StateStore::open(&db)?;
    enroll(&mut store)?;
    let d = draft("RFC-once");
    let r = store.submit_infra_rfc(REPO, BASE, MODULE, &d, || Ok(()))?;
    std::thread::scope(|scope| -> Result<()> {
        let apply = || {
            StateStore::open(&db)?
                .approve_infra_rfc(REPO, BASE, &d.rfc_id, &r.draft_sha256, || Ok(()))
        };
        let a = scope.spawn(apply);
        let b = scope.spawn(apply);
        assert_eq!(a.join().unwrap()?, b.join().unwrap()?);
        Ok(())
    })?;
    assert_eq!(store.status()?.last_event_seq, r.submitted_event_seq + 1);
    Ok(())
}
