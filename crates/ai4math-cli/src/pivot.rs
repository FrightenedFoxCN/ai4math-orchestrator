//! Operator CLI only. Do not expose Pivot mutations through daemon RPC or agent tools.
use crate::{
    bound_engine, print_json,
    promotion::{PendingRef, PromotionRepo},
};
use ai4math_store::{
    PivotAction, PivotIntent, PivotLessonDraft, PivotPlan, PivotResult, StateStore,
    retained::read_retained_review,
};
use anyhow::{Context, Result, bail, ensure};
use serde_json::json;
use std::path::Path;

fn repo_name(git: &PromotionRepo) -> Result<&str> {
    git.path().to_str().context("repository path is not UTF-8")
}

pub(crate) fn trip(
    root: &Path,
    repo: &Path,
    id: &str,
    module: &str,
    target: &str,
    reason: &str,
    critical: bool,
) -> Result<()> {
    let engine = bound_engine(root, module)?;
    let git = PromotionRepo::lock(repo)?;
    let snap = git.snapshot(module, None)?;
    ensure!(
        git.main_head()? == snap.base(),
        "Git main changed while recording the breaker"
    );
    let mut store = StateStore::open(root.join(".runtime/state.sqlite"))?;
    let report = store.register_tripped_breaker_report(
        repo_name(&git)?,
        snap.base(),
        id,
        target,
        module,
        &engine.to_string(),
        reason,
        critical,
    )?;
    print_json(
        json!({"breaker_id":id,"event_seq":report.event_seq,"state":"TRIPPED",
            "critical_claim_count":report.critical_claim_count,
            "inbox_priority":if report.critical_claim_count > 0 {"P0"} else {"P1"},
            "worker_processes_cancelled":false}),
    )
}

#[derive(Clone, Copy)]
struct Selection<'a> {
    action: PivotAction,
    plan_ids: &'a [&'a str],
    lesson: Option<&'a PivotLessonDraft>,
}

impl Selection<'_> {
    fn just(action: PivotAction) -> Self {
        Self {
            action,
            plan_ids: &[],
            lesson: None,
        }
    }
}

fn plan(
    root: &Path,
    git: &PromotionRepo,
    store: &mut StateStore,
    fact: &str,
    breaker: &str,
    reason: &str,
    selection: Selection<'_>,
) -> Result<PivotPlan> {
    let base = git.main_head()?;
    let review = read_retained_review(&root.join(".runtime/briefings"), fact)?
        .context("retained Fact Pack not found; pull a human briefing first")?;
    let repo = repo_name(git)?;
    let plan = match selection.action {
        PivotAction::ArchiveRoutes => store.preview_archive_routes(
            repo,
            &base,
            fact,
            &review,
            breaker,
            reason,
            selection.plan_ids,
        )?,
        PivotAction::ArchiveRoutesWithLesson => store.preview_archive_routes_with_lesson(
            repo,
            &base,
            fact,
            &review,
            breaker,
            reason,
            selection.plan_ids,
            selection
                .lesson
                .context("explicit route lesson is required")?,
        )?,
        action => store.preview_pivot(repo, &base, fact, &review, breaker, reason, action)?,
    };
    if let Some(c) = plan.checkpoint.as_ref() {
        ensure!(
            git.is_ancestor(&c.git_commit, &base)?,
            "last human checkpoint is not in current main history"
        );
    }
    Ok(plan)
}

fn markdown(p: &PivotPlan) -> Result<String> {
    let id = p.adr_id()?;
    let action = p.action.as_str();
    // Existing recovery validates the precise historical ADR blob.
    let effect = match p.action {
        PivotAction::ResumeAsIs => {
            "The approved breaker moves from TRIPPED to HALF_OPEN. No worker tasks are released, no probes are started and no mathematical definitions change."
        }
        PivotAction::ArchiveBreaker => {
            "The approved breaker moves from TRIPPED to ARCHIVED, and its linked decision is resolved. Its module remains gated. No worktree, task, route, Claim, proof or mathematical definition changes."
        }
        PivotAction::ArchiveRoutes => {
            "The approved breaker remains gated as ARCHIVED. Only the explicitly selected ProofPlans are archived; dependency records are kept and derived Claim/route status is recomputed. No worktree, task, mathematical source, local Claim trust or proof receipt is changed."
        }
        PivotAction::ArchiveRoutesWithLesson => {
            "The approved breaker remains gated as ARCHIVED. The selected ProofPlans are archived and the expressly approved lesson is retained as an operator warning, not a refutation or proof. Dependencies, mathematical source, tasks, worktrees, local Claim trust and receipts are preserved."
        }
    };
    let mut s = format!(
        "# {id}: {action}\n\n- Base Git commit: {}\n- Retained Fact Pack SHA-256: {}\n- Reviewed event interval: ({}, {}]\n- Pack presentation truncated: {}\n\n{effect}\n\n## Approved decision\n\n",
        p.base_commit,
        p.fact_sha256,
        p.event_from_exclusive,
        p.reviewed_through,
        p.presentation_truncated
    );
    // Treat model/workspace-sourced labels and the operator's explanation as
    // literal evidence, not active Markdown or directives.
    let mut decision = json!({"breaker_id":p.breaker.breaker_id,"definition_id":p.breaker.target_definition_id,"module_path":p.breaker.module_path,"engine":p.breaker.engine,"inbox_id":p.inbox_id,"previous_checkpoint":p.checkpoint,"reason":p.decision_reason});
    if !p.archive_plans.is_empty() {
        decision["archive_plans"] = serde_json::to_value(&p.archive_plans)?;
    }
    if let Some(lesson_id) = p.lesson_id()? {
        decision["graveyard_lesson_id"] = json!(lesson_id);
        decision["graveyard_lesson"] = serde_json::to_value(&p.lesson)?;
    }
    for line in serde_json::to_string_pretty(&decision)?.lines() {
        s.push_str("    ");
        s.push_str(line);
        s.push('\n');
    }
    Ok(s)
}

