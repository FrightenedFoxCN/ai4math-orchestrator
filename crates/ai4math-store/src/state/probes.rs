//! Durable coordinator-only half-open probe workflow. Only native sealed evidence closes a breaker.
use super::*;
use ai4math_evidence::BreakerProbeEvidence;
use serde::Deserialize;
use sha2::{Digest, Sha256};

const MAX_ATTEMPTS: i64 = 8;
const MAX_APPROVALS: i64 = 32;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BreakerProbeScope {
    pub breaker_id: String,
    pub adr_id: String,
    pub adr_commit: String,
    pub module_path: String,
    pub definition: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BreakerProbePlan {
    pub scope: BreakerProbeScope,
    pub git_commit: String,
    pub sources_sha256: String,
    pub cases: Vec<String>,
    pub planned_at_event_seq: i64,
    repo: String,
}

impl BreakerProbePlan {
    pub fn approval_id(&self) -> Result<String> {
        Ok(format!(
            "BP-{:x}",
            Sha256::digest(serde_json::to_vec(&("ai4math:breaker-probe:v1", self))?)
        ))
    }
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct BreakerProbeBatch {
    pub approval_id: String,
    pub plan: BreakerProbePlan,
    pub status: String,
    pub attempts: i64,
    pub receipt_sha256: Option<String>,
}

#[derive(Debug)]
pub struct BreakerProbeAttempt {
    batch: BreakerProbeBatch,
}

fn check_repo(db: &Connection, repo: &str) -> Result<()> {
    let bound: Option<String> = db
        .query_row("SELECT repo FROM managed_repo WHERE singleton=1", [], |r| {
            r.get(0)
        })
        .optional()?;
    ensure!(
        bound.as_deref() == Some(repo),
        "ledger is not bound to this managed repository"
    );
    Ok(())
}

fn check_no_git_intent(db: &Connection) -> Result<()> {
    ensure!(
        super::pivot::read_pending(db)?.is_none() && read_intent(db)?.is_none(),
        "recover the pending Git transaction before running breaker probes"
    );
    Ok(())
}

fn read_scope(db: &Connection, id: &str) -> Result<BreakerProbeScope> {
    db.query_row(
        "SELECT b.breaker_id,p.adr_id,p.git_commit,b.module_path,b.target_definition_id
        FROM circuit_breakers b JOIN decision_inbox i ON b.linked_inbox_id=i.inbox_id
        JOIN recorded_pivots p ON p.adr_id=i.resolved_adr
        WHERE b.breaker_id=?1 AND b.state='HALF_OPEN' AND b.engine='lean4' AND i.status='RESOLVED'
        AND json_extract(p.plan_json,'$.breaker.breaker_id')=b.breaker_id
        AND json_extract(p.plan_json,'$.breaker.module_path')=b.module_path
        AND json_extract(p.plan_json,'$.breaker.target_definition_id')=b.target_definition_id",
        [id],
        |r| {
            Ok(BreakerProbeScope {
                breaker_id: r.get(0)?,
                adr_id: r.get(1)?,
                adr_commit: r.get(2)?,
                module_path: r.get(3)?,
                definition: r.get(4)?,
            })
        },
    )
    .optional()?
    .context("expected a HALF_OPEN Lean breaker with a recorded Pivot ADR")
}

fn latest_batch(db: &Connection, id: &str) -> Result<Option<BreakerProbeBatch>> {
    let row = db
        .query_row(
            "SELECT approval_id,plan_json,status,attempts,receipt_sha256 FROM breaker_probe_batches
        WHERE breaker_id=?1 ORDER BY arm_event_seq DESC LIMIT 1",
            [id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, Option<String>>(4)?,
                ))
            },
        )
        .optional()?;
    row.map(|(approval_id, plan, status, attempts, receipt_sha256)| {
        let plan: BreakerProbePlan = serde_json::from_str(&plan)?;
        ensure!(
            plan.approval_id()? == approval_id && plan.scope.breaker_id == id,
            "invalid durable breaker probe plan"
        );
        Ok(BreakerProbeBatch {
            approval_id,
            plan,
            status,
            attempts,
            receipt_sha256,
        })
    })
    .transpose()
}

fn declaration(s: &str) -> bool {
    s.len() <= 128
        && s.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}
