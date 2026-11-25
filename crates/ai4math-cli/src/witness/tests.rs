use super::*;
use crate::{enroll_claim, init, managed_tests::git};
use ai4math_store::{Change, StateStore, Trust, Wiki, WikiQuery, WikiStratum};
use serde_json::json;
use std::{env, fs};

#[test]
fn refutation_requires_project_primary_lean() -> Result<()> {
    let root = tempfile::tempdir()?;
    init(root.path(), Engine::Agda)?;
    let path = Path::new("unused");
    let err =
        verify_and_record(root.path(), path, "C", "Spec.lean", "target", path, path).unwrap_err();
    assert!(err.to_string().contains("primary engine"));
    Ok(())
}

#[test]
#[cfg(target_os = "macos")]
#[ignore = "requires native Lean outside an outer Seatbelt sandbox"]
fn native_witness_is_archived_before_refutation_and_p0() -> Result<()> {
    let root = tempfile::tempdir()?;
    let root = root.path();
    init(root, Engine::Lean4)?;
    let source = root.join("source");
    fs::create_dir(&source)?;
    git(&source, &["init", "--initial-branch=main"]);
    fs::write(
        source.join("Spec.lean"),
        "def target : Prop := False\ndef other : Prop := True\n",
    )?;
    git(&source, &["add", "Spec.lean"]);
    git(
        &source,
        &["commit", "-m", "register immutable propositions"],
    );
    let base = git(&source, &["rev-parse", "HEAD"]);
    git(root, &["clone", "--bare", "source", "repo.git"]);
    let repo = root.join("repo.git");
    enroll_claim(root, "C", "Spec.lean", "target", "Claim", "False", true)?;
    enroll_claim(root, "T", "Spec.lean", "other", "True Claim", "true", true)?;
    let exe = env::var_os("AI4MATH_TEST_LEAN").context("set native Lean executable")?;
    let body = root.join("proof.txt");
    fs::write(&body, "fun h => h")?;
    assert!(
        verify_and_record(
            root,
            &repo,
            "C",
            "Spec.lean",
            "target",
            Path::new(&exe),
            &body
        )
        .unwrap_err()
        .to_string()
        .contains("bind-witness-spec")
    );
    bind(root, &repo, "C", "Spec.lean", "target", &base)?;
    bind(root, &repo, "T", "Spec.lean", "other", &base)?;
    let mut db = StateStore::open(root.join(".runtime/state.sqlite"))?;
    let change = Change {
        actor: "fixture",
        git_commit: Some(&base),
    };
    db.set_local_trust("C", Trust::L2a, change)?;
    // Keep a viable alternate route so only the directly refuted critical Claim raises P0.
    for (id, dependencies) in [
        ("dependent", &[("C", "lemma")][..]),
        ("alternative", &[][..]),
    ] {
        db.create_plan(
            ai4math_store::NewPlan {
                id,
                name: id,
                target: "T",
                track: "infrastructure",
                status: ai4math_store::PlanStatus::Active,
                dependencies,
            },
            change,
        )?;
    }
    assert!(
        db.append_event(change, "CRITICAL_CLAIM_REFUTED", "claim", "C", json!({}))
            .is_err()
    );
    let seq = db.status()?.last_event_seq;
    let before_check = db.check_graph()?;
    assert!(before_check.consistent, "{before_check:?}");
    assert_eq!(before_check.ledger_event_seq, seq);
    assert!(
        verify_and_record(
            root,
            &repo,
            "T",
            "Spec.lean",
            "other",
            Path::new(&exe),
            &body
        )
        .is_err()
    );
    assert!(
        verify_and_record(
            root,
            &repo,
            "T",
            "Spec.lean",
            "target",
            Path::new(&exe),
            &body
        )
        .is_err()
    );
    let archive = root.join(".mathwiki/graveyard");
    fs::remove_dir(&archive)?;
    assert!(
        verify_and_record(
            root,
            &repo,
            "C",
            "Spec.lean",
            "target",
            Path::new(&exe),
            &body
        )
        .is_err()
    );
    assert_eq!(db.claim("C")?.unwrap().local_trust, Trust::L2a);
    assert_eq!(db.status()?.last_event_seq, seq);
    assert_eq!(db.status()?.pending_decisions, 0);
    fs::create_dir(&archive)?;
    let res = verify_and_record(
        root,
        &repo,
        "C",
        "Spec.lean",
        "target",
        Path::new(&exe),
        &body,
    )?;
    let entry = archive.join(format!("{}.md", res.wiki_id));
    assert!(entry.is_file());
    let archived = fs::read_to_string(entry)?;
    assert!(archived.contains(&base) && archived.contains(&res.witness_sha256));
    assert_eq!(db.claim("C")?.unwrap().local_trust, Trust::L2b);
    assert_eq!(db.claim("T")?.unwrap().local_trust, Trust::L0);
    let mark = db.status()?.last_event_seq;
    let earlier = db
        .replay_plan("dependent", Some(seq))?
        .plan
        .context("missing historical plan")?;
    let current = db
        .replay_plan("dependent", None)?
        .plan
        .context("missing invalidated plan")?;
    assert_eq!(
        (
            earlier.durable_status,
            earlier.status,
            earlier.refuted_claim_id
        ),
        ("ACTIVE", "ACTIVE", None)
    );
    assert_eq!(
        (
            current.durable_status,
            current.status,
            current.refuted_claim_id.as_deref()
        ),
        (
            "INVALIDATED_BY_REFUTATION",
            "INVALIDATED_BY_REFUTATION",
            Some("C")
        )
    );
    assert_eq!(current.blocking_claim_id, None);
    assert_eq!(current.effective_trust, Trust::L0);
    assert!((seq + 1..=mark).contains(&current.last_event_seq));
    assert_eq!(
        db.plan_state("dependent")?,
        Some((current.status.into(), current.effective_trust))
    );
    assert_eq!(
        db.replay_plan("alternative", None)?
            .plan
            .context("missing alternate plan")?
            .status,
        "ACTIVE"
    );
    let after_check = db.check_graph()?;
    assert!(after_check.consistent, "{after_check:?}");
    assert_eq!(after_check.ledger_event_seq, mark);
    assert_eq!(db.status()?.last_event_seq, mark);
    let retry = verify_and_record(
        root,
        &repo,
        "C",
        "Spec.lean",
        "target",
        Path::new(&exe),
        &body,
    )?;
    assert!(retry.already_recorded);
    assert_eq!(retry.event_seq, res.event_seq);
    assert_eq!(db.status()?.last_event_seq, mark);
    assert_eq!(db.status()?.pending_decisions, 1);
    assert_eq!(git(&repo, &["rev-parse", "refs/heads/main"]), base);
    let pack = db.briefing_for_alerts()?;
    assert_eq!(
        pack.refutations.items[0].graveyard_id.as_deref(),
        Some(res.wiki_id.as_str())
    );
    assert!(
        pack.alert_sources
            .iter()
            .any(|e| e.event_type == "CRITICAL_CLAIM_REFUTED" && e.entity_id == "C")
    );
    let mut wiki = Wiki::open(root.join(".mathwiki/.index.sqlite"))?;
    assert_eq!(wiki.rebuild(root.join(".mathwiki"))?, 1);
    let results = wiki.search(WikiQuery {
        scope: WikiStratum::Graveyard,
        query: "refutation",
        ..WikiQuery::default()
    })?;
    assert!(results.hits.iter().any(|hit| hit.summary.id == res.wiki_id));
    git(
        &source,
        &["commit", "--allow-empty", "-m", "advance approved snapshot"],
    );
    git(
        &source,
        &["push", repo.to_str().unwrap(), "HEAD:refs/heads/main"],
    );
    let new = git(&source, &["rev-parse", "HEAD"]);
    assert!(
        bind(root, &repo, "C", "Spec.lean", "target", &new)
            .unwrap_err()
            .to_string()
            .contains("immutable")
    );
    assert!(
        verify_and_record(
            root,
            &repo,
            "C",
            "Spec.lean",
            "target",
            Path::new(&exe),
            &body
        )
        .unwrap_err()
        .to_string()
        .contains("approved witness spec")
    );
    Ok(())
}
