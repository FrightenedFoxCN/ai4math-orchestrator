use super::*;
use crate::{init, managed_tests::git};
use ai4math_core::Engine;
use ai4math_store::{Change, retained::hash_fact};
use serde_json::Value;
use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf};

const MODULE: &str = "Spec.lean";
const REASON: &str = "Keep the current definition; validate two independent probes later.";
const B: &str = "B1";

fn setup(root: &Path) -> Result<PathBuf> {
    init(root, Engine::Lean4)?;
    let source = root.join("source");
    fs::create_dir(&source)?;
    git(&source, &["init", "--initial-branch=main"]);
    fs::write(source.join(MODULE), "def target : Prop := True\n")?;
    git(&source, &["add", MODULE]);
    git(&source, &["commit", "-m", "frozen spec"]);
    git(root, &["clone", "--bare", "source", "repo.git"]);
    let repo = root.join("repo.git");
    crate::enroll_claim(
        root,
        "CriticalDefinition",
        MODULE,
        "target",
        "Definition",
        "Registered target",
        true,
    )?;
    trip(
        root,
        &repo,
        B,
        MODULE,
        "target",
        "Review reported definition smell",
        true,
    )?;
    Ok(repo)
}

fn retain(root: &Path, mut facts: Value) -> Result<String> {
    let dir = root.join(".runtime/briefings");
    fs::create_dir_all(&dir)?;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
    let id = hash_fact(&facts)?;
    facts["fact_sha256"] = json!(&id);
    let path = dir.join(format!("{id}.json"));
    fs::write(&path, serde_json::to_vec(&facts)?)?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    Ok(id)
}

fn fixture_event(store: &mut StateStore) -> Result<i64> {
    store.append_event(
        Change {
            actor: "fixture",
            git_commit: None,
        },
        "NOTE_RECORDED",
        "system",
        "example",
        json!({}),
    )
}

#[test]
fn post_cas_recovery_preserves_unreviewed_events_and_finalizes_once() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let root = temp.path();
    let repo = setup(root)?;
    trip(
        root,
        &repo,
        "B2",
        MODULE,
        "a_second_target",
        "A separate pending decision",
        false,
    )?;
    let state = root.join(".runtime/state.sqlite");
    let mut store = StateStore::open(&state)?;
    let facts = store.briefing_for_alerts()?;
    assert_eq!(facts.alert_sources.len(), 1);
    assert_eq!(
        facts.alert_sources[0].event_type,
        "CRITICAL_BREAKER_TRIPPED"
    );
    let id = retain(root, serde_json::to_value(&facts)?)?;
    preview(root, &repo, &id, B, REASON, PivotAction::ResumeAsIs)?;
    let reviewed = store.status()?.last_event_seq;
    let intent = {
        let repo = PromotionRepo::lock(&repo)?;
        let p = plan(
            root,
            &repo,
            &mut store,
            &id,
            B,
            REASON,
            Selection::just(PivotAction::ResumeAsIs),
        )?;
        let pin = repo.pivot_pending_ref(&state)?;
        let intent = prepare(&mut store, &repo, &pin, p)?;
        git(repo.path(), &["gc", "--prune=now"]);
        repo.advance_adr(
            &intent.plan.base_commit,
            &intent.new_commit,
            &intent.adr_id,
            &markdown(&intent.plan)?,
        )?;
        assert_eq!(store.status()?.last_event_seq, reviewed);
        intent
    }; // Simulate exit after Git CAS, before committing SQLite.
    let unseen = fixture_event(&mut store)?;
    assert!(abandon(root, &repo, &intent.adr_id).is_err());
    recover(root, &repo)?;
    let (_, result) = store.recorded_pivot(&intent.adr_id)?.unwrap();
    assert!(!result.probes_started);
    assert_eq!(result.breaker_state, "HALF_OPEN");
    assert_eq!(result.reviewed_through, reviewed);
    assert_eq!(
        git(&repo, &["rev-parse", "refs/heads/main"]),
        intent.new_commit
    );
    let actual = git(
        &repo,
        &[
            "show",
            &format!("{}:docs/adr/{}.md", intent.new_commit, intent.adr_id),
        ],
    );
    assert_eq!(actual, markdown(&intent.plan)?.trim());
    let after = store.briefing_facts()?;
    assert_eq!(after.checkpoint.unwrap().event_seq, reviewed);
    assert!(after.events.items.iter().any(|e| e.event_seq == unseen));
    assert_eq!(
        after
            .breakers
            .items
            .iter()
            .find(|x| x.breaker_id == B)
            .unwrap()
            .state,
        "HALF_OPEN"
    );
    assert_eq!(
        after
            .breakers
            .items
            .iter()
            .find(|x| x.breaker_id == "B2")
            .unwrap()
            .state,
        "TRIPPED"
    );
    assert_eq!(
        after
            .inbox
            .items
            .iter()
            .map(|x| x.inbox_id.as_str())
            .collect::<Vec<_>>(),
        ["DEF-B2"]
    );
    let end = store.status()?.last_event_seq;
    assert!(
        apply(
            root,
            &repo,
            &id,
            B,
            "changed reason",
            &intent.adr_id,
            PivotAction::ResumeAsIs
        )
        .is_err()
    );
    apply(
        root,
        &repo,
        &id,
        B,
        REASON,
        &intent.adr_id,
        PivotAction::ResumeAsIs,
    )?;
    recover(root, &repo)?;
    assert_eq!(end, store.status()?.last_event_seq);
    let git = PromotionRepo::lock(&repo)?;
    assert!(git.pivot_pending_ref(&state)?.current()?.is_none());
    // Mathematical snapshot stays usable after the additive ADR lands.
    assert_eq!(git.snapshot(MODULE, None)?.project()?.len(), 1);
    Ok(())
}

