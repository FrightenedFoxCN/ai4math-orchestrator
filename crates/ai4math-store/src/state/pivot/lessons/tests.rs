use super::*;

const REPO: &str = "/lesson-fixture.git";
const BASE: &str = "1111111111111111111111111111111111111111";
const FACT: &str = "2222222222222222222222222222222222222222222222222222222222222222";
const COMMIT: &str = "3333333333333333333333333333333333333333";
const CHANGE: Change<'static> = Change {
    actor: "fixture",
    git_commit: Some(BASE),
};

fn setup(path: &Path) -> Result<(StateStore, RetainedReview)> {
    let mut store = StateStore::open(path)?;
    store.create_claim(
        NewClaim {
            id: "goal",
            title: "Fixture Goal",
            informal_statement: "A fixture",
            engine: "lean4",
            formal_target: Some("Defs.lean#Watched"),
            module_path: Some("Defs.lean"),
            critical: false,
        },
        CHANGE,
    )?;
    store.set_local_trust("goal", Trust::L3, CHANGE)?;
    for id in ["one", "two", "unselected"] {
        store.create_plan(
            NewPlan {
                id,
                target: "goal",
                status: PlanStatus::Active,
                name: id,
                track: "exploration",
                dependencies: &[],
            },
            CHANGE,
        )?;
    }
    store.register_tripped_breaker(
        REPO,
        BASE,
        "breaker",
        "Watched",
        "Defs.lean",
        "lean4",
        "Incident",
        false,
    )?;
    let facts = store.briefing_facts()?;
    let review = RetainedReview {
        event_from_exclusive: facts.event_from_exclusive,
        event_to_inclusive: facts.event_to_inclusive,
        to_git_commit: facts.to_git_commit,
        checkpoint: facts.checkpoint,
        presentation_truncated: facts.frontier.plans.truncated,
        breakers: facts.breakers.items,
        inbox: facts.inbox.items,
        frontier_edges: facts.frontier.plans.items,
    };
    Ok((store, review))
}

#[test]
fn strict_draft_validation_uses_trimmed_single_line_utf8_bytes() -> Result<()> {
    let title = "é".repeat(60);
    let lesson = "⚒".repeat(170) + "é";
    assert_eq!(PivotLessonDraft::new(&title, &lesson)?.lesson.len(), 512);
    assert!(PivotLessonDraft::new(&(title + "a"), &lesson).is_err());
    assert!(PivotLessonDraft::new("a", &(lesson + "b")).is_err());
    for bad in [
        "",
        " leading",
        "trailing\u{a0}",
        "a\r\nb",
        "a\tb",
        "a\0b",
        "a\u{85}b",
        "a\u{2028}b",
        "a\u{2029}b",
        "a\u{202e}b",
    ] {
        assert!(PivotLessonDraft::new(bad, "ok").is_err(), "{bad:?}");
        assert!(PivotLessonDraft::new("ok", bad).is_err(), "{bad:?}");
    }
    Ok(())
}

