//! Advisory lessons are bound to finalized archive approvals, not to proof evidence.
use super::*;

#[cfg(test)]
mod tests;

const MAX_SCAN: usize = 256;
const MAX_LIMIT: usize = 16;

/// Operator-supplied advisory text, exactly as bound into the Pivot approval.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PivotLessonDraft {
    pub title: String,
    pub lesson: String,
}

impl PivotLessonDraft {
    pub fn new(title: &str, lesson: &str) -> Result<Self> {
        validate_draft(title, lesson)?;
        Ok(Self {
            title: title.into(),
            lesson: lesson.into(),
        })
    }

    pub(super) fn validate(&self) -> Result<()> {
        validate_draft(&self.title, &self.lesson)
    }
}

fn validate_draft(title: &str, lesson: &str) -> Result<()> {
    fn safe_line(text: &str, max_bytes: usize) -> bool {
        safe_text(text, max_bytes)
            && text == text.trim()
            && !text.chars().any(|c| {
                matches!(c, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
            })
    }
    ensure!(
        safe_line(title, 120),
        "lesson title must be a trimmed, safe single line of 1 to 120 UTF-8 bytes"
    );
    ensure!(
        safe_line(lesson, 512),
        "lesson text must be a trimmed, safe single line of 1 to 512 UTF-8 bytes"
    );
    Ok(())
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PivotLessonWarning {
    pub lesson_id: String,
    pub kind: &'static str,
    pub trust_level: &'static str,
    pub advisory_only: bool,
    pub adr_id: String,
    pub breaker_id: String,
    pub title: String,
    pub lesson: String,
    pub plan_ids: Vec<String>,
    pub event_seq: i64,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PivotLessonPage {
    pub items: Vec<PivotLessonWarning>,
    pub truncated: bool,
}

pub(super) fn id_for_adr(adr_id: &str) -> Result<String> {
    let sha = adr_id
        .strip_prefix("ADR-PIVOT-")
        .filter(|sha| sha.len() == 64 && oid(sha))
        .context("invalid Pivot approval ID for lesson")?;
    Ok(format!("GRV-PIVOT-{sha}"))
}

fn summary_digest(
    adr_id: &str,
    breaker_id: &str,
    title: &str,
    lesson: &str,
    ids: &[&str],
) -> Result<String> {
    digest(&(
        "PIVOT_ROUTE_LESSON/v1",
        adr_id,
        breaker_id,
        title,
        lesson,
        ids,
    ))
}

pub(super) fn plan_digest(plan: &PivotPlan, adr_id: &str) -> Result<String> {
    let draft = plan.lesson.as_ref().context("missing lesson draft")?;
    let ids: Vec<_> = plan.archive_plan_ids().collect();
    summary_digest(
        adr_id,
        &plan.breaker.breaker_id,
        &draft.title,
        &draft.lesson,
        &ids,
    )
}

fn from_plan(
    plan: &PivotPlan,
    adr_id: &str,
    lesson_id: &str,
    event_seq: i64,
) -> Result<PivotLessonWarning> {
    let draft = plan.lesson.as_ref().context("missing lesson draft")?;
    Ok(PivotLessonWarning {
        lesson_id: lesson_id.into(),
        kind: "operator_route_lesson",
        trust_level: "L0",
        advisory_only: true,
        adr_id: adr_id.into(),
        breaker_id: plan.breaker.breaker_id.clone(),
        title: draft.title.clone(),
        lesson: draft.lesson.clone(),
        plan_ids: plan.archive_plan_ids().map(str::to_owned).collect(),
        event_seq,
    })
}

pub(super) fn insert(
    tx: &Transaction<'_>,
    plan: &PivotPlan,
    adr_id: &str,
    lesson_id: &str,
    event_seq: i64,
) -> Result<()> {
    let lesson = from_plan(plan, adr_id, lesson_id, event_seq)?;
    tx.execute(
        "INSERT INTO pivot_route_lessons(event_seq,adr_id,lesson_id,breaker_id,title,lesson,plan_ids_json)
         VALUES(?1,?2,?3,?4,?5,?6,?7)",
        params![lesson.event_seq, lesson.adr_id, lesson.lesson_id, lesson.breaker_id,
            lesson.title, lesson.lesson, serde_json::to_string(&lesson.plan_ids)?],
    )?;
    Ok(())
}

// Require both stable IDs to identify the same finalized approval and event.
// The indexed event lookup checks bounded metadata, never the private plan blob.
const READ: &str =
    "SELECT l.lesson_id,l.adr_id,l.breaker_id,l.title,l.lesson,l.plan_ids_json,l.event_seq,
    (SELECT CASE WHEN length(CAST(e.payload_json AS BLOB))<=4096
                 THEN CASE WHEN json_extract(e.payload_json,'$.action')='ARCHIVE_ROUTES_WITH_LESSON'
                           AND json_extract(e.payload_json,'$.breaker_id')=l.breaker_id
                           THEN json_extract(e.payload_json,'$.lesson_sha256') END END
       FROM recorded_pivots p JOIN events e USING (event_seq)
       WHERE p.adr_id=l.adr_id AND p.event_seq=l.event_seq
         AND e.event_type='PIVOT_APPLIED' AND e.entity_type='adr' AND e.entity_id=l.adr_id)
    FROM pivot_route_lessons l";

fn decode(row: &Row<'_>) -> Result<PivotLessonWarning> {
    let lesson_id: String = row.get(0)?;
    let adr_id: String = row.get(1)?;
    let breaker_id: String = row.get(2)?;
    let title: String = row.get(3)?;
    let lesson: String = row.get(4)?;
    let ids_json: String = row.get(5)?;
    let event_seq: i64 = row.get(6)?;
    let approved_sha256: Option<String> = row.get(7)?;
    ensure!(
        event_seq > 0 && lesson_id == id_for_adr(&adr_id)? && valid_id(&breaker_id),
        "invalid persisted Pivot lesson approval binding"
    );
    validate_draft(&title, &lesson)?;
    ensure!(
        ids_json.len() <= 8192,
        "oversized persisted Pivot lesson plan selection"
    );
    let plan_ids: Vec<String> =
        serde_json::from_str(&ids_json).context("decode Pivot lesson plan IDs")?;
    let ids: Vec<&str> = plan_ids.iter().map(String::as_str).collect();
    ensure!(
        PivotPlan::normalize_archive_plan_ids(&ids)? == ids
            && serde_json::to_string(&plan_ids)? == ids_json,
        "noncanonical persisted Pivot lesson plan selection"
    );
    ensure!(
        approved_sha256.as_deref()
            == Some(summary_digest(&adr_id, &breaker_id, &title, &lesson, &ids)?.as_str()),
        "persisted Pivot lesson does not match its finalized approval"
    );
    Ok(PivotLessonWarning {
        lesson_id,
        kind: "operator_route_lesson",
        trust_level: "L0",
        advisory_only: true,
        adr_id,
        breaker_id,
        title,
        lesson,
        plan_ids,
        event_seq,
    })
}

pub(super) fn confirm_recorded(
    db: &Connection,
    plan: &PivotPlan,
    adr_id: &str,
    lesson_id: &str,
    event_seq: i64,
) -> Result<()> {
    let mut stmt = db.prepare(&format!("{READ} WHERE l.adr_id=?1"))?;
    let mut rows = stmt.query([adr_id])?;
    let warning = decode(rows.next()?.context("recorded Pivot lesson is missing")?)?;
    ensure!(
        warning == from_plan(plan, adr_id, lesson_id, event_seq)?,
        "recorded Pivot lesson does not match its approval"
    );
    Ok(())
}

impl StateStore {
    /// Scan the newest 256 finalized lessons by event sequence (newest first).
    /// Split the query on Unicode whitespace; every term must be a literal
    /// substring of at least one public text/ID field. Matching uses Unicode
    /// lowercasing, not normalization, regex, wildcards, stemming or FTS.
    /// A page is truncated if another match is known or older rows were not scanned.
    pub fn pivot_lessons(&self, query: &str, limit: usize) -> Result<PivotLessonPage> {
        ensure!(
            query.len() <= 512 && !query.trim().is_empty() && !query.contains('\0'),
            "Pivot lesson lookup requires 1 to 512 UTF-8 bytes of nonblank query text without NUL"
        );
        ensure!(
            (1..=MAX_LIMIT).contains(&limit),
            "Pivot lesson limit must be in 1..={MAX_LIMIT}"
        );
        let lower_query = query.to_lowercase();
        let terms: Vec<_> = lower_query.split_whitespace().collect();
        let mut stmt = self
            .db
            .prepare(&format!("{READ} ORDER BY l.event_seq DESC LIMIT ?1"))?;
        let mut rows = stmt.query([MAX_SCAN + 1])?;
        let mut page = PivotLessonPage {
            items: Vec::new(),
            truncated: false,
        };
        let mut inspected = 0;
        while let Some(row) = rows.next()? {
            if inspected == MAX_SCAN {
                page.truncated = true;
                break;
            }
            inspected += 1;
            let warning = decode(row)?;
            if matches_query(&warning, &terms) {
                if page.items.len() == limit {
                    page.truncated = true;
                    break;
                }
                page.items.push(warning);
            }
        }
        Ok(page)
    }
}

fn matches_query(warning: &PivotLessonWarning, terms: &[&str]) -> bool {
    let fields: Vec<_> = [
        &warning.title,
        &warning.lesson,
        &warning.breaker_id,
        &warning.lesson_id,
        &warning.adr_id,
    ]
    .into_iter()
    .chain(&warning.plan_ids)
    .map(|field| field.to_lowercase())
    .collect();
    terms
        .iter()
        .all(|term| fields.iter().any(|field| field.contains(term)))
}