#[test]
fn stale_unadopted_plan_cannot_land_and_requires_fresh_human_review() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let root = temp.path();
    let repo = setup(root)?;
    let state = root.join(".runtime/state.sqlite");
    let mut store = StateStore::open(&state)?;
    let mut omitted = serde_json::to_value(store.briefing_facts()?)?;
    omitted["inbox"]["items"] = json!([]);
    omitted["inbox"]["truncated"] = json!(true);
    let omitted = retain(root, omitted)?;
    assert!(
        preview(root, &repo, &omitted, B, REASON, PivotAction::ResumeAsIs)
            .unwrap_err()
            .to_string()
            .contains("both appear")
    );
    let facts = retain(root, serde_json::to_value(store.briefing_facts()?)?)?;
    let intent = {
        let git = PromotionRepo::lock(&repo)?;
        let p = plan(
            root,
            &git,
            &mut store,
            &facts,
            B,
            REASON,
            Selection::just(PivotAction::ResumeAsIs),
        )?;
        let pin = git.pivot_pending_ref(&state)?;
        prepare(&mut store, &git, &pin, p)?
    };
    fixture_event(&mut store)?;
    assert!(format!("{:#}", recover(root, &repo).unwrap_err()).contains("ledger changed"));
    assert_eq!(
        git(&repo, &["rev-parse", "refs/heads/main"]),
        intent.plan.base_commit
    );
    assert!(store.recorded_pivot(&intent.adr_id)?.is_none());
    abandon(root, &repo, &intent.adr_id)?;
    assert!(store.pivot_intent()?.is_none());
    assert!(preview(root, &repo, &facts, B, REASON, PivotAction::ResumeAsIs).is_err());
    let fresh = retain(root, serde_json::to_value(store.briefing_facts()?)?)?;
    let approved = {
        let git = PromotionRepo::lock(&repo)?;
        plan(
            root,
            &git,
            &mut store,
            &fresh,
            B,
            REASON,
            Selection::just(PivotAction::ResumeAsIs),
        )?
        .adr_id()?
    };
    assert!(
        apply(
            root,
            &repo,
            &fresh,
            B,
            REASON,
            &intent.adr_id,
            PivotAction::ResumeAsIs
        )
        .is_err()
    );
    apply(
        root,
        &repo,
        &fresh,
        B,
        REASON,
        &approved,
        PivotAction::ResumeAsIs,
    )?;
    assert!(store.recorded_pivot(&approved)?.is_some());
    Ok(())
}

