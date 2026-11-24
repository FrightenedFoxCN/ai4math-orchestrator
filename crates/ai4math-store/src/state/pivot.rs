//! Trusted operator's durable RESUME_AS_IS decision. A journal row is not a Git receipt.
use super::*;
use crate::{
    BreakerProjection, Checkpoint, InboxProjection,
    observation::{current_checkpoint, safe_text},
    retained::RetainedReview,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};

mod critical_routes;

pub(crate) const CRITICAL_BREAKER_TRIPPED: &str = "CRITICAL_BREAKER_TRIPPED";

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct BreakerTripReport {
    pub event_seq: i64,
    /// Exact number of newly blocked critical Claims for an escalated trip; zero for P1.
    pub critical_claim_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PivotPlan {
    pub repo: String,
    pub base_commit: String,
    pub fact_sha256: String,
    pub event_from_exclusive: i64,
    pub reviewed_through: i64,
    pub checkpoint: Option<Checkpoint>,
    pub presentation_truncated: bool,
    pub breaker: BreakerProjection,
    pub inbox_id: String,
    pub decision_reason: String,
    target_sha256: String,
}

impl PivotPlan {
    pub fn adr_id(&self) -> Result<String> {
        Ok(format!("ADR-PIVOT-{}", digest(&("RESUME_AS_IS/v1", self))?))
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PivotIntent {
    pub adr_id: String,
    pub plan: PivotPlan,
    pub new_commit: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PivotResult {
    pub adr_id: String,
    pub git_commit: String,
    pub event_seq: i64,
    pub reviewed_through: i64,
    pub breaker_state: &'static str,
    pub probes_started: bool,
}

fn digest(x: &impl Serialize) -> Result<String> {
    Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(x)?)))
}

pub(super) fn read_pending(db: &Connection) -> Result<Option<PivotIntent>> {
    db.query_row(
        "SELECT adr_id,plan_json,new_commit FROM pivot_resume_intent WHERE singleton=1",
        [],
        |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        },
    )
    .optional()?
    .map(|(adr_id, plan, new_commit)| {
        Ok(PivotIntent {
            adr_id,
            plan: serde_json::from_str(&plan)?,
            new_commit,
        })
    })
    .transpose()
}

fn oid(s: &str) -> bool {
    matches!(s.len(), 40 | 64)
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn valid_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 100
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b':'))
}

fn reason_ok(s: &str) -> bool {
    !s.trim().is_empty() && s.len() <= 512 && !s.chars().any(char::is_control)
}

fn bound_repo(db: &Connection, repo: &str) -> Result<()> {
    let actual: Option<String> = db
        .query_row("SELECT repo FROM managed_repo WHERE singleton=1", [], |r| {
            r.get(0)
        })
        .optional()?;
    ensure!(
        actual.as_deref() == Some(repo),
        "ledger is not bound to this managed Git repository"
    );
    Ok(())
}

fn read_target(db: &Connection, id: &str) -> Result<(BreakerProjection, InboxProjection, String)> {
    let found=db.query_row("SELECT b.breaker_id,b.target_definition_id,b.module_path,b.engine,b.state,b.updated_at,
        i.inbox_id,i.priority,i.category,substr(i.summary,1,512),i.leverage_score,i.created_at,i.payload_json,
        json_array(b.pinned_snapshot_id,b.retained_worktree_path,b.paused_task_ids_json,b.trigger_signals_json,b.tripped_at,i.summary,i.resolved_adr,i.resolved_at)
        FROM circuit_breakers b JOIN decision_inbox i ON i.inbox_id=b.linked_inbox_id
        WHERE b.breaker_id=?1 AND b.state='TRIPPED' AND i.status='PENDING' AND i.category='breaking_def_change'
        AND length(CAST(i.payload_json AS BLOB)) <= 4096 AND length(CAST(i.summary AS BLOB)) <= 512
        AND length(CAST(b.paused_task_ids_json AS BLOB))+length(CAST(b.trigger_signals_json AS BLOB)) <= 65536",
        [id],|r| Ok((BreakerProjection{breaker_id:r.get(0)?,target_definition_id:r.get(1)?,module_path:r.get(2)?,engine:r.get(3)?,state:r.get(4)?,updated_at:r.get(5)?,probe_status:None,probe_passed:None,probe_required:None},
        InboxProjection{inbox_id:r.get(6)?,priority:r.get(7)?,category:r.get(8)?,summary:r.get(9)?,leverage_score:r.get(10)?,created_at:r.get(11)?,payload:None,payload_truncated:false},
        r.get::<_,String>(12)?,r.get::<_,String>(13)?))).optional()?.context("selected TRIPPED breaker has no bounded linked PENDING decision")?;
    let (breaker, mut inbox, payload, private) = found;
    inbox.payload = Some(serde_json::from_str(&payload).context("malformed linked inbox payload")?);
    let fingerprint = digest(&(&breaker, &inbox, private))?;
    Ok((breaker, inbox, fingerprint))
}

