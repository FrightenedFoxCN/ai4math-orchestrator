//! Operator-only durable RFC intake. Approval records consent, not work admission or authority.
use super::*;
use ai4math_core::{Engine, InfraRFC, InfraStatus, validate_source_path};
use sha2::{Digest, Sha256};

pub const INFRA_RFC_MAX_BYTES: usize = 16 * 1024;
pub(crate) const SUBMITTED: &str = "INFRA_RFC_SUBMITTED";
pub(crate) const APPROVED: &str = "INFRA_RFC_APPROVED";
const ACTOR: &str = "operator:infra-rfc";
const ENTITY: &str = "infra_rfc";
const READ: &str = "SELECT module_path,reviewed_commit,draft_sha256,consumer_sha256,draft_json,submitted_event_seq,approved_event_seq FROM infra_rfcs WHERE rfc_id=?1 AND repo=?2";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum InfraRfcStatus {
    Draft,
    Approved,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InfraRfcRecord {
    schema: &'static str,
    pub module_path: String,
    pub reviewed_commit: String,
    pub draft_sha256: String,
    pub status: InfraRfcStatus,
    pub draft: InfraRFC,
    pub submitted_event_seq: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approved_event_seq: Option<i64>,
}

fn id(text: &str, max: usize) -> bool {
    !text.is_empty()
        && text.len() <= max
        && text.as_bytes()[0].is_ascii_alphanumeric()
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b':'))
}
fn hex(text: &str, n: usize) -> bool {
    text.len() == n
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'a'..=b'f'))
}
fn commit(text: &str) -> bool {
    (hex(text, 40) || hex(text, 64)) && text.bytes().any(|b| b != b'0')
}
fn sha(data: &impl Serialize) -> Result<String> {
    Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(data)?)))
}
fn text_ok(s: &str, max: usize, multiline: bool) -> bool {
    !s.is_empty() && s.len()<=max && s.trim()==s && !s.chars().any(|c| {
        (c.is_control() && !(multiline && matches!(c,'\n' | '\t')))
            || matches!(c,'\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
    })
}
fn list_ok(items: &[String], nonempty: bool) -> bool {
    (!nonempty || !items.is_empty())
        && items.len() <= 16
        && items.iter().all(|item| text_ok(item, 512, false))
        && items.iter().collect::<BTreeSet<_>>().len() == items.len()
}
fn canonical(draft: &InfraRFC) -> Result<String> {
    ensure!(
        draft.status == InfraStatus::Draft
            && id(&draft.rfc_id, 100)
            && id(&draft.requested_by_claim, 256),
        "InfraRFC requires DRAFT and valid RFC and consumer Claim IDs"
    );
    ensure!(
        text_ok(&draft.motivation, 2048, true)
            && text_ok(&draft.consumer_usage_sketch, 4096, true)
            && list_ok(&draft.proposed_definitions, true)
            && list_ok(&draft.required_api_lemmas, false)
            && list_ok(&draft.smoke_test_criteria, true),
        "invalid bounded InfraRFC text, definitions, lemmas or smoke criteria"
    );
    let bytes = serde_json::to_string(draft)?;
    ensure!(
        bytes.len() <= INFRA_RFC_MAX_BYTES,
        "canonical InfraRFC exceeds 16 KiB"
    );
    Ok(bytes)
}

/// Parse the strict object form shared by the operator and store readback.
pub fn parse_infra_rfc_draft(bytes: &[u8]) -> Result<InfraRFC> {
    ensure!(
        bytes.len() <= INFRA_RFC_MAX_BYTES
            && bytes.iter().find(|b| !b.is_ascii_whitespace()) == Some(&b'{'),
        "InfraRFC draft must be a JSON object of at most 16 KiB"
    );
    let draft: InfraRFC = serde_json::from_slice(bytes)?;
    canonical(&draft)?;
    Ok(draft)
}

fn bound(db: &Connection, repo: &str) -> Result<()> {
    ensure!(
        Path::new(repo).is_absolute() && text_ok(repo, 4096, false),
        "invalid managed InfraRFC repository path"
    );
    let bound = db
        .query_row("SELECT repo FROM managed_repo WHERE singleton=1", [], |r| {
            r.get::<_, String>(0)
        })
        .optional()?;
    ensure!(
        bound.as_deref() == Some(repo),
        "InfraRFC requires this exact bound managed repository"
    );
    Ok(())
}
fn writing_scope(tx: &Transaction<'_>, repo: &str, review: &str) -> Result<()> {
    bound(tx, repo)?;
    ensure!(
        commit(review),
        "InfraRFC requires a full reviewed Git commit ID"
    );
    let ready:bool=tx.query_row("SELECT NOT EXISTS(SELECT 1 FROM promotion_intent) AND NOT EXISTS(SELECT 1 FROM pivot_resume_intent)",[],|r|r.get(0))?;
    ensure!(
        ready,
        "recover pending promotion or Pivot before changing an InfraRFC"
    );
    Ok(())
}
fn consumer(db: &Connection, draft: &InfraRFC) -> Result<String> {
    let (module,target,state):(String,String,String)=db.query_row(
        "SELECT c.module_path,c.formal_target,
                json_array(c.title,c.informal_statement,c.formal_signature,c.local_trust_level,c.effective_trust_level,
                           c.is_critical_path,c.receipt_id,c.wiki_id,c.updated_at,
                           (SELECT max(e.event_seq) FROM events e WHERE e.entity_type='claim' AND e.entity_id=c.claim_id))
           FROM claims c WHERE c.claim_id=?1 AND c.engine=?2 AND c.module_path IS NOT NULL AND c.formal_target IS NOT NULL
             AND c.stale=0 AND c.effective_stale=0
             AND c.local_trust_level NOT IN ('L2a','L2b','L5') AND c.effective_trust_level NOT IN ('L2a','L2b','L5')",
        params![draft.requested_by_claim,draft.engine.as_str()],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)),
    ).optional()?.context("InfraRFC requires an open nonstale same-engine consumer Claim with a formal target")?;
    ensure!(
        validate_source_path(draft.engine, &module).is_ok()
            && target
                .split_once('#')
                .is_some_and(|(path, decl)| path == module && id(decl, 256)),
        "InfraRFC consumer does not have an exact module#declaration target"
    );
    sha(&(
        "ai4math:infra-rfc-consumer:v1",
        &draft.requested_by_claim,
        draft.engine,
        module,
        target,
        state,
    ))
}
fn draft_sha(
    repo: &str,
    review: &str,
    module: &str,
    draft: &InfraRFC,
    consumer_sha: &str,
) -> Result<String> {
    sha(&(
        "ai4math:infra-rfc-draft:v1",
        repo,
        review,
        module,
        draft,
        consumer_sha,
    ))
}
fn valid_module(module: &str, engine: Engine) -> Result<()> {
    ensure!(
        module.len() <= 256 && validate_source_path(engine, module).is_ok(),
        "invalid InfraRFC target module"
    );
    Ok(())
}
fn event_payload(module: &str, engine: Engine, digest: &str) -> Value {
    json!({"module_path":module,"engine":engine.as_str(),"draft_sha256":digest})
}
fn event_matches(
    db: &Connection,
    seq: i64,
    kind: &str,
    id: &str,
    review: &str,
    payload: &Value,
) -> Result<bool> {
    Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM events WHERE event_seq=?1 AND event_type=?2 AND entity_type=?3 AND entity_id=?4 AND git_commit=?5 AND actor=?6 AND payload_json=?7)",
        params![seq,kind,ENTITY,id,review,ACTOR,serde_json::to_string(payload)?],|r|r.get(0))?)
}

