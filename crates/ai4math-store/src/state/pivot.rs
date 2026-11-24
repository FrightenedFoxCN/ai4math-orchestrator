//! Trusted operator's durable Pivot decisions. A journal row is not a Git receipt.
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

/// The operator-approved state transition for a selected TRIPPED breaker.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PivotAction {
    #[default]
    ResumeAsIs,
    ArchiveBreaker,
}

impl PivotAction {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ResumeAsIs => "RESUME_AS_IS",
            Self::ArchiveBreaker => "ARCHIVE_BREAKER",
        }
    }

    pub const fn target_state(self) -> &'static str {
        match self {
            Self::ResumeAsIs => "HALF_OPEN",
            Self::ArchiveBreaker => "ARCHIVED",
        }
    }

    fn is_resume(&self) -> bool {
        *self == Self::ResumeAsIs
    }

    fn adr_domain(self) -> &'static str {
        match self {
            Self::ResumeAsIs => "RESUME_AS_IS/v1",
            Self::ArchiveBreaker => "ARCHIVE_BREAKER/v1",
        }
    }
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
    // Resume plans are also the legacy on-disk ADR identity input. Preserve their bytes.
    #[serde(default, skip_serializing_if = "PivotAction::is_resume")]
    pub action: PivotAction,
    target_sha256: String,
}

impl PivotPlan {
    pub fn adr_id(&self) -> Result<String> {
        Ok(format!(
            "ADR-PIVOT-{}",
            digest(&(self.action.adr_domain(), self))?
        ))
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
        self.preview_pivot(
            repo,
            base,
            fact,
            review,
            breaker_id,
            reason,
            PivotAction::ResumeAsIs,
        )
    }