#[test]
fn main_movement_between_prepare_and_recovery_requires_no_state_change() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let root = temp.path();
    let repo = setup(root)?;
    let state = root.join(".runtime/state.sqlite");
    let mut store = StateStore::open(&state)?;
    let facts = retain(root, serde_json::to_value(store.briefing_facts()?)?)?;
    let before = store.status()?.last_event_seq;
    let intent = {
        let git = PromotionRepo::lock(&repo)?;
        let p = plan(
            root,
            &git,
            &mut store,
            &facts,
            B,
            REASON,
            Selection::just(PivotAction::ResumeAsIs),
        )?;
        let pin = git.pivot_pending_ref(&state)?;
        prepare(&mut store, &git, &pin, p)?
    };
    let source = root.join("source");
    git(
        &source,
        &[
            "commit",
            "--allow-empty",
            "-m",
            "move main without landing ADR",
        ],
    );
    git(
        &source,
        &["push", repo.to_str().unwrap(), "HEAD:refs/heads/main"],
    );
    assert!(
        recover(root, &repo)
            .unwrap_err()
            .to_string()
            .contains("main moved")
    );
    assert_eq!(before, store.status()?.last_event_seq);
    assert_eq!(store.pivot_intent()?, Some(intent.clone()));
    assert!(store.recorded_pivot(&intent.adr_id)?.is_none());
    abandon(root, &repo, &intent.adr_id)?;
    assert!(preview(root, &repo, &facts, B, REASON, PivotAction::ResumeAsIs).is_err());
    Ok(())
}

#[test]
fn archive_recovery_validates_adr_and_keeps_scope_gated_after_git_cas() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let root = temp.path();
    let repo = setup(root)?;
    let state = root.join(".runtime/state.sqlite");
    let sentry = root.join("source/unfinished-work.txt");
    fs::write(&sentry, b"unfinished work is not ours to move\n")?;
    let (intent, before, unreviewed) = {
        let mut store = StateStore::open(&state)?;
        let fact = retain(root, serde_json::to_value(store.briefing_facts()?)?)?;
        let git = PromotionRepo::lock(&repo)?;
        let resume = plan(
            root,
            &git,
            &mut store,
            &fact,
            B,
            REASON,
            Selection::just(PivotAction::ResumeAsIs),
        )?;
        let archive = plan(
            root,
            &git,
            &mut store,
            &fact,
            B,
            REASON,
            Selection::just(PivotAction::ArchiveBreaker),
        )?;
        assert_ne!(resume.adr_id()?, archive.adr_id()?);
        assert_eq!(git.main_head()?, archive.base_commit);
        assert!(store.pivot_intent()?.is_none());
        let before = serde_json::to_value(store.claim("CriticalDefinition")?)?;
        let pin = git.pivot_pending_ref(&state)?;
        let intent = prepare(&mut store, &git, &pin, archive)?;
        git.advance_adr(
            &intent.plan.base_commit,
            &intent.new_commit,
            &intent.adr_id,
            &markdown(&intent.plan)?,
        )?;
        assert_eq!(store.live_telemetry()?.archived_breakers, 0);
        assert_eq!(store.status()?.last_event_seq, intent.plan.reviewed_through);
        let unreviewed = fixture_event(&mut store)?;
        (intent, before, unreviewed)
    }; // Handles drop here, reproducing startup from a committed Git CAS.
    assert!(abandon(root, &repo, &intent.adr_id).is_err());
    recover(root, &repo)?;
    let mut store = StateStore::open(&state)?;
    let (_, result) = store
        .recorded_pivot(&intent.adr_id)?
        .context("missing archive receipt")?;
    assert_eq!(result.git_commit, intent.new_commit);
    assert_eq!(result.breaker_state, "ARCHIVED");
    assert!(!result.probes_started);
    assert!(store.module_breaker_gated("lean4", MODULE)?);
    assert!(
        store
            .breaker_probe_scope(repo.to_str().unwrap(), B)
            .is_err()
    );
    assert!(store.pivot_intent()?.is_none());
    let after = store.briefing_facts()?;
    assert_eq!(
        after.checkpoint.as_ref().unwrap().event_seq,
        intent.plan.reviewed_through
    );
    assert!(after.events.items.iter().any(|e| e.event_seq == unreviewed));
    assert!(
        after
            .inbox
            .items
            .iter()
            .all(|i| i.inbox_id != intent.plan.inbox_id)
    );
    assert_eq!(
        serde_json::to_value(store.claim("CriticalDefinition")?)?,
        before
    );
    assert_eq!(fs::read(&sentry)?, b"unfinished work is not ours to move\n");
    let changes = git(
        &repo,
        &[
            "diff-tree",
            "--no-commit-id",
            "--name-only",
            "-r",
            &intent.new_commit,
        ],
    );
    assert_eq!(changes, format!("docs/adr/{}.md", intent.adr_id));
    let latest = store.status()?.last_event_seq;
    assert!(
        apply(
            root,
            &repo,
            &intent.plan.fact_sha256,
            B,
            REASON,
            &intent.adr_id,
            PivotAction::ResumeAsIs
        )
        .is_err()
    );
    apply(
        root,
        &repo,
        &intent.plan.fact_sha256,
        B,
        REASON,
        &intent.adr_id,
        PivotAction::ArchiveBreaker,
    )?;
    recover(root, &repo)?;
    assert_eq!(store.status()?.last_event_seq, latest);
    Ok(())
}