struct Stored {
    row: InfraRfcRecord,
    consumer_sha: String,
}
fn fetch(db: &Connection, repo: &str, id: &str) -> Result<Option<Stored>> {
    let found = db
        .query_row(READ, params![id, repo], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, Option<i64>>(6)?,
            ))
        })
        .optional()?;
    let Some((module, review, digest, consumer_sha, body, submitted, approved)) = found else {
        return Ok(None);
    };
    let draft = parse_infra_rfc_draft(body.as_bytes())?;
    valid_module(&module, draft.engine)?;
    ensure!(
        draft.rfc_id == id
            && commit(&review)
            && hex(&consumer_sha, 64)
            && canonical(&draft)? == body
            && draft_sha(repo, &review, &module, &draft, &consumer_sha)? == digest,
        "stored InfraRFC does not match its immutable draft binding"
    );
    let payload = event_payload(&module, draft.engine, &digest);
    ensure!(
        submitted > 0 && event_matches(db, submitted, SUBMITTED, id, &review, &payload)?,
        "invalid InfraRFC submission event binding"
    );
    if let Some(seq) = approved {
        ensure!(
            seq > submitted && event_matches(db, seq, APPROVED, id, &review, &payload)?,
            "invalid InfraRFC approval event binding"
        );
    }
    Ok(Some(Stored {
        consumer_sha,
        row: InfraRfcRecord {
            schema: "ai4math.infra-rfc.v1",
            module_path: module,
            reviewed_commit: review,
            draft_sha256: digest,
            status: if approved.is_some() {
                InfraRfcStatus::Approved
            } else {
                InfraRfcStatus::Draft
            },
            draft,
            submitted_event_seq: submitted,
            approved_event_seq: approved,
        },
    }))
}