fn lowercase_hex(s: &str) -> bool {
    s.bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn probe_event(
    tx: &Transaction<'_>,
    p: &BreakerProbePlan,
    kind: &str,
    status: &str,
    state: &str,
    receipt: Option<&str>,
) -> Result<i64> {
    let mut payload = json!({"state":state,"adr_id":p.scope.adr_id,"approval_id":p.approval_id()?,"probe_status":status,
        "probe_required":p.cases.len(),"probe_passed":if status=="PASSED" {p.cases.len()} else {0}});
    if let Some(digest) = receipt {
        payload["receipt_sha256"] = json!(digest);
    }
    let c = Change {
        actor: "operator:breaker-probe",
        // On failure the old approved commit need not still be current.
        git_commit: (status != "NEEDS_PROBE").then_some(p.git_commit.as_str()),
    };
    event(tx, c, kind, "breaker", &p.scope.breaker_id, payload)
}

impl StateStore {
    pub fn breaker_probe_scope(&self, repo: &str, id: &str) -> Result<BreakerProbeScope> {
        check_repo(&self.db, repo)?;
        read_scope(&self.db, id)
    }

    /// The trusted Git coordinator verifies ancestry and exact ordered source bytes
    /// before calling. Preview never schedules or changes a breaker.
    pub fn preview_breaker_probes(
        &mut self,
        repo: &str,
        scope: &BreakerProbeScope,
        commit: &str,
        sources_sha256: &str,
        cases: &[&str],
    ) -> Result<BreakerProbePlan> {
        ensure!(
            matches!(commit.len(), 40 | 64)
                && lowercase_hex(commit)
                && sources_sha256.len() == 64
                && lowercase_hex(sources_sha256),
            "invalid probe snapshot or source digest"
        );
        ensure!(
            (1..=2).contains(&cases.len())
                && cases.iter().all(|c| declaration(c))
                && (cases.len() == 1 || cases[0] != cases[1])
                && declaration(&scope.definition),
            "provide one or two distinct root Lean theorem names and a root definition"
        );
        let tx = self.db.transaction()?;
        check_repo(&tx, repo)?;
        check_no_git_intent(&tx)?;
        ensure!(
            &read_scope(&tx, &scope.breaker_id)? == scope,
            "breaker scope changed during preview"
        );
        let p = BreakerProbePlan {
            scope: scope.clone(),
            git_commit: commit.into(),
            sources_sha256: sources_sha256.into(),
            cases: cases.iter().map(|c| (*c).to_owned()).collect(),
            planned_at_event_seq: last_seq(&tx)?,
            repo: repo.into(),
        };
        tx.commit()?;
        Ok(p)
    }

    pub fn arm_breaker_probes(&mut self, p: &BreakerProbePlan, approved: &str) -> Result<i64> {
        ensure!(
            p.approval_id()? == approved,
            "probe approval does not match preview"
        );
        self.write(|tx|{
            check_repo(tx,&p.repo)?;
            check_no_git_intent(tx)?;
            ensure!(last_seq(tx)?==p.planned_at_event_seq && read_scope(tx,&p.scope.breaker_id)?==p.scope,"ledger or breaker changed; repeat probe preview");
            let existing:i64=tx.query_row("SELECT count(*) FROM breaker_probe_batches WHERE breaker_id=?1",[&p.scope.breaker_id],|r|r.get(0))?;
            ensure!(existing<MAX_APPROVALS,"probe approval budget exhausted for this breaker");
            let seq=probe_event(tx,p,"BREAKER_PROBES_ARMED","READY","HALF_OPEN",None)?;
            tx.execute("INSERT INTO breaker_probe_batches(approval_id,breaker_id,adr_id,plan_json,arm_event_seq,required,status) VALUES(?1,?2,?3,?4,?5,?6,'READY')",params![approved,p.scope.breaker_id,p.scope.adr_id,serde_json::to_string(p)?,seq,p.cases.len()])?;
            Ok(seq)
        })
    }

    pub fn current_breaker_probe_batch(&self, id: &str) -> Result<Option<BreakerProbeBatch>> {
        latest_batch(&self.db, id)
    }

    /// The caller owns the per-repository execution lock. A prior RUNNING record
    /// under that lock is incomplete, not proof of a live or successful process.
    pub fn start_breaker_probes(
        &mut self,
        id: &str,
        approval: &str,
        current_main: &str,
    ) -> Result<BreakerProbeAttempt> {
        self.write(|tx|{
            check_no_git_intent(tx)?;
            let mut batch=latest_batch(tx,id)?.context("approve a breaker probe batch first")?;
            let p=&batch.plan;
            check_repo(tx,&p.repo)?;
            ensure!(batch.approval_id==approval && p.git_commit==current_main && read_scope(tx,id)?==p.scope,"probe batch is no longer the approved current Git scope");
            ensure!(batch.status!="PASSED" && batch.attempts<MAX_ATTEMPTS,"probe attempt budget exhausted or already passed; review a new batch if still half-open");
            tx.execute("UPDATE breaker_probe_batches SET attempts=attempts+1,status='RUNNING' WHERE approval_id=?1",[approval])?;
            probe_event(tx,p,"BREAKER_PROBES_STARTED","RUNNING","HALF_OPEN",None)?;
            batch.status="RUNNING".into();
            batch.attempts+=1;
            Ok(BreakerProbeAttempt{batch})
        })
    }

    /// Trusted coordinator rechecks main while holding the repository lock.
    /// Failed, unavailable or interrupted probes never grant release or change trust.
    pub fn finish_breaker_probes(
        &mut self,
        attempt: &BreakerProbeAttempt,
        current_main: &str,
        evidence: Option<&dyn BreakerProbeEvidence>,
    ) -> Result<i64> {
        self.write(|tx|{
            let b=&attempt.batch;
            let p=&b.plan;
            let id=&p.scope.breaker_id;
            check_repo(tx,&p.repo)?;
            check_no_git_intent(tx)?;
            ensure!(latest_batch(tx,id)?.as_ref()==Some(b) && read_scope(tx,id)?==p.scope,"stale or superseded probe attempt");
            let (kind,status,state,receipt)=if let Some(e)=evidence {
                ensure!(current_main==p.git_commit && e.base_commit()==current_main && e.file_path()==p.scope.module_path
                    && e.definition()==p.scope.definition && e.cases()==p.cases && e.sources_sha256()==p.sources_sha256,
                    "sealed probe evidence does not match the approved current snapshot");
                ("BREAKER_PROBES_PASSED","PASSED","CLOSED",Some(e.receipt_sha256()))
            } else {
                ("BREAKER_PROBES_FAILED","NEEDS_PROBE","HALF_OPEN",None)
            };
            tx.execute("UPDATE breaker_probe_batches SET status=?1,receipt_sha256=?2 WHERE approval_id=?3",params![status,receipt,b.approval_id])?;
            if state=="CLOSED" {
                ensure!(tx.execute(&format!("UPDATE circuit_breakers SET state='CLOSED',updated_at={NOW} WHERE breaker_id=?1 AND state='HALF_OPEN'"),[id])?==1,"breaker changed state while completing probes");
            }
            let seq=probe_event(tx,p,kind,status,state,receipt)?;
            Ok(seq)
        })
    }
}
