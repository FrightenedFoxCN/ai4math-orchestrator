use super::*;

fn lesson(event_seq: i64) -> PivotLessonWarning {
    PivotLessonWarning {
        lesson_id: format!("GRV-PIVOT-{event_seq:064x}"),
        kind: "operator_route_lesson",
        trust_level: "L0",
        advisory_only: true,
        adr_id: format!("{event_seq:064x}"),
        breaker_id: "breaker-1".to_owned(),
        title: "Route warning".to_owned(),
        lesson: "Do not repeat this shortcut.".to_owned(),
        plan_ids: vec!["plan-1".to_owned()],
        event_seq,
    }
}

#[test]
fn empty_route_projection_preserves_static_shape_unless_scan_is_truncated() {
    let static_page = json!({"warnings":[], "truncated":false});
    let mut page = static_page.clone();
    wiki_add_route_lessons(&mut page, vec![], 1, false).unwrap();
    assert_eq!(page, static_page);

    wiki_add_route_lessons(&mut page, vec![], 1, true).unwrap();
    assert_eq!(
        page,
        json!({"warnings":[], "truncated":false,
               "route_lessons":[], "route_lessons_truncated":true})
    );
}

#[test]
fn overlapping_lesson_queries_merge_newest_first_before_enforcing_limit() {
    let mut page = json!({"warnings":[], "truncated":false});
    wiki_add_route_lessons(
        &mut page,
        vec![lesson(3), lesson(1), lesson(4), lesson(3)],
        2,
        false,
    )
    .unwrap();
    assert_eq!(
        page,
        json!({"warnings":[], "truncated":false,
               "route_lessons":[lesson(4), lesson(3)], "route_lessons_truncated":true})
    );
}

#[test]
fn route_projection_exposes_only_the_advisory_record() {
    let mut page = json!({"hits":[], "truncated":false});
    let row = lesson(1);
    let expected = json!({
        "lesson_id":row.lesson_id,
        "kind":"operator_route_lesson",
        "trust_level":"L0",
        "advisory_only":true,
        "adr_id":row.adr_id,
        "breaker_id":"breaker-1",
        "title":"Route warning",
        "lesson":"Do not repeat this shortcut.",
        "plan_ids":["plan-1"],
        "event_seq":1,
    });
    wiki_add_route_lessons(&mut page, vec![row], 1, false).unwrap();
    assert_eq!(page["route_lessons"], json!([expected]));
    assert_eq!(page["route_lessons_truncated"], false);
}

#[test]
fn route_projection_uses_remaining_bytes_without_displacing_static_warnings() {
    let warnings = vec!["a".repeat(WIKI_REPLY_BYTES - 140)];
    let static_page = wiki_page("warnings", warnings, 1, false, None).unwrap();
    assert_eq!(static_page["warnings"].as_array().unwrap().len(), 1);
    let mut page = static_page.clone();
    wiki_add_route_lessons(&mut page, vec![lesson(1)], 1, false).unwrap();
    assert_eq!(page["warnings"], static_page["warnings"]);
    assert_eq!(page["truncated"], false);
    assert_eq!(page["route_lessons"], json!([]));
    assert_eq!(page["route_lessons_truncated"], true);
    assert!(wiki_fit(&page, WIKI_REPLY_BYTES).is_some());
}