impl StateStore {
    /// Read a historical RFC for exactly the bound repository. This is a description, not launch authorization.
    pub fn infra_rfc(&self, repo: &str, rfc_id: &str) -> Result<Option<InfraRfcRecord>> {
        ensure!(id(rfc_id, 100), "invalid InfraRFC ID");
        bound(&self.db, repo)?;
        let table: bool = self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='infra_rfcs')",
            [],
            |r| r.get(0),
        )?;
        Ok(if table {
            fetch(&self.db, repo, rfc_id)?.map(|r| r.row)
        } else {
            None
        })
    }

    /// Caller holds the Git lock and confirms the actual reviewed head immediately before SQLite commit.
    pub fn submit_infra_rfc(
        &mut self,
        repo: &str,
        review: &str,
        module: &str,
        draft: &InfraRFC,
        confirm_git: impl FnOnce() -> Result<()>,
    ) -> Result<InfraRfcRecord> {
        let body = canonical(draft)?;
        valid_module(module, draft.engine)?;
        self.write(|tx| {
            writing_scope(tx,repo,review)?;
            let consumer_sha=consumer(tx,draft)?;
            let digest=draft_sha(repo,review,module,draft,&consumer_sha)?;
            let rfc=&draft.rfc_id;
            if let Some(stored)=fetch(tx,repo,rfc)? {
                ensure!(stored.row.draft_sha256==digest,"InfraRFC ID already identifies a different immutable draft or review scope");
                confirm_git()?;
                return Ok(stored.row);
            }
            let seq=event(tx,Change{actor:ACTOR,git_commit:Some(review)},SUBMITTED,ENTITY,rfc,event_payload(module,draft.engine,&digest))?;
            tx.execute("INSERT INTO infra_rfcs(rfc_id,repo,module_path,reviewed_commit,draft_sha256,consumer_sha256,draft_json,submitted_event_seq) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
                params![rfc,repo,module,review,digest,consumer_sha,body,seq])?;
            let row=fetch(tx,repo,rfc)?.context("submitted InfraRFC disappeared")?.row;
            confirm_git()?;
            Ok(row)
        })
    }

    /// Approval is compare-and-set against the draft and consumer snapshot, not a build readiness flag.
    pub fn approve_infra_rfc(
        &mut self,
        repo: &str,
        review: &str,
        rfc_id: &str,
        approval: &str,
        confirm_git: impl FnOnce() -> Result<()>,
    ) -> Result<InfraRfcRecord> {
        ensure!(
            id(rfc_id, 100) && hex(approval, 64),
            "invalid InfraRFC ID or approval digest"
        );
        self.write(|tx| {
            writing_scope(tx,repo,review)?;
            let stored=fetch(tx,repo,rfc_id)?.context("unknown InfraRFC")?;
            let row=&stored.row;
            ensure!(row.reviewed_commit==review && row.draft_sha256==approval,"InfraRFC approval does not match the exact reviewed draft");
            ensure!(consumer(tx,&row.draft)?==stored.consumer_sha,"InfraRFC consumer changed since submission");
            if row.approved_event_seq.is_some() {
                confirm_git()?;
                return Ok(stored.row);
            }
            let seq=event(tx,Change{actor:ACTOR,git_commit:Some(review)},APPROVED,ENTITY,rfc_id,event_payload(&row.module_path,row.draft.engine,approval))?;
            ensure!(tx.execute("UPDATE infra_rfcs SET approved_event_seq=?2 WHERE rfc_id=?1 AND approved_event_seq IS NULL",params![rfc_id,seq])?==1,"InfraRFC approval row changed");
            let approved=fetch(tx,repo,rfc_id)?.context("approved InfraRFC disappeared")?.row;
            confirm_git()?;
            Ok(approved)
        })
    }
}

#[cfg(test)]
mod tests;