pub(crate) fn preview(
    root: &Path,
    repo: &Path,
    fact: &str,
    breaker: &str,
    reason: &str,
    action: PivotAction,
) -> Result<()> {
    preview_with(root, repo, fact, breaker, reason, Selection::just(action))
}

pub(crate) fn preview_routes(
    root: &Path,
    repo: &Path,
    fact: &str,
    breaker: &str,
    reason: &str,
    plan_ids: &[&str],
) -> Result<()> {
    preview_with(
        root,
        repo,
        fact,
        breaker,
        reason,
        Selection {
            action: PivotAction::ArchiveRoutes,
            plan_ids,
            lesson: None,
        },
    )
}

pub(crate) fn preview_lesson(
    root: &Path,
    repo: &Path,
    fact: &str,
    breaker: &str,
    reason: &str,
    lesson: &PivotLessonDraft,
    plan_ids: &[&str],
) -> Result<()> {
    preview_with(
        root,
        repo,
        fact,
        breaker,
        reason,
        Selection {
            action: PivotAction::ArchiveRoutesWithLesson,
            plan_ids,
            lesson: Some(lesson),
        },
    )
}

fn preview_with(
    root: &Path,
    repo: &Path,
    fact: &str,
    breaker: &str,
    reason: &str,
    selection: Selection<'_>,
) -> Result<()> {
    let git = PromotionRepo::lock(repo)?;
    let mut store = StateStore::open_readonly(root.join(".runtime/state.sqlite"))?;
    let p = plan(root, &git, &mut store, fact, breaker, reason, selection)?;
    let action = p.action;
    let mut preview = json!({"adr_id":p.adr_id()?,"action":action.as_str(),"to_state":action.target_state(),"probe_tasks":[],"stopped_tasks":[],"tainted_claims":[],"archived_claims":[],"new_tasks":[],"plan":p,"adr_markdown":markdown(&p)?});
    if !p.archive_plans.is_empty() {
        preview["archive_plans"] = serde_json::to_value(&p.archive_plans)?;
    }
    if let Some(lesson_id) = p.lesson_id()? {
        preview["lesson_id"] = json!(lesson_id);
    }
    print_json(preview)
}

fn cleanup(pin: &PendingRef<'_>, commit: &str) {
    if let Err(e) = pin.remove(commit) {
        eprintln!("ai4math: run pivot-recover to clean pending ref: {e:#}");
    }
}

fn prepare(
    store: &mut StateStore,
    git: &PromotionRepo,
    pin: &PendingRef<'_>,
    p: PivotPlan,
) -> Result<PivotIntent> {
    let adr_id = p.adr_id()?;
    let new_commit = git.prepare_adr(&p.base_commit, &adr_id, &markdown(&p)?)?;
    let intent = PivotIntent {
        adr_id,
        plan: p,
        new_commit,
    };
    pin.ensure_pinned(&intent.new_commit)?;
    if let Err(err) = store.prepare_pivot(&intent) {
        if let Ok(current) = store.pivot_intent()
            && current.as_ref() != Some(&intent)
        {
            cleanup(pin, &intent.new_commit);
        }
        return Err(err);
    }
    Ok(intent)
}

fn land(
    store: &mut StateStore,
    git: &PromotionRepo,
    pin: &PendingRef<'_>,
    intent: &PivotIntent,
) -> Result<PivotResult> {
    let main = git.main_head()?;
    let p = &intent.plan;
    ensure!(
        repo_name(git)? == p.repo && p.adr_id()? == intent.adr_id,
        "pending Pivot identity mismatch"
    );
    let adopted = main == intent.new_commit;
    ensure!(
        adopted || main == p.base_commit,
        "main moved away from the reviewed base or exact ADR {}; leaving journal unchanged",
        intent.adr_id
    );
    let md = markdown(p)?;
    let result = store
        .land_pivot(intent, adopted, || {
            if adopted {
                git.validate_adr(&p.base_commit, &intent.new_commit, &intent.adr_id, &md)?;
            } else {
                git.advance_adr(&p.base_commit, &intent.new_commit, &intent.adr_id, &md)?;
            }
            ensure!(
                git.main_head()? == intent.new_commit,
                "exact ADR no longer equals main"
            );
            Ok(())
        })
        .with_context(|| {
            format!(
                "Pivot {} still pending; use pivot-recover or pivot-abandon if unadopted",
                intent.adr_id
            )
        })?;
    cleanup(pin, &intent.new_commit);
    Ok(result)
}