#[test]
fn lesson_is_exactly_bound_and_atomic_across_recovery() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("state.sqlite");
    let (mut store, review) = setup(&path)?;
    let draft = PivotLessonDraft::new("Échec [literal]", "Operator note: keep % and _ literal.")?;
    let old = store.preview_archive_routes(
        REPO,
        BASE,
        FACT,
        &review,
        "breaker",
        "Reviewed",
        &["two", "one"],
    )?;
    assert_eq!(old.lesson_id()?, None);
    assert!(serde_json::to_value(&old)?.get("lesson").is_none());
    let plan = store.preview_archive_routes_with_lesson(
        REPO,
        BASE,
        FACT,
        &review,
        "breaker",
        "Reviewed",
        &["one", "two"],
        &draft,
    )?;
    assert_eq!(plan.archive_plans, old.archive_plans);
    assert_eq!(plan.target_sha256, old.target_sha256);
    assert_ne!(plan.adr_id()?, old.adr_id()?);
    assert_eq!(
        serde_json::to_value(&plan)?["lesson"],
        serde_json::to_value(&draft)?
    );
    let id = plan.lesson_id()?.unwrap();
    let mut tampered = plan.clone();
    tampered.lesson.as_mut().unwrap().lesson.push('\n');
    assert!(tampered.adr_id().is_err());
    assert!(tampered.lesson_id().is_err());
    tampered.lesson = None;
    assert!(tampered.adr_id().is_err());
    let mut tampered = old;
    tampered.lesson = Some(draft.clone());
    assert!(tampered.adr_id().is_err());
    let mut wrong_review = review.clone();
    wrong_review
        .frontier_edges
        .retain(|edge| edge.plan_id != "two");
    assert!(
        store
            .preview_archive_routes_with_lesson(
                REPO,
                BASE,
                FACT,
                &wrong_review,
                "breaker",
                "Reviewed",
                &["one", "two"],
                &draft
            )
            .is_err()
    );

    let intent = PivotIntent {
        adr_id: plan.adr_id()?,
        plan,
        new_commit: COMMIT.into(),
    };
    assert_eq!(id, format!("GRV-PIVOT-{}", &intent.adr_id[10..]));
    store.prepare_pivot(&intent)?;
    assert!(store.pivot_lessons("literal", 8)?.items.is_empty());
    store.db.execute_batch(
        "CREATE TRIGGER interrupt_lesson BEFORE INSERT ON pivot_route_lessons
        BEGIN SELECT RAISE(ABORT, 'simulated post-CAS lesson write failure'); END;",
    )?;
    let mut adopted = false;
    let err = store
        .land_pivot(&intent, false, || {
            adopted = true;
            Ok(())
        })
        .unwrap_err();
    assert!(adopted);
    assert!(
        err.to_string()
            .contains("simulated post-CAS lesson write failure")
    );
    assert!(store.recorded_pivot(&intent.adr_id)?.is_none());
    assert!(store.pivot_lessons("literal", 8)?.items.is_empty());
    assert_eq!(last_seq(&store.db)?, review.event_to_inclusive);
    assert_eq!(store.plan_state("one")?.unwrap().0, "ACTIVE");
    store.db.execute_batch("DROP TRIGGER interrupt_lesson;")?;
    drop(store);

    let mut store = StateStore::open(&path)?;
    assert_eq!(store.pivot_intent()?.as_ref(), Some(&intent));
    let original = serde_json::to_string(&intent.plan)?;
    let mut altered = intent.plan.clone();
    altered.lesson.as_mut().unwrap().lesson = "A different valid draft".into();
    assert_ne!(altered.adr_id()?, intent.adr_id);
    store.db.execute(
        "UPDATE pivot_resume_intent SET plan_json=?1",
        [serde_json::to_string(&altered)?],
    )?;
    assert!(store.pivot_intent().is_err());
    assert!(
        store
            .land_pivot(&intent, true, || panic!("tampered lesson must not recover"))
            .is_err()
    );
    store
        .db
        .execute("UPDATE pivot_resume_intent SET plan_json=?1", [original])?;

    let applied = store.land_pivot(&intent, true, || Ok(()))?;
    assert_eq!(applied.lesson_id.as_deref(), Some(id.as_str()));
    assert_eq!(applied.breaker_state, "ARCHIVED");
    assert!(!applied.probes_started);
    assert!(store.pivot_intent()?.is_none());
    let recorded = store.recorded_pivot(&intent.adr_id)?.unwrap();
    assert_eq!(recorded, (intent.plan.clone(), applied.clone()));
    assert!(
        store
            .land_pivot(&intent, true, || panic!("must not refinalize"))
            .is_err()
    );
    for route in ["one", "two"] {
        assert_eq!(
            store.plan_state(route)?,
            Some(("ARCHIVED_BY_ADR".into(), Trust::L0))
        );
    }
    assert_eq!(store.plan_state("unselected")?.unwrap().0, "ACTIVE");
    assert_eq!(store.claim("goal")?.unwrap().local_trust, Trust::L3);
    assert_eq!(
        store
            .db
            .query_row("SELECT count(*) FROM verification_receipts", [], |r| r
                .get::<_, i64>(0))?,
        0
    );
    assert_eq!(
        store
            .db
            .query_row("SELECT count(*) FROM pivot_route_lessons", [], |r| r
                .get::<_, i64>(0))?,
        1
    );
    let page = store.pivot_lessons("  ÉCHEC\n[LITERAL]  % two ", 1)?;
    assert!(!page.truncated);
    assert_eq!(
        serde_json::to_value(&page.items)?,
        json!([{
            "lesson_id": id, "kind": "operator_route_lesson", "trust_level": "L0", "advisory_only": true,
            "adr_id": intent.adr_id, "breaker_id": "breaker", "title": draft.title, "lesson": draft.lesson,
            "plan_ids": ["one", "two"], "event_seq": applied.event_seq,
        }])
    );
    assert!(store.pivot_lessons("[.*]", 2)?.items.is_empty());
    assert!(store.pivot_lessons("%unselected", 2)?.items.is_empty());
    for sql in [
        "UPDATE pivot_route_lessons SET title='different'",
        "DELETE FROM pivot_route_lessons",
        "INSERT OR REPLACE INTO pivot_route_lessons SELECT * FROM pivot_route_lessons",
    ] {
        assert!(
            store
                .db
                .execute(sql, [])
                .unwrap_err()
                .to_string()
                .contains("immutable")
        );
    }
    // Simulate a corrupt backing row; the read must not sanitize or surface it.
    store
        .db
        .execute_batch("DROP TRIGGER pivot_route_lessons_no_update;")?;
    store.db.execute(
        "UPDATE pivot_route_lessons SET title='A different valid title'",
        [],
    )?;
    assert!(store.pivot_lessons("literal", 8).is_err());
    assert!(store.recorded_pivot(&intent.adr_id).is_err());
    store.db.execute(
        "UPDATE pivot_route_lessons SET title=?1, plan_ids_json='[\"two\",\"one\"]'",
        [&draft.title],
    )?;
    assert!(store.pivot_lessons("literal", 8).is_err());
    assert!(store.recorded_pivot(&intent.adr_id).is_err());
    Ok(())
}