fn confirm_context(db: &Connection, plan: &PivotPlan, before_cas: bool) -> Result<()> {
    bound_repo(db, &plan.repo)?;
    ensure!(
        current_checkpoint(db)? == plan.checkpoint,
        "human checkpoint changed since Pivot preview"
    );
    if before_cas {
        ensure!(
            last_seq(db)? == plan.reviewed_through,
            "ledger changed since Pivot preview; review new facts before applying"
        );
    }
    let (breaker, inbox, hash) = read_target(db, &plan.breaker.breaker_id)?;
    ensure!(
        breaker == plan.breaker && inbox.inbox_id == plan.inbox_id && hash == plan.target_sha256,
        "selected Pivot target changed since review"
    );
    Ok(())
}

impl StateStore {
    /// Operator registration of a reported incident, returning the regular trip event.
    pub fn register_tripped_breaker(
        &mut self,
        repo: &str,
        base: &str,
        id: &str,
        target: &str,
        module: &str,
        engine: &str,
        reason: &str,
        critical: bool,
    ) -> Result<i64> {
        Ok(self
            .register_tripped_breaker_report(
                repo, base, id, target, module, engine, reason, critical,
            )?
            .event_seq)
    }

    /// Same operation, with the committed escalation count. `critical` is
    /// operator consent, not evidence that the breaker cuts a critical route.
    pub fn register_tripped_breaker_report(
        &mut self,
        repo: &str,
        base: &str,
        id: &str,
        target: &str,
        module: &str,
        engine: &str,
        reason: &str,
        critical: bool,
    ) -> Result<BreakerTripReport> {
        ensure!(
            Path::new(repo).is_absolute()
                && oid(base)
                && valid_id(id)
                && valid_id(target)
                && reason_ok(reason)
                && !module.is_empty()
                && module.len() <= 256
                && matches!(engine, "lean4" | "agda" | "rocq"),
            "invalid breaker input"
        );
        self.write(|tx| {
            ensure!(read_pending(tx)?.is_none() && read_intent(tx)?.is_none(),"recover pending Git transaction before registering a breaker");
            tx.execute("INSERT OR IGNORE INTO managed_repo(singleton,repo) VALUES(1,?1)",[repo])?;
            bound_repo(tx,repo)?;
            let impact = if critical { critical_routes::newly_blocked(tx, engine, module, target)? } else { None };
            let inbox_id=format!("DEF-{id}");
            let c=Change{actor:"operator:breaker",git_commit:Some(base)};
            let seq=event(tx,c,"BREAKER_TRIPPED","breaker",id,json!({"state":"TRIPPED","operator_report":true,"inbox_id":inbox_id}))?;
            let mut payload=json!({"breaker_id":id,"target_definition_id":target,"module_path":module,"operator_report":true});
            let alert = impact.as_ref().map(|impact| {
                let mut evidence = json!({"trip_event_seq":seq,"critical_claim_count":impact.claim_count});
                // Older/general Claim writes allow arbitrary IDs. Never turn a
                // shortened or control-bearing ID into a public reference.
                if safe_text(&impact.first_claim, 256) {
                    evidence["critical_claim_id"] = json!(impact.first_claim);
                }
                payload.as_object_mut().expect("Inbox object").extend(evidence.as_object().expect("evidence object").clone());
                evidence["state"] = json!("TRIPPED");
                evidence["inbox_id"] = json!(inbox_id);
                evidence
            });
            let priority = if alert.is_some() {"P0"} else {"P1"};
            tx.execute(&format!("INSERT INTO decision_inbox(inbox_id,created_at,priority,category,summary,payload_json,status) VALUES(?1,{NOW},?2,'breaking_def_change',?3,?4,'PENDING')"),params![inbox_id,priority,reason,payload.to_string()])?;
            tx.execute(&format!("INSERT INTO circuit_breakers(breaker_id,target_definition_id,module_path,engine,state,linked_inbox_id,tripped_at,updated_at) VALUES(?1,?2,?3,?4,'TRIPPED',?5,{NOW},{NOW})"),params![id,target,module,engine,inbox_id])?;
            super::tasks::freeze_module_leases(tx,engine,module,c)?;
            if let Some(alert) = alert {
                event(tx,c,CRITICAL_BREAKER_TRIPPED,"breaker",id,alert)?;
            }
            Ok(BreakerTripReport {event_seq: seq, critical_claim_count: impact.map_or(0, |i|i.claim_count)})
        })
    }