pub(crate) fn apply(
    root: &Path,
    repo: &Path,
    fact: &str,
    breaker: &str,
    reason: &str,
    approved_id: &str,
    action: PivotAction,
) -> Result<()> {
    apply_with(
        root,
        repo,
        fact,
        breaker,
        reason,
        approved_id,
        Selection::just(action),
    )
}

pub(crate) fn apply_routes(
    root: &Path,
    repo: &Path,
    fact: &str,
    breaker: &str,
    reason: &str,
    approved_id: &str,
    plan_ids: &[&str],
) -> Result<()> {
    apply_with(
        root,
        repo,
        fact,
        breaker,
        reason,
        approved_id,
        Selection {
            action: PivotAction::ArchiveRoutes,
            plan_ids,
            lesson: None,
        },
    )
}

pub(crate) fn apply_lesson(
    root: &Path,
    repo: &Path,
    fact: &str,
    breaker: &str,
    reason: &str,
    approved_id: &str,
    lesson: &PivotLessonDraft,
    plan_ids: &[&str],
) -> Result<()> {
    apply_with(
        root,
        repo,
        fact,
        breaker,
        reason,
        approved_id,
        Selection {
            action: PivotAction::ArchiveRoutesWithLesson,
            plan_ids,
            lesson: Some(lesson),
        },
    )
}

fn apply_with(
    root: &Path,
    repo: &Path,
    fact: &str,
    breaker: &str,
    reason: &str,
    approved_id: &str,
    selection: Selection<'_>,
) -> Result<()> {
    let git = PromotionRepo::lock(repo)?;
    let state_path = root.join(".runtime/state.sqlite");
    let mut store = StateStore::open(&state_path)?;
    if let Some((p, r)) = store.recorded_pivot(approved_id)? {
        let identical_selection = if matches!(
            selection.action,
            PivotAction::ArchiveRoutes | PivotAction::ArchiveRoutesWithLesson
        ) {
            p.archive_plan_ids()
                .eq(PivotPlan::normalize_archive_plan_ids(selection.plan_ids)?)
        } else {
            p.archive_plan_ids().next().is_none()
        };
        ensure!(
            repo_name(&git)? == p.repo
                && fact == p.fact_sha256
                && breaker == p.breaker.breaker_id
                && reason == p.decision_reason
                && p.action == selection.action
                && identical_selection
                && p.lesson.as_ref() == selection.lesson
                && p.adr_id()? == approved_id,
            "Pivot replay arguments do not match the committed approval"
        );
        git.validate_adr(&p.base_commit, &r.git_commit, approved_id, &markdown(&p)?)?;
        ensure!(
            git.is_ancestor(&r.git_commit, &git.main_head()?)?,
            "recorded ADR no longer belongs to main"
        );
        return print_json(r);
    }
    let p = plan(root, &git, &mut store, fact, breaker, reason, selection)?;
    ensure!(
        p.adr_id()? == approved_id,
        "approval ID does not match current Pivot preview"
    );
    let pin = git.pivot_pending_ref(&state_path)?;
    pin.clear()?;
    let intent = prepare(&mut store, &git, &pin, p)?;
    print_json(land(&mut store, &git, &pin, &intent)?)
}

pub(crate) fn recover(root: &Path, repo: &Path) -> Result<()> {
    let git = PromotionRepo::lock(repo)?;
    let state = root.join(".runtime/state.sqlite");
    let mut store = StateStore::open(&state)?;
    let pin = git.pivot_pending_ref(&state)?;
    let Some(intent) = store.pivot_intent()? else {
        pin.clear()?;
        return print_json(json!({"pending":false}));
    };
    ensure!(
        repo_name(&git)? == intent.plan.repo,
        "pending Pivot belongs to a different repository"
    );
    pin.ensure_pinned(&intent.new_commit)?;
    print_json(land(&mut store, &git, &pin, &intent)?)
}

pub(crate) fn abandon(root: &Path, repo: &Path, approved_id: &str) -> Result<()> {
    let git = PromotionRepo::lock(repo)?;
    let state = root.join(".runtime/state.sqlite");
    let mut store = StateStore::open(&state)?;
    let Some(intent) = store.pivot_intent()? else {
        bail!("no pending Pivot");
    };
    ensure!(
        repo_name(&git)? == intent.plan.repo && approved_id == intent.adr_id,
        "pending Pivot approval identity mismatch"
    );
    ensure!(
        !git.is_ancestor(&intent.new_commit, &git.main_head()?)?,
        "main adopted this ADR; cannot abandon"
    );
    store.abandon_unadopted_pivot(&intent)?;
    cleanup(&git.pivot_pending_ref(&state)?, &intent.new_commit);
    print_json(json!({"abandoned":approved_id}))
}

#[cfg(test)]
mod tests;