#[test]
fn bounded_lookup_reports_proven_omissions_and_unknown_older_matches() -> Result<()> {
    let mut store = StateStore::open(":memory:")?;
    store.write(|tx| {
        for n in 0..=MAX_SCAN {
            let adr = format!("ADR-PIVOT-{n:064x}");
            let draft = if n == 0 { "only outside cap" } else { "latest literal" };
            let sha = summary_digest(&adr, "test", draft, "lesson", &["route"])?;
            let seq = event(tx, CHANGE, "PIVOT_APPLIED", "adr", &adr,
                json!({"action":"ARCHIVE_ROUTES_WITH_LESSON", "breaker_id":"test", "lesson_sha256":sha}))?;
            tx.execute("INSERT INTO recorded_pivots VALUES (?1,'{}',?2,?3,?3)", params![adr, format!("{n:040x}"), seq])?;
            tx.execute("INSERT INTO pivot_route_lessons VALUES (?1,?2,?3,'test',?4,'lesson','[\"route\"]')",
                params![seq, adr, id_for_adr(&adr)?, draft])?;
        }
        Ok(())
    })?;
    let recent = store.pivot_lessons("LATEST literal", 2)?;
    assert_eq!(recent.items.len(), 2);
    assert!(recent.items[0].event_seq > recent.items[1].event_seq);
    assert!(recent.truncated);
    let out = store.pivot_lessons("outside cap", 3)?;
    assert!(out.items.is_empty());
    assert!(out.truncated);
    assert!(store.pivot_lessons("", 3).is_err());
    assert!(store.pivot_lessons("literal", MAX_LIMIT + 1).is_err());
    Ok(())
}