#[test]
fn approved_route_batch_recovers_after_git_cas_and_replays_exact_ids() -> Result<()> {
    use ai4math_store::{NewPlan, PlanStatus};
    let temp = tempfile::tempdir()?;
    let root = temp.path();
    let repo = setup(root)?;
    let db = root.join(".runtime/state.sqlite");
    let approved_plans = ["B-route", "A-route"];
    let (intent, untouched_event) = {
        let mut state = StateStore::open(&db)?;
        let git = PromotionRepo::lock(&repo)?;
        let base = git.main_head()?;
        for id in ["A-route", "B-route", "Untouched-route"] {
            state.create_plan(
                NewPlan {
                    id,
                    target: "CriticalDefinition",
                    name: id,
                    track: "exploration",
                    status: PlanStatus::Active,
                    dependencies: &[],
                },
                Change {
                    actor: "fixture",
                    git_commit: Some(&base),
                },
            )?;
        }
        let fact = retain(root, serde_json::to_value(state.briefing_facts()?)?)?;
        let choose = |plan_ids| Selection {
            action: PivotAction::ArchiveRoutes,
            plan_ids,
        };
        let p = plan(
            root,
            &git,
            &mut state,
            &fact,
            B,
            REASON,
            choose(&approved_plans),
        )?;
        let reversed = plan(
            root,
            &git,
            &mut state,
            &fact,
            B,
            REASON,
            choose(&["A-route", "B-route"]),
        )?;
        assert_eq!(p.adr_id()?, reversed.adr_id()?);
        assert_eq!(
            p.archive_plan_ids().collect::<Vec<_>>(),
            ["A-route", "B-route"]
        );
        let md = markdown(&p)?;
        assert!(md.contains("ARCHIVE_ROUTES") && md.contains("A-route") && md.contains("B-route"));
        assert!(!md.contains("Untouched-route"));
        let pin = git.pivot_pending_ref(&db)?;
        let intent = prepare(&mut state, &git, &pin, p)?;
        git.advance_adr(&base, &intent.new_commit, &intent.adr_id, &md)?;
        assert_eq!(state.plan_state("A-route")?.unwrap().0, "ACTIVE");
        let untouched_event = fixture_event(&mut state)?;
        (intent, untouched_event)
    };
    recover(root, &repo)?;
    let mut state = StateStore::open(&db)?;
    let report = state.audit_graph()?;
    assert!(report.consistent, "{report:#?}");
    for id in approved_plans {
        let old = state
            .replay_plan(id, Some(intent.plan.reviewed_through))?
            .plan
            .context("missing historical route")?;
        let new = state
            .replay_plan(id, None)?
            .plan
            .context("missing archived route")?;
        assert_eq!(old.durable_status, "ACTIVE");
        assert_eq!(new.durable_status, "ARCHIVED_BY_ADR");
        assert_eq!(new.archived_by_adr.as_deref(), Some(intent.adr_id.as_str()));
    }
    assert_eq!(state.plan_state("Untouched-route")?.unwrap().0, "ACTIVE");
    assert!(
        state
            .briefing_facts()?
            .events
            .items
            .iter()
            .any(|e| e.event_seq == untouched_event)
    );
    let done = state.status()?.last_event_seq;
    for wrong in [
        vec!["A-route"],
        vec!["A-route", "Untouched-route"],
        vec!["A-route", "A-route"],
    ] {
        assert!(
            apply_routes(
                root,
                &repo,
                &intent.plan.fact_sha256,
                B,
                REASON,
                &intent.adr_id,
                &wrong
            )
            .is_err()
        );
    }
    apply_routes(
        root,
        &repo,
        &intent.plan.fact_sha256,
        B,
        REASON,
        &intent.adr_id,
        &approved_plans,
    )?;
    assert_eq!(state.status()?.last_event_seq, done);
    Ok(())
}