    pub fn preview_resume(
        &mut self,
        repo: &str,
        base: &str,
        fact: &str,
        review: &RetainedReview,
        breaker_id: &str,
        reason: &str,
    ) -> Result<PivotPlan> {
        ensure!(
            oid(base) && fact.len() == 64 && oid(fact) && valid_id(breaker_id) && reason_ok(reason),
            "invalid Pivot approval inputs"
        );
        let tx = self.db.transaction()?;
        ensure!(
            read_pending(&tx)?.is_none() && read_intent(&tx)?.is_none(),
            "recover the pending Git transaction first"
        );
        bound_repo(&tx, repo)?;
        let checkpoint = current_checkpoint(&tx)?;
        ensure!(
            review.event_to_inclusive > 0
                && last_seq(&tx)? == review.event_to_inclusive
                && checkpoint == review.checkpoint
                && review.event_from_exclusive == checkpoint.as_ref().map_or(0, |c| c.event_seq)
                && review.to_git_commit.as_deref() == Some(base),
            "Fact Pack does not match current checkpoint, ledger generation and Git main"
        );
        let latest:Option<String>=tx.query_row("SELECT git_commit FROM events WHERE git_commit IS NOT NULL ORDER BY event_seq DESC LIMIT 1",[],|r|r.get(0)).optional()?;
        ensure!(
            latest.as_deref() == Some(base),
            "ledger does not attest current main"
        );
        let (breaker, inbox, target_sha256) = read_target(&tx, breaker_id)?;
        ensure!(
            review.reviewed_breaker(breaker_id) == Some(&breaker)
                && review.reviewed_inbox(&inbox.inbox_id) == Some(&inbox),
            "selected breaker and linked inbox must both appear unchanged in retained review"
        );
        let plan = PivotPlan {
            repo: repo.into(),
            base_commit: base.into(),
            fact_sha256: fact.into(),
            event_from_exclusive: review.event_from_exclusive,
            reviewed_through: review.event_to_inclusive,
            checkpoint,
            presentation_truncated: review.presentation_truncated,
            breaker,
            inbox_id: inbox.inbox_id,
            decision_reason: reason.into(),
            target_sha256,
        };
        tx.commit()?;
        Ok(plan)
    }

    pub fn pivot_intent(&self) -> Result<Option<PivotIntent>> {
        read_pending(&self.db)
    }