    pub fn preview_pivot(
        &mut self,
        repo: &str,
        base: &str,
        fact: &str,
        review: &RetainedReview,
        breaker_id: &str,
        reason: &str,
        action: PivotAction,
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
            action,
            target_sha256,
        };
        tx.commit()?;
        Ok(plan)
    }

    pub fn pivot_intent(&self) -> Result<Option<PivotIntent>> {
        read_pending(&self.db)
    }

    pub fn recorded_pivot(&self, adr_id: &str) -> Result<Option<(PivotPlan, PivotResult)>> {
        self.db
            .query_row(
                "SELECT plan_json,git_commit,event_seq,reviewed_through FROM recorded_pivots WHERE adr_id=?1",
                [adr_id],
                |r| Ok((r.get::<_, String>(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?
            .map(|(plan, git_commit, event_seq, reviewed_through)| {
                let plan: PivotPlan = serde_json::from_str(&plan)?;
                let result = PivotResult {
                    adr_id: adr_id.into(),
                    git_commit,
                    event_seq,
                    reviewed_through,
                    breaker_state: plan.action.target_state(),
                    probes_started: false,
                };
                Ok((plan, result))
            })
            .transpose()
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
            let breaker_state = p.action.target_state();
            let seq=event(tx,change,"PIVOT_APPLIED","adr",id,json!({"action":p.action.as_str(),"fact_sha256":p.fact_sha256,"reviewed_through":p.reviewed_through,"breaker_id":p.breaker.breaker_id,"inbox_id":p.inbox_id,"probes_started":false}))?;
            tx.execute(&format!("UPDATE circuit_breakers SET state=?2,updated_at={NOW} WHERE breaker_id=?1"),params![p.breaker.breaker_id,breaker_state])?;
            let mut transition = json!({"previous_state":"TRIPPED","state":breaker_state,"adr_id":id});
            let breaker_event = match p.action {
                PivotAction::ResumeAsIs => {
                    transition["probe_status"] = json!("AWAITING_REVIEW");
                    "BREAKER_STATE_CHANGED"
                }
                PivotAction::ArchiveBreaker => "BREAKER_ARCHIVED",
            };
            event(tx,change,breaker_event,"breaker",&p.breaker.breaker_id,transition)?;
            tx.execute(&format!("UPDATE decision_inbox SET status='RESOLVED',resolved_adr=?2,resolved_at={NOW} WHERE inbox_id=?1"),params![p.inbox_id,id])?;
            tx.execute("UPDATE human_checkpoints SET is_current=0 WHERE is_current=1",[])?;
            // Never hide events that arrived between a successful CAS and recovery.
            tx.execute(&format!("INSERT INTO human_checkpoints(cursor_id,updated_at,event_seq,git_commit,last_adr_id,is_current) VALUES(?1,{NOW},?2,?3,?1,1)"),params![id,p.reviewed_through,intent.new_commit])?;
            tx.execute("INSERT INTO recorded_pivots(adr_id,plan_json,git_commit,event_seq,reviewed_through) VALUES(?1,?2,?3,?4,?5)",params![id,serde_json::to_string(p)?,intent.new_commit,seq,p.reviewed_through])?;
            tx.execute("DELETE FROM pivot_resume_intent WHERE singleton=1",[])?;
            Ok(PivotResult{adr_id:id.clone(),git_commit:intent.new_commit.clone(),event_seq:seq,reviewed_through:p.reviewed_through,breaker_state,probes_started:false})
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_resume_plan_keeps_its_exact_bytes_and_adr_id() -> Result<()> {
        let legacy = concat!(
            r#"{"repo":"/repo","base_commit":"1111111111111111111111111111111111111111","#,
            r#""fact_sha256":"2222222222222222222222222222222222222222222222222222222222222222","#,
            r#""event_from_exclusive":3,"reviewed_through":4,"checkpoint":{"cursor_id":"old-adr","event_seq":3,"#,
            r#""git_commit":"0000000000000000000000000000000000000000","last_adr_id":"old-adr"},"#,
            r#""presentation_truncated":false,"breaker":{"breaker_id":"test","target_definition_id":"Defs.name","#,
            r#""module_path":"Defs","engine":"lean4","state":"TRIPPED","updated_at":"now"},"#,
            r#""inbox_id":"DEF-test","decision_reason":"reviewed","#,
            r#""target_sha256":"3333333333333333333333333333333333333333333333333333333333333333"}"#,
        );
        let mut plan: PivotPlan = serde_json::from_str(legacy)?;
        assert_eq!(plan.action, PivotAction::ResumeAsIs);
        assert_eq!(serde_json::to_string(&plan)?, legacy);
        let legacy_adr_id = plan.adr_id()?;
        assert_eq!(
            legacy_adr_id,
            "ADR-PIVOT-4329b8b16232396635ec2d54058db8e2244401fbd89b7314232f3d4008e41d4d"
        );
        assert_eq!(serde_json::to_value(plan.action)?, "RESUME_AS_IS");

        plan.action = PivotAction::ArchiveBreaker;
        let archive = serde_json::to_value(&plan)?;
        assert_eq!(archive["action"], "ARCHIVE_BREAKER");
        assert_eq!(serde_json::from_value::<PivotPlan>(archive)?, plan);
        assert_ne!(plan.adr_id()?, legacy_adr_id);
        assert_ne!(
            plan.adr_id()?,
            format!("ADR-PIVOT-{}", digest(&("RESUME_AS_IS/v1", &plan))?)
        );
        Ok(())
    }

    #[test]
    fn archive_recovery_finalizes_only_the_reviewed_breaker_and_inbox() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("state.sqlite");
        let mut store = StateStore::open(&path)?;
        let repo = "/repo";
        let base = "1".repeat(40);
        let fact = "2".repeat(64);
        let new_commit = "3".repeat(40);
        store.register_tripped_breaker(
            repo,
            &base,
            "chosen",
            "Defs.name",
            "Defs",
            "lean4",
            "incident",
            false,
        )?;
        store.register_tripped_breaker(
            repo,
            &base,
            "other",
            "Other.name",
            "Other",
            "lean4",
            "different incident",
            false,
        )?;
        let untouched = read_target(&store.db, "other")?;
        let facts = store.briefing_facts()?;
        let review = RetainedReview {
            event_from_exclusive: facts.event_from_exclusive,
            event_to_inclusive: facts.event_to_inclusive,
            to_git_commit: facts.to_git_commit,
            checkpoint: facts.checkpoint,
            presentation_truncated: false,
            breakers: facts.breakers.items,
            inbox: facts.inbox.items,
        };
        let resume = store.preview_resume(repo, &base, &fact, &review, "chosen", "reviewed")?;
        assert_eq!(resume.action, PivotAction::ResumeAsIs);
        let plan = store.preview_pivot(
            repo,
            &base,
            &fact,
            &review,
            "chosen",
            "reviewed",
            PivotAction::ArchiveBreaker,
        )?;
        assert_eq!(
            plan,
            PivotPlan {
                action: PivotAction::ArchiveBreaker,
                ..resume
            }
        );
        let intent = PivotIntent {
            adr_id: plan.adr_id()?,
            plan,
            new_commit: new_commit.clone(),
        };
        store.prepare_pivot(&intent)?;

        // Fail after the adoption callback and just before journaling completes. No
        // partial SQLite finalization may survive, and the intent must be recoverable.
        store.db.execute_batch(
            "CREATE TRIGGER interrupt_pivot BEFORE INSERT ON recorded_pivots
            BEGIN SELECT RAISE(ABORT, 'simulated interrupted finalization'); END;",
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
                .contains("simulated interrupted finalization")
        );
        assert_eq!(last_seq(&store.db)?, intent.plan.reviewed_through);
        assert_eq!(current_checkpoint(&store.db)?, intent.plan.checkpoint);
        assert_eq!(read_target(&store.db, "chosen")?.0, intent.plan.breaker);
        assert!(store.recorded_pivot(&intent.adr_id)?.is_none());
        store.db.execute_batch("DROP TRIGGER interrupt_pivot")?;
        drop(store);

        let mut store = StateStore::open(&path)?;
        assert_eq!(store.pivot_intent()?.as_ref(), Some(&intent));
        let change = Change {
            actor: "test",
            git_commit: None,
        };
        for (kind, entity, id) in [
            ("PIVOT_APPLIED", "adr", intent.adr_id.as_str()),
            ("BREAKER_ARCHIVED", "breaker", "chosen"),
        ] {
            let err = store
                .append_event(change, kind, entity, id, json!({}))
                .unwrap_err();
            assert!(err.to_string().contains("finalized ADR transaction"));
        }
        let unreviewed =
            store.append_event(change, "UNREVIEWED_TEST_EVENT", "test", "test", json!({}))?;
        let result = store.land_pivot(&intent, true, || Ok(()))?;
        assert_eq!(result.breaker_state, "ARCHIVED");
        assert!(!result.probes_started);
        assert_eq!(result.reviewed_through, review.event_to_inclusive);
        assert!(store.pivot_intent()?.is_none());
        assert_eq!(
            store.recorded_pivot(&intent.adr_id)?,
            Some((intent.plan.clone(), result.clone()))
        );
        assert_eq!(read_target(&store.db, "other")?, untouched);
        assert!(store.module_breaker_gated("lean4", "Defs")?);
        assert!(store.breaker_probe_scope(repo, "chosen").is_err());
        assert!(store.current_breaker_probe_batch("chosen")?.is_none());

        let resolved: (String, String, bool) = store.db.query_row(
            "SELECT status,resolved_adr,resolved_at IS NOT NULL FROM decision_inbox WHERE inbox_id=?1",
            [&intent.plan.inbox_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        assert_eq!(resolved, ("RESOLVED".into(), intent.adr_id.clone(), true));
        let facts = store.briefing_for_alerts()?;
        assert_eq!(
            facts.checkpoint,
            Some(Checkpoint {
                cursor_id: intent.adr_id.clone(),
                event_seq: review.event_to_inclusive,
                git_commit: new_commit,
                last_adr_id: Some(intent.adr_id.clone()),
            })
        );
        assert_eq!(
            facts
                .events
                .items
                .iter()
                .map(|e| (e.event_type.as_str(), e.event_seq))
                .collect::<Vec<_>>(),
            vec![
                ("UNREVIEWED_TEST_EVENT", unreviewed),
                ("PIVOT_APPLIED", result.event_seq),
                ("BREAKER_ARCHIVED", result.event_seq + 1),
            ]
        );
        assert_eq!(
            facts.events.items[1].payload,
            json!({
                "action":"ARCHIVE_BREAKER", "fact_sha256":fact, "reviewed_through":review.event_to_inclusive,
                "breaker_id":"chosen", "inbox_id":"DEF-chosen", "probes_started":false,
            })
        );
        assert!(facts.alert_sources.is_empty());
        let changes =
            store.breaker_events_through(review.event_to_inclusive, facts.event_to_inclusive, 2)?;
        assert_eq!(changes.items.len(), 1);
        assert_eq!(changes.items[0].event_type, "BREAKER_ARCHIVED");
        assert_eq!(
            changes.items[0].payload,
            json!({"previous_state":"TRIPPED", "state":"ARCHIVED", "adr_id":intent.adr_id})
        );
        Ok(())
    }
}