    pub fn recorded_pivot(&self, adr_id: &str) -> Result<Option<(PivotPlan, PivotResult)>> {
        self.db.query_row("SELECT plan_json,git_commit,event_seq,reviewed_through FROM recorded_pivots WHERE adr_id=?1",[adr_id],|r| Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,i64>(2)?,r.get::<_,i64>(3)?))).optional()?.map(|(plan,git_commit,event_seq,reviewed_through)|Ok((serde_json::from_str(&plan)?,PivotResult{adr_id:adr_id.into(),git_commit,event_seq,reviewed_through,breaker_state:"HALF_OPEN",probes_started:false}))).transpose()
    }

    pub fn prepare_pivot(&mut self, intent: &PivotIntent) -> Result<()> {
        self.write(|tx| {
            ensure!(intent.adr_id==intent.plan.adr_id()? && oid(&intent.new_commit) && intent.new_commit!=intent.plan.base_commit,"invalid prepared Pivot");
            ensure!(read_pending(tx)?.is_none() && read_intent(tx)?.is_none(),"recover the pending Git transaction first");
            confirm_context(tx,&intent.plan,true)?;
            tx.execute("INSERT INTO pivot_resume_intent(singleton,adr_id,plan_json,new_commit) VALUES(1,?1,?2,?3)",params![intent.adr_id,serde_json::to_string(&intent.plan)?,intent.new_commit])?;
            Ok(())
        })
    }

    /// The trusted coordinator holds the repository lock. The callback verifies
    /// the exact prepared commit and its adoption (or atomically advances main).
    /// Hold the SQLite writer lock across CAS to close the pre-CAS ledger race.
    pub fn land_pivot(
        &mut self,
        intent: &PivotIntent,
        already_adopted: bool,
        adopt: impl FnOnce() -> Result<()>,
    ) -> Result<PivotResult> {
        self.write(|tx| {
            ensure!(read_pending(tx)?.as_ref()==Some(intent),"Pivot intent changed or is missing");
            let p=&intent.plan;
            confirm_context(tx,p,!already_adopted)?;
            adopt()?;
            let change=Change{actor:"operator:pivot",git_commit:Some(&intent.new_commit)};
            let id=&intent.adr_id;
            let seq=event(tx,change,"PIVOT_APPLIED","adr",id,json!({"action":"RESUME_AS_IS","fact_sha256":p.fact_sha256,"reviewed_through":p.reviewed_through,"breaker_id":p.breaker.breaker_id,"inbox_id":p.inbox_id,"probes_started":false}))?;
            tx.execute(&format!("UPDATE circuit_breakers SET state='HALF_OPEN',updated_at={NOW} WHERE breaker_id=?1"),[&p.breaker.breaker_id])?;
            event(tx,change,"BREAKER_STATE_CHANGED","breaker",&p.breaker.breaker_id,json!({"previous_state":"TRIPPED","state":"HALF_OPEN","adr_id":id,"probe_status":"AWAITING_REVIEW"}))?;
            tx.execute(&format!("UPDATE decision_inbox SET status='RESOLVED',resolved_adr=?2,resolved_at={NOW} WHERE inbox_id=?1"),params![p.inbox_id,id])?;
            tx.execute("UPDATE human_checkpoints SET is_current=0 WHERE is_current=1",[])?;
            // Never hide events that arrived between a successful CAS and recovery.
            tx.execute(&format!("INSERT INTO human_checkpoints(cursor_id,updated_at,event_seq,git_commit,last_adr_id,is_current) VALUES(?1,{NOW},?2,?3,?1,1)"),params![id,p.reviewed_through,intent.new_commit])?;
            tx.execute("INSERT INTO recorded_pivots(adr_id,plan_json,git_commit,event_seq,reviewed_through) VALUES(?1,?2,?3,?4,?5)",params![id,serde_json::to_string(p)?,intent.new_commit,seq,p.reviewed_through])?;
            tx.execute("DELETE FROM pivot_resume_intent WHERE singleton=1",[])?;
            Ok(PivotResult{adr_id:id.clone(),git_commit:intent.new_commit.clone(),event_seq:seq,reviewed_through:p.reviewed_through,breaker_state:"HALF_OPEN",probes_started:false})
        })
    }

    /// The trusted Git coordinator must first establish that main did not adopt the prepared commit.
    pub fn abandon_unadopted_pivot(&mut self, intent: &PivotIntent) -> Result<()> {
        self.write(|tx| {
            ensure!(
                read_pending(tx)?.as_ref() == Some(intent),
                "Pivot intent changed or is missing"
            );
            tx.execute("DELETE FROM pivot_resume_intent WHERE singleton=1", [])?;
            Ok(())
        })
    }
}
