use crate::{
    Trust, open_db,
    refutation::{self, RefutationRecord, WitnessInput, WitnessSpecBinding},
};
use ai4math_broker::VerifiedWitness;
use ai4math_core::{aggregate_plan, best_proof_plan};
use anyhow::{Context, Result, ensure};
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Row, Transaction, TransactionBehavior,
    config::DbConfig, params,
};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    time::Duration,
};

mod claim_replay;
mod graph_audit;
mod graph_check;
mod graph_replay;
mod infra_rfc;
mod ledger_replay;
mod migrations;
mod oracle;
mod pivot;
mod plan_replay;
mod probes;
pub(crate) mod regression;
mod reviewed_lean;
mod sediment_adaptation;
mod sediment_lineage;
mod sediment_recheck;
mod sweep;
mod t3_baseline;
mod t3_follow;
mod tasks;
mod trust_debt;
pub use claim_replay::{ClaimReplay, ReplayedClaim};
pub use graph_audit::{GraphAudit, GraphMismatch, GraphMismatchKind};
pub use graph_check::{GraphSemanticCheck, GraphSemanticMismatch};
pub use graph_replay::GraphReplay;
pub use infra_rfc::{INFRA_RFC_MAX_BYTES, InfraRfcRecord, InfraRfcStatus, parse_infra_rfc_draft};
pub use oracle::{OracleApproval, RetainedOracleObservation};
pub use pivot::{
    BreakerTripReport, PivotAction, PivotArchivePlan, PivotIntent, PivotLessonDraft,
    PivotLessonPage, PivotLessonWarning, PivotPlan, PivotResult,
};
pub use plan_replay::{PlanReplay, ReplayedPlan, ReplayedPlanDependency};
pub use probes::{BreakerProbeAttempt, BreakerProbeBatch, BreakerProbePlan, BreakerProbeScope};
pub use reviewed_lean::{LeanSpecComparison, LeanSpecRecheck, ReviewedLeanSpec};
pub use sediment_adaptation::{SedimentAdaptationApproval, SedimentAdaptationPlan};
pub use sediment_lineage::{SedimentCoreEvidence, SedimentLineage, SedimentLineageInput};
pub use sediment_recheck::{RetainedSedimentReplay, SedimentReplayScope};
pub use sweep::{T3SweepState, T3SweepStatus, T3SweepStep};
pub use t3_baseline::T3DiscoveryBaseline;
pub(crate) use tasks::scheduler_snapshot;
pub use tasks::{
    NewTask, OracleTaskHold, TaskCandidate, TaskCandidateSource, TaskLease, TaskRunOutcome,
    TaskSchedulerStatus, TaskTrack, TaskTrackStatus,
};
pub use trust_debt::{
    TrustDebtCounts, TrustDebtFinding, TrustDebtKind, TrustDebtRole, TrustDebtScan,
};

const NOW: &str = "strftime('%Y-%m-%dT%H:%M:%fZ','now')";
const DDL: &str = include_str!("../../../schemas/state-store.sql");
const T3_OPERATOR: &str = "operator:t3";

pub struct StateStore {
    pub(crate) db: Connection,
}

#[derive(Debug, Clone, Copy)]
pub struct Change<'a> {
    pub actor: &'a str,
    pub git_commit: Option<&'a str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromotionIntent {
    pub repo: String,
    pub claim_id: String,
    pub base_commit: String,
    pub new_commit: String,
    pub file_path: String,
    pub target_decl: String,
    pub proof_body: String,
}

/// Operator-only replay input. Use Git and Broker to establish fresh evidence; this row is not a receipt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedProofRecipe {
    pub claim_id: String,
    pub base_commit: String,
    pub promoted_commit: String,
    pub file_path: String,
    pub target_decl: String,
    pub proof_body: String,
    pub source_sha256: String,
}

fn read_intent(db: &Connection) -> Result<Option<PromotionIntent>> {
    Ok(db.query_row("SELECT repo,claim_id,base_commit,new_commit,file_path,target_decl,proof_body FROM promotion_intent WHERE singleton=1", [], |r| Ok(PromotionIntent {
        repo:r.get(0)?,claim_id:r.get(1)?,base_commit:r.get(2)?,new_commit:r.get(3)?,file_path:r.get(4)?,target_decl:r.get(5)?,proof_body:r.get(6)?,
    })).optional()?)
}

#[derive(Debug, Serialize)]
pub struct StateStatus {
    pub claims: i64,
    pub proof_plans: i64,
    pub stale_claims: i64,
    pub pending_decisions: i64,
    pub last_event_seq: i64,
}

#[derive(Debug, Serialize)]
pub struct Event {
    pub event_seq: i64,
    pub occurred_at: String,
    pub event_type: String,
    pub entity_type: String,
    pub entity_id: String,
    pub git_commit: Option<String>,
    pub actor: String,
    pub payload: Value,
}

#[derive(Debug)]
pub struct NewClaim<'a> {
    pub id: &'a str,
    pub title: &'a str,
    pub informal_statement: &'a str,
    pub engine: &'a str,
    pub formal_target: Option<&'a str>,
    pub module_path: Option<&'a str>,
    pub critical: bool,
}

#[derive(Debug, Serialize, Clone)]
pub struct ClaimState {
    pub id: String,
    pub engine: String,
    pub formal_target: Option<String>,
    pub module_path: Option<String>,
    pub critical: bool,
    pub local_trust: Trust,
    pub effective_trust: Trust,
    pub prior_trust: Option<Trust>,
    pub stale: bool,
    pub effective_stale: bool,
    pub taint_root_commit: Option<String>,
    pub receipt_id: Option<String>,
    pub wiki_id: Option<String>,
}

const CLAIM_SELECT: &str = "SELECT claim_id, engine, formal_target, module_path, is_critical_path,
    local_trust_level, effective_trust_level, prior_trust_level, stale, effective_stale,
    taint_root_commit, receipt_id, wiki_id FROM claims";

fn trust(row: &Row<'_>, idx: usize) -> rusqlite::Result<Trust> {
    let s: String = row.get(idx)?;
    s.parse().map_err(|_| rusqlite::Error::InvalidQuery)
}

fn read_claim(r: &Row<'_>) -> rusqlite::Result<ClaimState> {
    Ok(ClaimState {
        id: r.get(0)?,
        engine: r.get(1)?,
        formal_target: r.get(2)?,
        module_path: r.get(3)?,
        critical: r.get(4)?,
        local_trust: trust(r, 5)?,
        effective_trust: trust(r, 6)?,
        prior_trust: if r.get::<_, Option<String>>(7)?.is_some() {
            Some(trust(r, 7)?)
        } else {
            None
        },
        stale: r.get(8)?,
        effective_stale: r.get(9)?,
        taint_root_commit: r.get(10)?,
        receipt_id: r.get(11)?,
        wiki_id: r.get(12)?,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanStatus {
    Active,
    Proved,
    SuspendedSoftPrune,
    InvalidatedByRefutation,
    ArchivedByAdr,
}

impl PlanStatus {
    fn sql(self) -> &'static str {
        match self {
            Self::Active => "ACTIVE",
            Self::Proved => "PROVED",
            Self::SuspendedSoftPrune => "SUSPENDED_SOFT_PRUNE",
            Self::InvalidatedByRefutation => "INVALIDATED_BY_REFUTATION",
            Self::ArchivedByAdr => "ARCHIVED_BY_ADR",
        }
    }
}

pub struct NewPlan<'a> {
    pub id: &'a str,
    pub target: &'a str,
    pub name: &'a str,
    pub track: &'a str,
    pub status: PlanStatus,
    /// Pairs of dependency claim ID and DDL dependency kind.
    pub dependencies: &'a [(&'a str, &'a str)],
}

/// A store-local projection of sealed broker evidence, not a verifier or a Git receipt.
/// The trusted coordinator must verify the physical Git snapshot and commit.
#[derive(Debug)]
pub struct VerifiedReceipt {
    id: String,
    issued_at: String,
    tier: String,
    engine: String,
    base_commit: String,
    patch_sha256: String,
    target_decl: String,
    file_path: String,
    allowlist_passed: bool,
    axioms_used: Vec<String>,
    external_checker_passed: Option<bool>,
    compile_seconds: f64,
    max_heartbeats: Option<i64>,
    receipt_sha256: String,
    raw: Value,
    regression_checked: bool,
    lean_comparison: Option<reviewed_lean::ComparedSpec>,
}

impl VerifiedReceipt {
    /// Project a sealed in-process broker result, not a caller-supplied JSON receipt.
    pub fn from_promotion(
        candidate: &ai4math_broker::VerifiedCandidate,
        artifacts: &ai4math_broker::RegressionArtifacts,
    ) -> Result<Self> {
        ensure!(
            artifacts.matches(candidate),
            "regression did not check this candidate"
        );
        let mut receipt = Self::from_broker(candidate)?;
        receipt.regression_checked = true;
        Ok(receipt)
    }

    pub fn from_broker(candidate: &ai4math_broker::VerifiedCandidate) -> Result<Self> {
        let r = candidate.receipt();
        ensure!(
            r.matches(
                r.base_commit(),
                r.file_path(),
                candidate.source().as_bytes()
            ),
            "broker receipt binding mismatch"
        );
        let timestamp = i64::try_from(r.issued_unix_seconds())?;
        let issued_at = Connection::open_in_memory()?.query_row(
            "SELECT strftime('%Y-%m-%dT%H:%M:%SZ', ?1, 'unixepoch')",
            [timestamp],
            |row| row.get(0),
        )?;
        Ok(Self {
            id: r.receipt_sha256().into(),
            issued_at,
            tier: r.tier().as_str().into(),
            engine: r.engine().as_str().into(),
            base_commit: r.base_commit().into(),
            patch_sha256: r.patch_sha256().into(),
            target_decl: r.target_id().into(),
            file_path: r.file_path().into(),
            allowlist_passed: true,
            axioms_used: r.observed_axioms().to_vec(),
            external_checker_passed: None,
            compile_seconds: r.compile_seconds(),
            max_heartbeats: None,
            receipt_sha256: r.receipt_sha256().into(),
            raw: serde_json::to_value(r)?,
            regression_checked: false,
            lean_comparison: None,
        })
    }
}

struct AppliedT3Plan {
    approval: T3PlanApproval,
    claims: BTreeSet<String>,
}

enum RecheckContext<'a> {
    Commit(&'a str),
    Plan {
        hash: &'a str,
        only_claim: Option<&'a str>,
    },
}

pub enum RecheckOutcome {
    Verified(Box<VerifiedReceipt>),
    /// Only report a proof failure here, not infrastructure failure. Return Err for the latter.
    Failed {
        diagnostics: String,
    },
    /// No proof conclusion: the claim remains stale and can be retried.
    Deferred {
        reason: String,
    },
}

#[derive(Default, Debug, Serialize)]
pub struct RecheckReport {
    pub verified: Vec<String>,
    pub needs_reproof: Vec<String>,
    pub deferred: BTreeMap<String, String>,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct TaintImpact {
    pub claims: Vec<String>,
    /// Dependency-first order for the broker's physical rebuild.
    pub modules: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct T3PlanApproval {
    pub plan_sha256: String,
    pub base_commit: String,
    pub reviewed_commit: String,
    pub anchor_module: String,
    pub preview_event_seq: i64,
}

/// Operator snapshot of an applied plan in dependency-first claim order.
#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct T3WorklistPage {
    pub plan_sha256: String,
    pub reviewed_commit: String,
    pub last_event_seq: i64,
    pub entries: Vec<T3WorklistEntry>,
    /// Exclusive cursor for another page, if this snapshot has more entries.
    pub next_after_claim: Option<String>,
}

/// Outcome of one bounded pass over the approved T3 worklist.
#[derive(Debug, Serialize)]
pub struct T3BatchReport {
    pub plan_sha256: String,
    pub reviewed_commit: String,
    /// All examined worklist rows in dependency order, including skips and deferrals.
    pub examined_claims: Vec<String>,
    pub report: RecheckReport,
    /// Exclusive cursor for another page, if any. Pair it with last_event_seq.
    pub next_after_claim: Option<String>,
    /// The ledger sequence after this page's independently committed results.
    pub last_event_seq: i64,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct T3WorklistEntry {
    pub claim_id: String,
    pub status: T3WorklistStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum T3WorklistStatus {
    /// Locally stale, previously L5, and tainted at this plan's reviewed commit.
    Pending,
    /// No longer locally stale.
    Resolved,
    /// Still locally stale, but the prior trust or taint root does not match.
    NotEligible,
    /// Otherwise pending, but a sealed comparison already exists for this plan.
    RetainedComparison,
}

#[derive(Debug, Serialize)]
pub struct TaintPreview {
    pub event_seq: i64,
    pub impact: TaintImpact,
}

impl StateStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let db = open_db(path.as_ref(), DDL)?;
        db.execute_batch(include_str!("../../../schemas/task-scheduler.sql"))?;
        // The schema defines the stream; prohibit mutation even through local SQL clients.
        db.execute_batch(
            "CREATE TRIGGER IF NOT EXISTS events_no_update BEFORE UPDATE ON events
            BEGIN SELECT RAISE(ABORT, 'events are append-only'); END;
            CREATE TRIGGER IF NOT EXISTS events_no_delete BEFORE DELETE ON events
            BEGIN SELECT RAISE(ABORT, 'events are append-only'); END;",
        )?;
        let mut store = Self { db };
        migrations::permit_distinct_same_head_t3_plans(&mut store.db)?;
        store.initialize_route_projection()?;
        Ok(store)
    }

    /// For operator dry-runs. No DDL, migrations, or state updates are performed.
    pub fn open_readonly(path: impl AsRef<Path>) -> Result<Self> {
        let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        db.busy_timeout(Duration::from_secs(5))?;
        Ok(Self { db })
    }

    pub fn managed_repo(&self) -> Result<Option<String>> {
        Ok(self
            .db
            .query_row("SELECT repo FROM managed_repo WHERE singleton=1", [], |r| {
                r.get(0)
            })
            .optional()?)
    }

    /// Compact read stamp for guarded Wiki reads. Pending Git state must not be
    /// presented as current even when the journal did not append a new event.
    pub fn managed_wiki_stamp(&self) -> Result<Option<(String, i64)>> {
        read_managed_wiki_stamp(&self.db)
    }

    /// Reserve the SQLite writer while rechecking the generation and running the final
    /// operator Git operation. The caller must validate the private ledger path and
    /// coordinate concurrent Git operations separately. The callback must be short
    /// and must not call another SQLite writer. Nothing is written to the ledger.
    pub fn with_managed_generation_reservation<T>(
        path: &Path,
        expected_repo: &str,
        expected_seq: i64,
        operation: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        let mut db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)
            .context("open existing managed ledger for generation reservation")?;
        db.busy_timeout(Duration::from_secs(1))?;
        ensure!(!db.is_readonly("main")?, "managed ledger is read-only");
        // Even as the last connection, do not checkpoint someone else's WAL writes.
        db.set_db_config(DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE, true)?;
        let tx = db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .context("reserve managed ledger writer")?;
        let (repo, seq) = read_managed_wiki_stamp(&tx)?
            .context("managed ledger has no repository binding or a pending Git promotion/Pivot")?;
        ensure!(
            repo == expected_repo && seq == expected_seq,
            "managed ledger generation changed; review the current repository and event sequence again"
        );
        let result = operation();
        // Release the reservation by rollback on drop, without a fallible commit
        // after the external operation has already run.
        drop(tx);
        result
    }

    /// Trust-boundary setup called by the operator with the canonical repository
    /// path and fully reviewed main OID, under the Git coordination lock.
    pub fn bind_task_repository(&mut self, repo: &str, main: &str) -> Result<()> {
        ensure!(
            Path::new(repo).is_absolute() && !repo.chars().any(char::is_control),
            "expected a canonical repository path"
        );
        ensure!(
            matches!(main.len(), 40 | 64)
                && main
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "expected a full Git commit"
        );
        self.write(|tx| {
            let n = tx.execute(
                "INSERT OR IGNORE INTO managed_repo(singleton,repo) VALUES(1,?1)",
                [repo],
            )?;
            let bound: String =
                tx.query_row("SELECT repo FROM managed_repo WHERE singleton=1", [], |r| {
                    r.get(0)
                })?;
            ensure!(
                bound == repo,
                "ledger is already bound to another managed repository"
            );
            if n == 1 {
                event(
                    tx,
                    Change {
                        actor: "operator:task",
                        git_commit: Some(main),
                    },
                    "TASK_REPOSITORY_BOUND",
                    "project",
                    "managed",
                    json!({}),
                )?;
            }
            Ok(())
        })
    }

    fn initialize_route_projection(&mut self) -> Result<()> {
        let missing = self.db.query_row(
            "SELECT NOT EXISTS(SELECT 1 FROM state_migrations WHERE version=1)",
            [],
            |r| r.get::<_, bool>(0),
        )?;
        if missing {
            self.write(|tx| {
                if tx.execute(
                    "INSERT OR IGNORE INTO state_migrations(version) VALUES(1)",
                    [],
                )? == 1
                {
                    recompute(
                        tx,
                        Change {
                            actor: "migration:route-projection",
                            git_commit: None,
                        },
                    )?;
                }
                Ok(())
            })?;
        }
        Ok(())
    }

    /// Older promotions may have no recipe. Do not silently fall back to a different claim or commit.
    pub fn managed_proof_recipe(&self, claim: &str) -> Result<Option<ManagedProofRecipe>> {
        read_managed_proof_recipe(&self.db, claim, None)
    }

    /// Resolve an exact historical promotion recipe. Never fall back to the latest recipe
    /// or to an arbitrary receipt if this Claim was not promoted at the pinned commit.
    pub fn managed_proof_recipe_at(
        &self,
        claim: &str,
        commit: &str,
    ) -> Result<Option<ManagedProofRecipe>> {
        read_managed_proof_recipe(&self.db, claim, Some(commit))
    }

    pub fn promotion_intent(&self) -> Result<Option<PromotionIntent>> {
        read_intent(&self.db)
    }

    pub fn prepare_promotion(
        &mut self,
        intent: &PromotionIntent,
        receipt: &VerifiedReceipt,
    ) -> Result<()> {
        self.write(|tx| {
            ensure!(read_intent(tx)?.is_none(), "project has a pending promotion; recover it first");
            ensure!(pivot::read_pending(tx)?.is_none(), "project has a pending Pivot; recover it first");
            tx.execute("INSERT OR IGNORE INTO managed_repo(singleton,repo) VALUES(1,?1)",[&intent.repo])?;
            let repo:String=tx.query_row("SELECT repo FROM managed_repo WHERE singleton=1",[],|r|r.get(0))?;
            ensure!(repo==intent.repo,"project is bound to a different Git repository");
            validate_promotion_receipt(&require_claim(tx,&intent.claim_id)?, intent, receipt)?;
            reviewed_lean::check_comparison(tx,intent,receipt)?;
            ensure!(Path::new(&intent.repo).is_absolute() && intent.proof_body.len() <= 256*1024,
                "invalid promotion intent path or size");
            ensure!(intent.new_commit != intent.base_commit && intent.new_commit.len()==intent.base_commit.len()
                && intent.new_commit.bytes().all(|b|b.is_ascii_hexdigit()), "invalid prepared commit hash");
            tx.execute("INSERT INTO promotion_intent(singleton,repo,claim_id,base_commit,new_commit,file_path,target_decl,proof_body) VALUES(1,?1,?2,?3,?4,?5,?6,?7)",
                params![intent.repo,intent.claim_id,intent.base_commit,intent.new_commit,intent.file_path,intent.target_decl,intent.proof_body])?;
            Ok(())
        })
    }

    /// The trusted Git coordinator must verify that the prepared commit reached main.
    /// The journal alone is not proof evidence and cannot construct a receipt.
    pub fn finish_promotion(
        &mut self,
        intent: &PromotionIntent,
        receipt: &VerifiedReceipt,
    ) -> Result<i64> {
        self.write(|tx| {
            reviewed_lean::check_comparison(tx,intent,receipt)?;
            if let Some((claim,base,patch,seq)) = tx.query_row("SELECT p.claim_id,p.base_commit,r.patch_sha256,p.event_seq
                FROM recorded_promotions p JOIN verification_receipts r USING(receipt_id) WHERE p.git_commit=?1",
                [&intent.new_commit], |r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,i64>(3)?))).optional()? {
                ensure!(claim==intent.claim_id && base==intent.base_commit && patch==receipt.patch_sha256,"conflicting promotion replay");
                if receipt.lean_comparison.is_some() {
                    ensure!(reviewed_lean::read_comparison(tx,&intent.new_commit)?.is_some(),
                        "recorded promotion is missing reviewed Lean comparison evidence");
                }
                return Ok(seq);
            }
            ensure!(read_intent(tx)?.as_ref()==Some(intent),"promotion intent changed or is missing");
            let c=require_claim(tx,&intent.claim_id)?;
            validate_promotion_receipt(&c,intent,receipt)?;
            let verification=Change{actor:"broker",git_commit:Some(&intent.base_commit)};
            let promoted=Change{actor:"broker",git_commit:Some(&intent.new_commit)};
            persist_receipt(tx,&c,receipt,verification)?;
            update_trust(tx,&c,Trust::L5,Some(&receipt.id),promoted,json!({"receipt_id":receipt.id}))?;
            recompute(tx,promoted)?;
            let mut detail=json!({
                "base_commit":intent.base_commit,"receipt_id":receipt.id,"patch_sha256":receipt.patch_sha256,
                "file_path":intent.file_path,"target_decl":intent.target_decl,
            });
            if let Some(comparison)=&receipt.lean_comparison {
                detail["comparison_receipt_sha256"]=json!(comparison.receipt_sha256);
            }
            let seq=event(tx,promoted,"PROMOTION_COMMITTED","claim",&intent.claim_id,detail)?;
            tx.execute("INSERT INTO recorded_promotions(git_commit,base_commit,claim_id,receipt_id,event_seq) VALUES(?1,?2,?3,?4,?5)",
                params![intent.new_commit,intent.base_commit,intent.claim_id,receipt.id,seq])?;
            tx.execute("INSERT INTO managed_proof_recipes(git_commit,file_path,target_decl,proof_body) VALUES(?1,?2,?3,?4)",
                params![intent.new_commit,intent.file_path,intent.target_decl,intent.proof_body])?;
            if let Some(comparison)=&receipt.lean_comparison {
                reviewed_lean::persist_comparison(tx,&intent.new_commit,comparison)?;
            }
            regression::observe_base(tx, &intent.new_commit)?;
            tx.execute("DELETE FROM promotion_intent WHERE singleton=1",[])?;
            Ok(seq)
        })
    }

    pub fn status(&self) -> Result<StateStatus> {
        Ok(self.db.query_row("SELECT (SELECT count(*) FROM claims),
            (SELECT count(*) FROM proof_plans), (SELECT count(*) FROM claims WHERE effective_stale=1),
            (SELECT count(*) FROM decision_inbox WHERE status='PENDING'),
            (SELECT coalesce(max(event_seq),0) FROM events)", [], |r| Ok(StateStatus {
                claims: r.get(0)?, proof_plans: r.get(1)?, stale_claims: r.get(2)?,
                pending_decisions: r.get(3)?, last_event_seq: r.get(4)?,
            }))?)
    }

    pub fn events_after(&self, seq: i64, limit: u32) -> Result<Vec<Event>> {
        let mut stmt = self.db.prepare("SELECT event_seq, occurred_at, event_type, entity_type,
            entity_id, git_commit, actor, payload_json FROM events WHERE event_seq > ?1 ORDER BY event_seq LIMIT ?2")?;
        stmt.query_map(params![seq, limit], |r| {
            let payload: String = r.get(7)?;
            Ok(Event {
                event_seq: r.get(0)?,
                occurred_at: r.get(1)?,
                event_type: r.get(2)?,
                entity_type: r.get(3)?,
                entity_id: r.get(4)?,
                git_commit: r.get(5)?,
                actor: r.get(6)?,
                payload: serde_json::from_str(&payload).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        7,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
    }

    pub fn append_event(
        &mut self,
        change: Change<'_>,
        kind: &str,
        entity: &str,
        id: &str,
        payload: Value,
    ) -> Result<i64> {
        ensure!(
            !matches!(kind, "CRITICAL_CLAIM_REFUTED" | "WITNESS_VERIFIED")
                && !(kind == "CLAIM_TRUST_CHANGED"
                    && payload.get("local").and_then(Value::as_str) == Some("L2b")),
            "kernel refutation events require verified witness evidence"
        );
        ensure!(
            !matches!(
                kind,
                "PIVOT_APPLIED"
                    | "BREAKER_ARCHIVED"
                    | "PLAN_ARCHIVED"
                    | "BREAKER_PROBES_ARMED"
                    | "BREAKER_PROBES_STARTED"
                    | "BREAKER_PROBES_FAILED"
                    | "BREAKER_PROBES_PASSED"
            ),
            "Pivot events require a finalized ADR transaction"
        );
        ensure!(
            kind != "T3_TAINT_APPLIED"
                && !(kind == "CLAIM_TRUST_CHANGED"
                    && payload
                        .get("detail")
                        .and_then(|detail| detail.get("lean_spec_recheck"))
                        .is_some()),
            "T3 evidence events require a taint or recheck transaction"
        );
        ensure!(
            !matches!(kind, oracle::RECORDED | tasks::HELD | tasks::RELEASED),
            "Oracle events require the typed advisory evidence or task hold API"
        );
        ensure!(
            kind != tasks::ALL_TRACKS_BLOCKED,
            "scheduler starvation events require the typed scheduler reconciliation API"
        );
        ensure!(
            kind != pivot::CRITICAL_BREAKER_TRIPPED,
            "critical breaker events require verified route impact in the breaker registration transaction"
        );
        ensure!(
            kind != regression::MELTDOWN,
            "managed regression events require typed broker failure evidence"
        );
        ensure!(
            kind != sediment_recheck::RETAINED,
            "sediment replay events require sealed broker evidence"
        );
        ensure!(
            kind != sediment_adaptation::APPROVED,
            "sediment adaptation events require the typed approval API"
        );
        ensure!(
            kind != sediment_lineage::RETAINED,
            "sediment adaptation lineage events require the typed retention API"
        );
        ensure!(
            !matches!(kind, infra_rfc::SUBMITTED | infra_rfc::APPROVED),
            "InfraRFC events require the typed submission or approval API"
        );
        self.write(|tx| event(tx, change, kind, entity, id, payload))
    }

    pub fn claim(&self, id: &str) -> Result<Option<ClaimState>> {
        claim(&self.db, id)
    }

    /// Coordinator-only check for stale retained candidate promotion.
    pub fn module_breaker_gated(&self, engine: &str, module: &str) -> Result<bool> {
        Ok(self.db.query_row("SELECT EXISTS(SELECT 1 FROM circuit_breakers WHERE engine=?1 AND module_path=?2 AND state<>'CLOSED')",[engine,module],|r|r.get(0))?)
    }

    /// Coordinator-only dispatch input; no chat histories or worker output.
    pub fn claim_statement_for_dispatch(&self, id: &str) -> Result<String> {
        let statement: String = self
            .db
            .query_row(
                "SELECT substr(informal_statement,1,16385) FROM claims WHERE claim_id=?1",
                [id],
                |r| r.get(0),
            )
            .optional()?
            .context("unknown dispatch Claim")?;
        ensure!(
            !statement.trim().is_empty()
                && statement.len() <= 16 * 1024
                && !statement.contains('\0'),
            "invalid or oversized Claim dispatch statement"
        );
        Ok(statement)
    }

    pub fn plan_state(&self, id: &str) -> Result<Option<(String, Trust)>> {
        Ok(self
            .db
            .query_row(
                "SELECT CASE WHEN b.plan_id IS NULL THEN p.status ELSE 'BLOCKED_BY_REFUTATION' END, p.effective_trust_level
                 FROM proof_plans p LEFT JOIN proof_plan_route_blocks b USING(plan_id) WHERE p.plan_id=?1",
                [id],
                |r| Ok((r.get(0)?, trust(r, 1)?)),
            )
            .optional()?)
    }

    pub fn create_claim(&mut self, c: NewClaim<'_>, change: Change<'_>) -> Result<()> {
        self.write(|tx| {
            tx.execute(&format!("INSERT INTO claims (claim_id,title,informal_statement,engine,formal_target,
                module_path,is_critical_path,local_trust_level,effective_trust_level,created_at,updated_at)
                VALUES (?1,?2,?3,?4,?5,?6,?7,'L0','L0',{NOW},{NOW})"),
                params![c.id,c.title,c.informal_statement,c.engine,c.formal_target,c.module_path,c.critical])?;
            event(tx, change, "CLAIM_CREATED", "claim", c.id, json!({"title": c.title, "engine": c.engine,"informal_statement":c.informal_statement,"formal_target":c.formal_target,"module_path":c.module_path,"critical":c.critical}))?;
            Ok(())
        })
    }

    pub fn create_plan(&mut self, p: NewPlan<'_>, change: Change<'_>) -> Result<()> {
        ensure!(
            matches!(p.status, PlanStatus::Active | PlanStatus::Proved),
            "new plan must be active or proved"
        );
        self.write(|tx| {
            tx.execute(&format!("INSERT INTO proof_plans (plan_id,target_claim_id,name,track,status,effective_trust_level,created_at,updated_at)
                VALUES (?1,?2,?3,?4,?5,'L0',{NOW},{NOW})"), params![p.id,p.target,p.name,p.track,p.status.sql()])?;
            for &(dep, kind) in p.dependencies {
                tx.execute("INSERT INTO plan_dependencies (plan_id,dep_claim_id,dep_kind) VALUES (?1,?2,?3)", params![p.id,dep,kind])?;
            }
            graph_order(tx)?; // Reject cycles even in dormant plans.
            event(tx, change, "PLAN_CREATED", "proof_plan", p.id, json!({"target":p.target,"name":p.name,"track":p.track,"status":p.status.sql(),"dependencies":p.dependencies}))?;
            invalidate_against_known_refutations(tx, p.id, change)?;
            recompute(tx, change)
        })
    }

    pub fn set_local_trust(&mut self, id: &str, level: Trust, change: Change<'_>) -> Result<()> {
        ensure!(
            matches!(
                level,
                Trust::L0 | Trust::L1 | Trust::L2a | Trust::L3 | Trust::L4a
            ),
            "L2b needs an archived witness, L5 a broker receipt, and L4b requires an ADR admission workflow"
        );
        self.write(|tx| {
            let c = require_claim(tx, id)?;
            ensure!(
                c.local_trust != Trust::L2b,
                "kernel refutation is terminal for this claim ID"
            );
            ensure!(!c.stale, "stale claim requires a recheck or a new proof");
            update_trust(tx, &c, level, None, change, json!({}))?;
            recompute(tx, change)
        })
    }

    /// Internal graph test setup; production T1 must use the managed protocol.
    #[cfg(test)]
    fn record_verified(
        &mut self,
        id: &str,
        receipt: &VerifiedReceipt,
        change: Change<'_>,
    ) -> Result<()> {
        self.write(|tx| {
            let c = require_claim(tx, id)?;
            ensure!(
                c.local_trust != Trust::L2b,
                "kernel refutation is terminal for this claim ID"
            );
            ensure!(
                matches!(receipt.tier.as_str(), "T1_PROMOTION" | "T3_TAINT_REBUILD"),
                "receipt tier cannot promote"
            );
            ensure!(
                !c.stale || receipt.tier == "T3_TAINT_REBUILD",
                "stale claim requires a T3 receipt"
            );
            persist_receipt(tx, &c, receipt, change)?;
            update_trust(
                tx,
                &c,
                Trust::L5,
                Some(&receipt.id),
                change,
                json!({"receipt_id": receipt.id}),
            )?;
            recompute(tx, change)
        })
    }

    pub fn witness_spec_binding(&self, claim_id: &str) -> Result<Option<WitnessSpecBinding>> {
        read_witness_binding(&self.db, claim_id)
    }

    /// Records the operator-approved immutable Git snapshot, never mathematical proof.
    pub fn bind_witness_spec(
        &mut self,
        claim_id: &str,
        binding: &WitnessSpecBinding,
    ) -> Result<()> {
        ensure!(
            Path::new(&binding.repo).is_absolute()
                && matches!(binding.commit_id.len(), 40 | 64)
                && binding
                    .commit_id
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "invalid Git witness spec binding"
        );
        self.write(|tx| {
            ensure!(read_intent(tx)?.is_none(),"recover pending promotion before binding a witness spec");
            let c=require_claim(tx,claim_id)?;
            let expected=format!("{}#{}",binding.file_path,binding.proposition_name);
            ensure!(c.engine=="lean4" && c.formal_target.as_deref()==Some(expected.as_str())
                && c.module_path.as_deref().is_none_or(|m|m==binding.file_path),"Git witness spec does not match the registered claim");
            if let Some(old)=read_witness_binding(tx,claim_id)? {
                ensure!(old==*binding,"witness spec binding is immutable; register a new claim ID for a different snapshot");
                return Ok(());
            }
            tx.execute("INSERT OR IGNORE INTO managed_repo(singleton,repo) VALUES(1,?1)",[&binding.repo])?;
            let bound:String=tx.query_row("SELECT repo FROM managed_repo WHERE singleton=1",[],|r|r.get(0))?;
            ensure!(bound==binding.repo,"project is bound to a different Git repository");
            tx.execute("INSERT INTO witness_spec_bindings VALUES(?1,?2,?3,?4,?5)",params![claim_id,binding.repo,binding.commit_id,binding.file_path,binding.proposition_name])?;
            event(tx,Change{actor:"operator:witness",git_commit:Some(&binding.commit_id)},"WITNESS_SPEC_BOUND","claim",claim_id,json!({"file_path":binding.file_path,"target_decl":binding.proposition_name}))?;
            Ok(())
        })
    }

    /// The witness is sealed by the broker; the operator coordinator checks the physical Git ref.
    /// Match registered proposition and bytes, archive first, then publish the kernel transition.
    pub fn record_refutation(
        &mut self,
        input: WitnessInput<'_>,
        witness: &VerifiedWitness,
    ) -> Result<RefutationRecord> {
        let id = input.claim_id;
        ensure!(
            !id.is_empty()
                && id.len() <= 128
                && id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':')),
            "managed refutation requires a bounded claim identifier"
        );
        ensure!(
            input.repository.is_absolute() && input.proof_body.len() <= 256 * 1024,
            "invalid witness input"
        );
        let repo = input
            .repository
            .to_str()
            .context("managed repository path is not UTF-8")?;
        let base = witness.base_commit();
        let path = witness.file_path();
        let proposition = witness.proposition_name();
        let stable = witness.witness_sha256();
        ensure!(
            witness.matches(
                base,
                path,
                proposition,
                input.committed_source.as_bytes(),
                input.proof_body.as_bytes()
            ),
            "witness bytes do not match sealed proof"
        );
        let wiki_id = refutation::wiki_id(id, stable);
        let change = Change {
            actor: "broker:witness",
            git_commit: Some(base),
        };
        self.write(|tx| {
            ensure!(read_intent(tx)?.is_none(), "recover pending promotion before recording refutation");
            let c=require_claim(tx,id)?;
            let expected_binding=WitnessSpecBinding{repo:repo.into(),commit_id:base.into(),file_path:path.into(),proposition_name:proposition.into()};
            ensure!(read_witness_binding(tx,id)?.as_ref()==Some(&expected_binding),"witness lacks an approved exact Git spec binding");
            let expected=format!("{path}#{proposition}");
            ensure!(c.engine=="lean4" && c.formal_target.as_deref()==Some(expected.as_str())
                && c.module_path.as_deref().is_none_or(|m|m==path),"witness does not target the registered Lean claim");
            tx.execute("INSERT OR IGNORE INTO managed_repo(singleton,repo) VALUES(1,?1)",[repo])?;
            let bound:String=tx.query_row("SELECT repo FROM managed_repo WHERE singleton=1",[],|r|r.get(0))?;
            ensure!(bound==repo,"project is bound to a different Git repository");
            let old=tx.query_row("SELECT wiki_id,witness_sha256,event_seq FROM verified_refutations WHERE claim_id=?1",[id],|r|
                Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,i64>(2)?))).optional()?;
            if let Some((old_id,old_digest,event_seq))=old {
                ensure!(old_id==wiki_id && old_digest==stable && c.local_trust==Trust::L2b,"claim already has a different verified refutation");
                refutation::materialize(&input,&wiki_id,base,path,proposition,witness.source_sha256(),stable,witness.source())?;
                return Ok(RefutationRecord{wiki_id,witness_sha256:stable.into(),event_seq,already_recorded:true});
            }
            ensure!(!matches!(c.local_trust,Trust::L2b|Trust::L5) && !c.stale,"existing L5, refutation, or stale claim requires operator review");
            refutation::materialize(&input,&wiki_id,base,path,proposition,witness.source_sha256(),stable,witness.source())?;
            let seq=event(tx,change,"WITNESS_VERIFIED","claim",id,json!({"wiki_id":wiki_id,"witness_sha256":stable,"file_path":path,"target_decl":proposition}))?;
            tx.execute("INSERT INTO verified_refutations VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
                params![id,wiki_id,stable,base,path,proposition,witness.source_sha256(),input.proof_body,serde_json::to_string(witness)?,seq])?;
            apply_refutation(tx,id,&wiki_id,change)?;
            Ok(RefutationRecord{wiki_id,witness_sha256:stable.into(),event_seq:seq,already_recorded:false})
        })
    }

    #[cfg(test)]
    fn hard_refute(&mut self, id: &str, graveyard_id: &str, change: Change<'_>) -> Result<()> {
        self.write(|tx| apply_refutation(tx, id, graveyard_id, change))
    }

    pub fn add_module_dependency(
        &mut self,
        module: &str,
        imports: &str,
        engine: &str,
        layer: &str,
        change: Change<'_>,
    ) -> Result<()> {
        self.write(|tx| {
            tx.execute(
                "INSERT INTO module_dependencies VALUES (?1,?2,?3,?4)",
                params![module, imports, engine, layer],
            )?;
            module_graph(tx)?.ordered()?;
            event(
                tx,
                change,
                "MODULE_DEPENDENCY_ADDED",
                "module",
                module,
                json!({"imports": imports, "engine": engine, "layer": layer}),
            )?;
            Ok(())
        })
    }

    /// Read the same physical/logical impact as mark_t3, without mutating trust or appending events.
    pub fn preview_t3(&mut self, claim_ids: &[&str], modules: &[&str]) -> Result<TaintPreview> {
        let snapshot = self.db.transaction()?;
        let event_seq = last_seq(&snapshot)?;
        let impact = compute_taint_impact(&snapshot, claim_ids, modules)?;
        snapshot.commit()?;
        Ok(TaintPreview { event_seq, impact })
    }

    pub fn applied_t3_plan(&self, hash: &str) -> Result<Option<T3PlanApproval>> {
        read_t3_plan(&self.db, hash)
    }

    /// The operator coordinator validates the Git ref and displayed plan digest before calling.
    /// The approval and taint transition commit in one SQLite transaction.
    pub fn apply_t3_plan(
        &mut self,
        approval: &T3PlanApproval,
        modules: &[&str],
    ) -> Result<Option<TaintImpact>> {
        ensure!(
            approval.plan_sha256.len() == 64
                && approval.plan_sha256.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid T3 plan hash"
        );
        self.write(|tx| {
            if let Some(applied) = read_t3_plan(tx, &approval.plan_sha256)? {
                ensure!(applied == *approval, "conflicting T3 plan replay");
                return Ok(None);
            }
            ensure!(
                read_intent(tx)?.is_none(),
                "recover pending promotion before applying T3"
            );
            ensure!(
                last_seq(tx)? == approval.preview_event_seq,
                "ledger changed since T3 preview"
            );
            tx.execute(
                "INSERT INTO applied_t3_plans VALUES (?1,?2,?3,?4,?5)",
                params![
                    approval.plan_sha256,
                    approval.base_commit,
                    approval.reviewed_commit,
                    approval.anchor_module,
                    approval.preview_event_seq
                ],
            )?;
            let impact = apply_taint(
                tx,
                &[],
                modules,
                Change {
                    actor: T3_OPERATOR,
                    git_commit: Some(&approval.reviewed_commit),
                },
            )?;
            Ok(Some(impact))
        })
    }

    /// Operator-only readback of the exact applied scope, including claims no longer pending.
    /// Cursors remain usable after a claim's status changes. Ordering is evaluated in the
    /// current graph snapshot; use the returned ledger sequence to detect intervening writes.
    pub fn t3_worklist(
        &mut self,
        hash: &str,
        limit: usize,
        after: Option<&str>,
    ) -> Result<T3WorklistPage> {
        ensure!(
            (1..=64).contains(&limit),
            "T3 worklist limit must be 1..=64"
        );
        let snapshot = self.db.transaction()?;
        let AppliedT3Plan {
            approval: plan,
            claims,
        } = require_applied_t3_plan(&snapshot, hash)?;
        let (ids, next_after_claim) = applied_t3_page(&snapshot, &claims, limit, after)?;
        let mut entries = Vec::with_capacity(ids.len());
        for id in ids {
            let c = require_claim(&snapshot, &id)?;
            let status = if !c.stale {
                T3WorklistStatus::Resolved
            } else if !is_t3_recheck_candidate(&c, &plan.reviewed_commit) {
                T3WorklistStatus::NotEligible
            } else if reviewed_lean::has_recheck(&snapshot, &id, hash)? {
                T3WorklistStatus::RetainedComparison
            } else {
                T3WorklistStatus::Pending
            };
            entries.push(T3WorklistEntry {
                claim_id: c.id,
                status,
            });
        }
        let last_event_seq = last_seq(&snapshot)?;
        snapshot.commit()?;
        Ok(T3WorklistPage {
            plan_sha256: plan.plan_sha256,
            reviewed_commit: plan.reviewed_commit,
            last_event_seq,
            entries,
            next_after_claim,
        })
    }

    /// Include the changed claims and modules themselves, then every transitive dependent.
    pub fn mark_t3(
        &mut self,
        claim_ids: &[&str],
        modules: &[&str],
        change: Change<'_>,
    ) -> Result<TaintImpact> {
        let _commit = change
            .git_commit
            .filter(|s| !s.trim().is_empty())
            .context("T3 requires a taint root commit")?;
        ensure!(
            !claim_ids.is_empty() || !modules.is_empty(),
            "T3 requires changed claims or modules"
        );
        self.write(|tx| {
            ensure!(
                read_intent(tx)?.is_none(),
                "recover pending promotion before applying T3"
            );
            apply_taint(tx, claim_ids, modules, change)
        })
    }

    /// Recheck ordinary stale L5 proofs in logical dependency order. Claims bound to a
    /// reviewed Lean spec are deferred; they require an applied operator plan.
    pub fn recheck_stale(
        &mut self,
        commit: &str,
        actor: &str,
        check: impl FnMut(&ClaimState) -> Result<RecheckOutcome>,
    ) -> Result<RecheckReport> {
        self.recheck_stale_inner(RecheckContext::Commit(commit), actor, check)
    }

    /// Recheck at the review commit pinned by the durable applied plan. The trusted operator
    /// coordinator verifies the exact Git snapshot and supplies fresh broker evidence.
    /// Bound Lean claims can only recover using a sealed comparison of the original proof.
    pub fn recheck_stale_for_plan(
        &mut self,
        plan_sha256: &str,
        actor: &str,
        check: impl FnMut(&ClaimState) -> Result<RecheckOutcome>,
    ) -> Result<RecheckReport> {
        self.recheck_stale_inner(
            RecheckContext::Plan {
                hash: plan_sha256,
                only_claim: None,
            },
            actor,
            check,
        )
    }

    /// Recheck only the requested claim from the exact applied plan. An in-scope
    /// claim that is no longer eligible is a successful no-op.
    pub fn recheck_one_stale_for_plan(
        &mut self,
        hash: &str,
        claim_id: &str,
        actor: &str,
        check: impl FnMut(&ClaimState) -> Result<RecheckOutcome>,
    ) -> Result<RecheckReport> {
        self.recheck_stale_inner(
            RecheckContext::Plan {
                hash,
                only_claim: Some(claim_id),
            },
            actor,
            check,
        )
    }

    /// Recheck one contiguous page of an applied plan, counting resolved and deferred
    /// rows toward the limit. Resume with the returned cursor and ledger sequence;
    /// if the cursor is stale or the batch fails, restart from the first page.
    pub fn recheck_page_stale_for_plan(
        &mut self,
        hash: &str,
        limit: usize,
        after: Option<(&str, i64)>,
        actor: &str,
        check: impl FnMut(&ClaimState) -> Result<RecheckOutcome>,
    ) -> Result<T3BatchReport> {
        ensure!((1..=8).contains(&limit), "T3 batch limit must be 1..=8");
        let (applied, examined_claims, next_after_claim, seq) = {
            let snapshot = self.db.transaction()?;
            let applied = require_applied_t3_plan(&snapshot, hash)?;
            let seq = last_seq(&snapshot)?;
            let cursor = if let Some((cursor, expected)) = after {
                ensure!(
                    seq == expected,
                    "T3 batch cursor is stale: ledger changed since the previous page; restart from the first page"
                );
                Some(cursor)
            } else {
                None
            };
            let (ids, next) = applied_t3_page(&snapshot, &applied.claims, limit, cursor)?;
            snapshot.commit()?;
            (applied, ids, next, seq)
        };
        let (report, last_event_seq) = self.recheck_claims(
            examined_claims.iter().cloned(),
            seq,
            Some(&applied),
            &applied.approval.reviewed_commit,
            actor,
            check,
        )?;
        // This fresh read validates the cursor even for an empty or all-skipped page.
        // Never return a newer sequence that would silently bless a changed worklist.
        ensure!(
            last_event_seq == last_seq(&self.db)?,
            "state changed during T3 recheck; restart from the first page"
        );
        Ok(T3BatchReport {
            plan_sha256: applied.approval.plan_sha256,
            reviewed_commit: applied.approval.reviewed_commit,
            examined_claims,
            report,
            next_after_claim,
            last_event_seq,
        })
    }

    // Callback errors leave the current claim stale; earlier results are independently
    // durable. A concurrent write invalidates any uncommitted callback result.
    fn recheck_stale_inner(
        &mut self,
        context: RecheckContext<'_>,
        actor: &str,
        check: impl FnMut(&ClaimState) -> Result<RecheckOutcome>,
    ) -> Result<RecheckReport> {
        let (ids, seq, applied) = {
            let snapshot = self.db.transaction()?;
            let (applied, only_claim) = match context {
                RecheckContext::Commit(_) => (None, None),
                RecheckContext::Plan { hash, only_claim } => {
                    let applied = require_applied_t3_plan(&snapshot, hash)?;
                    if let Some(id) = only_claim {
                        ensure!(
                            applied.claims.contains(id),
                            "claim {id} is not in the applied T3 plan"
                        );
                    }
                    (Some(applied), only_claim)
                }
            };
            let ids = match only_claim {
                Some(id) => vec![id.to_owned()],
                None => graph_order(&snapshot)?,
            };
            let seq = last_seq(&snapshot)?;
            snapshot.commit()?;
            (ids, seq, applied)
        };
        let commit = match context {
            RecheckContext::Commit(commit) => commit,
            RecheckContext::Plan { .. } => {
                &applied
                    .as_ref()
                    .context("T3 plan has not been applied")?
                    .approval
                    .reviewed_commit
            }
        };
        self.recheck_claims(ids, seq, applied.as_ref(), commit, actor, check)
            .map(|(report, _)| report)
    }

    fn recheck_claims(
        &mut self,
        ids: impl IntoIterator<Item = String>,
        mut seq: i64,
        applied: Option<&AppliedT3Plan>,
        commit: &str,
        actor: &str,
        mut check: impl FnMut(&ClaimState) -> Result<RecheckOutcome>,
    ) -> Result<(RecheckReport, i64)> {
        let change = Change {
            actor,
            git_commit: Some(commit),
        };
        let mut report = RecheckReport::default();
        for id in ids {
            let c = require_claim(&self.db, &id)?;
            ensure!(
                seq == last_seq(&self.db)?,
                "state changed during T3 recheck; retry against current state"
            );
            if !is_t3_recheck_candidate(&c, commit) {
                continue;
            }
            let binding = reviewed_lean::read_binding(&self.db, &id)?;
            let outside_plan = applied.is_some_and(|plan| !plan.claims.contains(&id));
            let reason = if binding.is_some() {
                match applied {
                    None => Some("reviewed Lean spec requires a comparator-backed recheck"),
                    Some(_) if outside_plan => {
                        Some("bound claim was not tainted by this applied plan")
                    }
                    Some(plan)
                        if reviewed_lean::has_recheck(
                            &self.db,
                            &id,
                            &plan.approval.plan_sha256,
                        )? =>
                    {
                        Some("reviewed Lean recheck already recorded for this claim and plan")
                    }
                    Some(_) => None,
                }
            } else {
                None
            };
            ensure!(
                seq == last_seq(&self.db)?,
                "state changed during T3 recheck; retry against current state"
            );
            if let Some(reason) = reason {
                report.deferred.insert(id, reason.into());
                continue;
            }
            if outside_plan {
                continue;
            }
            let outcome = check(&c).with_context(|| format!("broker recheck for {id}"))?;
            let deferred = match &outcome {
                RecheckOutcome::Deferred { reason } => Some(reason.clone()),
                RecheckOutcome::Failed { .. } if binding.is_some() => {
                    Some("generic T3 failure is not evidence against a reviewed Lean spec".into())
                }
                _ => None,
            };
            if let Some(reason) = deferred {
                ensure!(
                    seq == last_seq(&self.db)?,
                    "state changed during T3 recheck; retry against current state"
                );
                report.deferred.insert(id, reason);
                continue;
            }
            seq = self.write(|tx| {
                ensure!(
                    seq == last_seq(tx)?,
                    "state changed during T3 recheck; discard result for {id}"
                );
                ensure!(
                    reviewed_lean::read_binding(tx, &id)? == binding,
                    "reviewed Lean binding changed during T3 recheck"
                );
                match &outcome {
                    RecheckOutcome::Verified(receipt) => {
                        ensure!(
                            receipt.tier == "T3_TAINT_REBUILD",
                            "T3 recheck needs a T3 receipt"
                        );
                        let evidence = if let Some(spec) = &binding {
                            let plan = &applied
                                .context(
                                    "T3 cannot restore a bound Lean claim without an applied plan",
                                )?
                                .approval;
                            ensure!(
                                read_t3_plan(tx, &plan.plan_sha256)?.as_ref() == Some(plan),
                                "applied T3 plan changed during recheck"
                            );
                            Some(reviewed_lean::validate_recheck(
                                tx, &c, spec, plan, receipt,
                            )?)
                        } else {
                            ensure!(
                                receipt.lean_comparison.is_none(),
                                "Lean comparison has no reviewed spec binding"
                            );
                            None
                        };
                        persist_receipt(tx, &c, receipt, change)?;
                        let mut detail = json!({"receipt_id": receipt.id});
                        if let Some(evidence) = &evidence {
                            detail["lean_spec_recheck"] = serde_json::to_value(evidence)?;
                        }
                        update_trust(tx, &c, Trust::L5, Some(&receipt.id), change, detail)?;
                        if let Some(evidence) = &evidence {
                            reviewed_lean::persist_recheck(tx, evidence)?;
                        }
                    }
                    RecheckOutcome::Deferred { .. } => {
                        unreachable!("deferred is handled before any write")
                    }
                    RecheckOutcome::Failed { diagnostics } => {
                        ensure!(
                            binding.is_none(),
                            "generic T3 failure cannot demote a bound Lean claim"
                        );
                        update_trust(
                            tx,
                            &c,
                            Trust::L3,
                            None,
                            change,
                            json!({"diagnostics": diagnostics}),
                        )?;
                        event(
                            tx,
                            change,
                            "CLAIM_REPROOF_REQUESTED",
                            "claim",
                            &id,
                            json!({"diagnostics": diagnostics}),
                        )?;
                    }
                }
                recompute(tx, change)?;
                last_seq(tx)
            })?;
            match outcome {
                RecheckOutcome::Verified(_) => report.verified.push(id),
                RecheckOutcome::Failed { .. } => report.needs_reproof.push(id),
                RecheckOutcome::Deferred { .. } => {
                    unreachable!("deferred is handled before any write")
                }
            }
        }
        Ok((report, seq))
    }

    fn write<T>(&mut self, f: impl FnOnce(&Transaction<'_>) -> Result<T>) -> Result<T> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let value = f(&tx)?;
        tx.commit()?;
        Ok(value)
    }
}

fn read_managed_wiki_stamp(db: &Connection) -> Result<Option<(String, i64)>> {
    Ok(db
        .query_row(
            "SELECT repo,(SELECT coalesce(max(event_seq),0) FROM events) FROM managed_repo
             WHERE singleton=1 AND NOT EXISTS (SELECT 1 FROM promotion_intent)
             AND NOT EXISTS (SELECT 1 FROM pivot_resume_intent)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?)
}

fn read_managed_proof_recipe(
    db: &Connection,
    claim: &str,
    commit: Option<&str>,
) -> Result<Option<ManagedProofRecipe>> {
    Ok(db.query_row("SELECT p.claim_id,p.base_commit,p.git_commit,m.file_path,m.target_decl,m.proof_body,r.patch_sha256
        FROM recorded_promotions p LEFT JOIN managed_proof_recipes m USING(git_commit)
        JOIN verification_receipts r ON r.receipt_id=p.receipt_id WHERE p.claim_id=?1 AND (?2 IS NULL OR p.git_commit=?2) ORDER BY p.event_seq DESC LIMIT 1",params![claim, commit],|r| {
        let file=r.get::<_,Option<String>>(3)?;
        Ok(if let Some(file_path)=file {Some(ManagedProofRecipe {claim_id:r.get(0)?,base_commit:r.get(1)?,promoted_commit:r.get(2)?,file_path,target_decl:r.get(4)?,proof_body:r.get(5)?,source_sha256:r.get(6)?})} else {None})
    }).optional()?.flatten())
}

fn read_t3_plan(db: &Connection, hash: &str) -> Result<Option<T3PlanApproval>> {
    Ok(db.query_row("SELECT base_commit,reviewed_commit,anchor_module,preview_event_seq FROM applied_t3_plans WHERE plan_sha256=?1",[hash],|r|Ok(T3PlanApproval {
        plan_sha256:hash.to_owned(),base_commit:r.get(0)?,reviewed_commit:r.get(1)?,anchor_module:r.get(2)?,preview_event_seq:r.get(3)?,
    })).optional()?)
}

fn require_applied_t3_plan(db: &Connection, hash: &str) -> Result<AppliedT3Plan> {
    let approval = read_t3_plan(db, hash)?.context("T3 plan has not been applied")?;
    let claims = read_applied_t3_claims(db, &approval)?;
    Ok(AppliedT3Plan { approval, claims })
}

fn applied_t3_page(
    snapshot: &Connection,
    claims: &BTreeSet<String>,
    limit: usize,
    after: Option<&str>,
) -> Result<(Vec<String>, Option<String>)> {
    let ordered: Vec<_> = graph_order(snapshot)?
        .into_iter()
        .filter(|id| claims.contains(id))
        .collect();
    ensure!(
        ordered.len() == claims.len(),
        "applied T3 scope references missing claims"
    );
    let start = match after {
        Some(cursor) => {
            ordered
                .iter()
                .position(|id| id == cursor)
                .with_context(|| {
                    format!("invalid T3 worklist cursor {cursor}: not in the applied plan")
                })?
                + 1
        }
        None => 0,
    };
    let remaining = &ordered[start..];
    let next_after_claim = (remaining.len() > limit).then(|| remaining[limit - 1].clone());
    Ok((
        remaining.iter().take(limit).cloned().collect(),
        next_after_claim,
    ))
}

fn is_t3_recheck_candidate(c: &ClaimState, commit: &str) -> bool {
    c.stale && c.prior_trust == Some(Trust::L5) && c.taint_root_commit.as_deref() == Some(commit)
}

// Application compares the preview sequence inside its write transaction, then
// appends only per-claim taint events before the summary. The first non-claim-taint
// event after the preview must therefore be this plan's exact summary, not a
// subsequent taint at the same commit. This also supports older applied plans.
fn read_applied_t3_claims(db: &Connection, plan: &T3PlanApproval) -> Result<BTreeSet<String>> {
    let payload: String = db
        .query_row(
            "SELECT payload_json FROM events
         WHERE event_seq=(SELECT event_seq FROM events
                           WHERE event_seq>?1 AND event_type<>'CLAIM_TAINTED'
                           ORDER BY event_seq LIMIT 1)
           AND event_type='T3_TAINT_APPLIED' AND git_commit=?2
           AND entity_type='snapshot' AND entity_id=?2 AND actor=?3",
            params![plan.preview_event_seq, plan.reviewed_commit, T3_OPERATOR],
            |r| r.get(0),
        )
        .optional()?
        .context("applied T3 plan is missing its taint event")?;
    let payload: Value = serde_json::from_str(&payload)?;
    serde_json::from_value(
        payload
            .get("claims")
            .context("applied T3 taint has no claims")?
            .clone(),
    )
    .context("invalid applied T3 taint claim list")
}

fn apply_taint(
    tx: &Transaction<'_>,
    claim_ids: &[&str],
    modules: &[&str],
    change: Change<'_>,
) -> Result<TaintImpact> {
    let commit = change.git_commit.context("T3 needs a Git snapshot")?;
    let impact = compute_taint_impact(tx, claim_ids, modules)?;
    for id in &impact.claims {
        let c = require_claim(tx, id)?;
        tx.execute(&format!("UPDATE claims SET stale=1,prior_trust_level=CASE WHEN stale=1
            THEN prior_trust_level ELSE local_trust_level END,taint_root_commit=?2,updated_at={NOW} WHERE claim_id=?1"),
            params![id,commit])?;
        event(
            tx,
            change,
            "CLAIM_TAINTED",
            "claim",
            id,
            json!({"prior_trust": c.prior_trust.unwrap_or(c.local_trust)}),
        )?;
    }
    event(
        tx,
        change,
        "T3_TAINT_APPLIED",
        "snapshot",
        commit,
        json!({"claims": impact.claims, "modules": impact.modules}),
    )?;
    recompute(tx, change)?;
    Ok(impact)
}

fn compute_taint_impact(
    db: &Connection,
    claim_ids: &[&str],
    modules: &[&str],
) -> Result<TaintImpact> {
    ensure!(
        !claim_ids.is_empty() || !modules.is_empty(),
        "T3 requires changed claims or modules"
    );
    let mut graph = module_graph(db)?;
    let mut seeds = BTreeSet::from_iter(modules.iter().map(|s| (*s).to_owned()));
    for m in &seeds {
        graph.add_node(m);
    }
    for &id in claim_ids {
        let c = require_claim(db, id)?;
        if let Some(m) = c.module_path {
            graph.add_node(&m);
            seeds.insert(m);
        }
    }
    let affected_modules = graph.closure(seeds);
    let modules = graph
        .ordered()?
        .into_iter()
        .filter(|m| affected_modules.contains(m))
        .collect();
    let claims = all_claims(db)?;
    let seeds = claim_ids
        .iter()
        .map(|s| (*s).to_owned())
        .chain(claims.values().filter_map(|c| {
            c.module_path
                .as_ref()
                .filter(|m| affected_modules.contains(*m))
                .map(|_| c.id.clone())
        }))
        .collect();
    let logical = live_claim_graph(db)?;
    let affected = logical.closure(seeds);
    // Historical kernel witnesses are never labeled stale as if they were proofs.
    let claims = logical
        .ordered()?
        .into_iter()
        .filter(|id| affected.contains(id) && claims[id].local_trust != Trust::L2b)
        .collect();
    Ok(TaintImpact { claims, modules })
}

fn claim(db: &Connection, id: &str) -> Result<Option<ClaimState>> {
    Ok(db
        .query_row(
            &format!("{CLAIM_SELECT} WHERE claim_id=?1"),
            [id],
            read_claim,
        )
        .optional()?)
}

fn require_claim(db: &Connection, id: &str) -> Result<ClaimState> {
    claim(db, id)?.with_context(|| format!("unknown claim {id}"))
}

fn all_claims(db: &Connection) -> Result<BTreeMap<String, ClaimState>> {
    Ok(db
        .prepare(CLAIM_SELECT)?
        .query_map([], read_claim)?
        .map(|r| r.map(|c| (c.id.clone(), c)))
        .collect::<rusqlite::Result<_>>()?)
}

fn last_seq(db: &Connection) -> Result<i64> {
    Ok(
        db.query_row("SELECT coalesce(max(event_seq),0) FROM events", [], |r| {
            r.get(0)
        })?,
    )
}

fn event(
    tx: &Transaction<'_>,
    c: Change<'_>,
    kind: &str,
    entity: &str,
    id: &str,
    payload: Value,
) -> Result<i64> {
    ensure!(
        !c.actor.trim().is_empty()
            && !kind.trim().is_empty()
            && !entity.trim().is_empty()
            && !id.trim().is_empty(),
        "event identity cannot be blank"
    );
    tx.execute(&format!("INSERT INTO events (occurred_at,event_type,entity_type,entity_id,git_commit,actor,payload_json)
        VALUES ({NOW},?1,?2,?3,?4,?5,?6)"), params![kind,entity,id,c.git_commit,c.actor,payload.to_string()])?;
    Ok(tx.last_insert_rowid())
}

fn update_trust(
    tx: &Transaction<'_>,
    c: &ClaimState,
    level: Trust,
    receipt_id: Option<&str>,
    change: Change<'_>,
    detail: Value,
) -> Result<()> {
    tx.execute(
        &format!(
            "UPDATE claims SET local_trust_level=?2,stale=0,prior_trust_level=NULL,
        taint_root_commit=NULL,receipt_id=?3,oracle_debt=?4,updated_at={NOW} WHERE claim_id=?1"
        ),
        params![c.id, level.as_str(), receipt_id, level == Trust::L4a],
    )?;
    event(
        tx,
        change,
        "CLAIM_TRUST_CHANGED",
        "claim",
        &c.id,
        json!({"previous_local": c.local_trust, "local": level, "was_stale": c.stale, "detail": detail}),
    )?;
    Ok(())
}

fn validate_promotion_receipt(
    c: &ClaimState,
    intent: &PromotionIntent,
    r: &VerifiedReceipt,
) -> Result<()> {
    ensure!(
        r.regression_checked
            && r.tier == "T1_PROMOTION"
            && r.base_commit == intent.base_commit
            && r.file_path == intent.file_path
            && r.target_decl == intent.target_decl,
        "promotion receipt does not match its prepared context"
    );
    ensure!(
        !c.stale && c.local_trust != Trust::L2b,
        "stale or refuted claim cannot be promoted"
    );
    validate_target(c, r)
}

fn validate_target(c: &ClaimState, r: &VerifiedReceipt) -> Result<()> {
    let target = c
        .formal_target
        .as_deref()
        .context("claim has no formal target")?;
    let (path, decl) = target.rsplit_once('#').unwrap_or(("", target));
    let expected = c.module_path.as_deref().unwrap_or(path);
    ensure!(
        r.target_decl == decl && r.engine == c.engine && !decl.is_empty(),
        "receipt target/engine does not match claim"
    );
    ensure!(
        !expected.is_empty() && expected == r.file_path && (path.is_empty() || path == expected),
        "receipt module does not match claim"
    );
    Ok(())
}

fn persist_receipt(
    tx: &Transaction<'_>,
    c: &ClaimState,
    r: &VerifiedReceipt,
    change: Change<'_>,
) -> Result<()> {
    validate_target(c, r)?;
    ensure!(
        r.allowlist_passed && r.external_checker_passed != Some(false),
        "receipt records a failed check"
    );
    ensure!(
        change.git_commit == Some(r.base_commit.as_str()),
        "receipt base does not match verification commit"
    );
    // The store cannot infer Git ancestry: verify against the currently marked snapshot.
    ensure!(
        !c.stale || c.taint_root_commit.as_deref() == Some(r.base_commit.as_str()),
        "receipt base does not match the current taint root"
    );
    ensure!(
        r.compile_seconds.is_finite() && r.compile_seconds >= 0.0,
        "invalid receipt timing"
    );
    ensure!(
        [&r.patch_sha256, &r.receipt_sha256]
            .iter()
            .all(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())),
        "invalid receipt hash"
    );
    tx.execute("INSERT INTO verification_receipts (receipt_id,issued_at,tier,engine,base_commit,patch_sha256,
        target_decl,allowlist_passed,axioms_used_json,external_checker_passed,compile_seconds,max_heartbeats,
        receipt_sha256,raw_receipt_json) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
        params![r.id,r.issued_at,r.tier,r.engine,r.base_commit,r.patch_sha256,r.target_decl,r.allowlist_passed,
            serde_json::to_string(&r.axioms_used)?,r.external_checker_passed,r.compile_seconds,r.max_heartbeats,r.receipt_sha256,r.raw.to_string()])?;
    event(
        tx,
        change,
        "RECEIPT_RECORDED",
        "receipt",
        &r.id,
        json!({"claim_id": c.id, "tier": r.tier}),
    )?;
    Ok(())
}

fn read_witness_binding(db: &Connection, id: &str) -> Result<Option<WitnessSpecBinding>> {
    Ok(db.query_row("SELECT repo,commit_id,file_path,proposition_name FROM witness_spec_bindings WHERE claim_id=?1",[id],|r|Ok(WitnessSpecBinding{
      repo:r.get(0)?,commit_id:r.get(1)?,file_path:r.get(2)?,proposition_name:r.get(3)?,
    })).optional()?)
}

fn apply_refutation(
    tx: &Transaction<'_>,
    id: &str,
    graveyard_id: &str,
    change: Change<'_>,
) -> Result<()> {
    ensure!(
        !read_intent(tx)?.is_some_and(|intent| intent.claim_id == id),
        "recover pending promotion before refuting its claim"
    );
    let c = require_claim(tx, id)?;
    ensure!(
        matches!(c.engine.as_str(), "lean4" | "agda" | "rocq"),
        "kernel refutation requires a formal engine"
    );
    ensure!(c.local_trust != Trust::L2b, "already refuted");
    tx.execute(
        "UPDATE claims SET wiki_id=?2 WHERE claim_id=?1",
        params![id, graveyard_id],
    )?;
    update_trust(
        tx,
        &c,
        Trust::L2b,
        None,
        change,
        json!({"wiki_id": graveyard_id}),
    )?;
    let routes = tx
        .prepare(
            "SELECT d.plan_id FROM plan_dependencies d WHERE d.dep_claim_id=?1 ORDER BY d.plan_id",
        )?
        .query_map([id], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for plan in routes {
        invalidate_against_known_refutations(tx, &plan, change)?;
    }
    if c.critical {
        critical_alert(tx, id, id, change)?;
    }
    recompute(tx, change)
}

fn invalidate_against_known_refutations(
    tx: &Transaction<'_>,
    plan: &str,
    change: Change<'_>,
) -> Result<Option<String>> {
    let blocker: Option<(String, String)> = tx
        .query_row(
            "SELECT p.target_claim_id,c.claim_id FROM proof_plans p
        JOIN plan_dependencies d ON d.plan_id=p.plan_id JOIN claims c ON c.claim_id=d.dep_claim_id
        WHERE p.plan_id=?1 AND p.status NOT IN ('INVALIDATED_BY_REFUTATION','ARCHIVED_BY_ADR')
        AND c.local_trust_level='L2b' ORDER BY c.claim_id LIMIT 1",
            [plan],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if let Some((target, refuted)) = blocker {
        tx.execute(&format!("UPDATE proof_plans SET status='INVALIDATED_BY_REFUTATION',effective_trust_level='L0',updated_at={NOW} WHERE plan_id=?1"), [plan])?;
        event(
            tx,
            change,
            "PLAN_INVALIDATED",
            "proof_plan",
            plan,
            json!({"refuted_claim": refuted, "target": target}),
        )?;
        Ok(Some(target))
    } else {
        Ok(None)
    }
}

/// Determine whether each known route remains potentially viable. Exhaustion is
/// reversible (a new route can recover the claim) and is never kernel evidence.
fn refresh_route_blocks(
    tx: &Transaction<'_>,
    order: &[String],
    claims: &BTreeMap<String, ClaimState>,
    change: Change<'_>,
    alert_on_exhaustion: bool,
) -> Result<BTreeMap<String, String>> {
    let mut routed=tx.prepare("SELECT plan_id,status FROM proof_plans WHERE target_claim_id=?1 AND status<>'ARCHIVED_BY_ADR' ORDER BY plan_id")?;
    let mut deps = tx.prepare(
        "SELECT dep_claim_id FROM plan_dependencies WHERE plan_id=?1 ORDER BY dep_claim_id",
    )?;
    let old_claims = read_blocks(
        tx,
        "SELECT claim_id,refuted_claim_id FROM claim_route_exhaustion ORDER BY claim_id",
    )?;
    let old_plans = read_blocks(
        tx,
        "SELECT plan_id,refuted_claim_id FROM proof_plan_route_blocks ORDER BY plan_id",
    )?;
    let mut exhausted = BTreeMap::<String, String>::new();
    let mut blocked = BTreeMap::<String, String>::new();
    for id in order {
        let c = &claims[id];
        let mut no_alternative = true;
        let mut cause: Option<String> = None;
        for row in routed.query_map([id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })? {
            let (plan, status) = row?;
            let mut witness = None;
            for dep in deps.query_map([&plan], |r| r.get::<_, String>(0))? {
                let dep = dep?;
                let root = if claims[&dep].local_trust == Trust::L2b {
                    Some(&dep)
                } else {
                    exhausted.get(&dep)
                };
                if let Some(root) = root {
                    select_cause(&mut witness, root);
                }
            }
            if let Some(witness) = witness {
                if status != "INVALIDATED_BY_REFUTATION" {
                    blocked.insert(plan, witness.clone());
                }
                select_cause(&mut cause, &witness);
            } else {
                no_alternative = false;
            }
        }
        if c.local_trust != Trust::L2b
            && no_alternative
            && let Some(cause) = cause
        {
            exhausted.insert(id.clone(), cause);
        }
    }
    for (plan, cause) in &blocked {
        if old_plans.get(plan) != Some(cause) {
            tx.execute("INSERT INTO proof_plan_route_blocks VALUES (?1,?2) ON CONFLICT(plan_id) DO UPDATE SET refuted_claim_id=excluded.refuted_claim_id",params![plan,cause])?;
            event(
                tx,
                change,
                "PLAN_ROUTE_BLOCKED",
                "proof_plan",
                plan,
                json!({"refuted_claim":cause}),
            )?;
        }
    }
    for plan in old_plans.keys().filter(|p| !blocked.contains_key(*p)) {
        tx.execute(
            "DELETE FROM proof_plan_route_blocks WHERE plan_id=?1",
            [plan],
        )?;
        event(
            tx,
            change,
            "PLAN_ROUTE_UNBLOCKED",
            "proof_plan",
            plan,
            json!({}),
        )?;
    }
    for (id, cause) in &exhausted {
        if old_claims.get(id) != Some(cause) {
            tx.execute("INSERT INTO claim_route_exhaustion VALUES (?1,?2) ON CONFLICT(claim_id) DO UPDATE SET refuted_claim_id=excluded.refuted_claim_id",params![id,cause])?;
        }
        if !old_claims.contains_key(id) {
            event(
                tx,
                change,
                "CLAIM_ROUTES_EXHAUSTED",
                "claim",
                id,
                json!({"refuted_claim":cause}),
            )?;
            if alert_on_exhaustion
                && claims[id].critical
                && (change.actor != "migration:route-projection"
                    || !legacy_exhaustion_alert_exists(tx, id)?)
            {
                critical_alert(tx, id, cause, change)?;
            }
        }
    }
    for id in old_claims.keys().filter(|id| !exhausted.contains_key(*id)) {
        tx.execute("DELETE FROM claim_route_exhaustion WHERE claim_id=?1", [id])?;
        if claims[id].local_trust != Trust::L2b {
            event(tx, change, "CLAIM_ROUTES_REOPENED", "claim", id, json!({}))?;
        }
    }
    Ok(blocked)
}

fn legacy_exhaustion_alert_exists(db: &Connection, id: &str) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM events WHERE event_type='CRITICAL_CLAIM_REFUTED'
        AND entity_type='claim' AND entity_id=?1 AND CASE WHEN json_valid(payload_json)
        THEN json_extract(payload_json,'$.claim_itself_refuted')=0 ELSE 0 END)",
        [id],
        |r| r.get(0),
    )?)
}

fn read_blocks(db: &Connection, sql: &str) -> Result<BTreeMap<String, String>> {
    let mut stmt = db.prepare(sql)?;
    Ok(stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?)
}

fn select_cause(current: &mut Option<String>, root: &str) {
    if current.as_deref().is_none_or(|before| root < before) {
        *current = Some(root.into());
    }
}

fn critical_alert(
    tx: &Transaction<'_>,
    target: &str,
    refuted: &str,
    change: Change<'_>,
) -> Result<()> {
    let payload = json!({"critical_claim": target, "refuted_claim": refuted, "claim_itself_refuted": target == refuted});
    let seq = event(
        tx,
        change,
        "CRITICAL_CLAIM_REFUTED",
        "claim",
        target,
        payload.clone(),
    )?;
    let id = format!("refutation:{seq}");
    let summary = if target == refuted {
        format!("Critical claim {target} refuted")
    } else {
        format!("All known routes to critical claim {target} invalidated")
    };
    tx.execute(&format!("INSERT INTO decision_inbox (inbox_id,created_at,priority,category,summary,payload_json,status)
        VALUES (?1,{NOW},'P0','strategy_branch_pruning',?2,?3,'PENDING')"), params![id,summary,payload.to_string()])?;
    event(tx, change, "INBOX_CREATED", "inbox", &id, payload)?;
    Ok(())
}

#[derive(Default)]
struct Dag {
    incoming: BTreeMap<String, BTreeSet<String>>,
    outgoing: BTreeMap<String, BTreeSet<String>>,
}
impl Dag {
    fn add_node(&mut self, node: &str) {
        self.incoming.entry(node.to_owned()).or_default();
    }
    fn add_edge(&mut self, dep: &str, target: &str) {
        self.add_node(dep);
        self.incoming
            .entry(target.to_owned())
            .or_default()
            .insert(dep.to_owned());
        self.outgoing
            .entry(dep.to_owned())
            .or_default()
            .insert(target.to_owned());
    }
    fn ordered(&self) -> Result<Vec<String>> {
        let mut incoming: BTreeMap<_, _> = self
            .incoming
            .iter()
            .map(|(id, deps)| (id, deps.len()))
            .collect();
        let mut ready: BTreeSet<_> = incoming
            .iter()
            .filter_map(|(&id, &n)| (n == 0).then_some(id))
            .collect();
        let mut order = Vec::new();
        while let Some(id) = ready.pop_first() {
            order.push(id.clone());
            for target in self.outgoing.get(id).into_iter().flatten() {
                let n = incoming.get_mut(target).expect("edge target registered");
                *n -= 1;
                if *n == 0 {
                    ready.insert(target);
                }
            }
        }
        ensure!(
            order.len() == incoming.len(),
            "dependency graph contains a cycle"
        );
        Ok(order)
    }
    fn closure(&self, mut affected: BTreeSet<String>) -> BTreeSet<String> {
        let mut stack: Vec<_> = affected.iter().cloned().collect();
        while let Some(id) = stack.pop() {
            for next in self.outgoing.get(&id).into_iter().flatten() {
                if affected.insert(next.clone()) {
                    stack.push(next.clone());
                }
            }
        }
        affected
    }
}

fn graph_edges(db: &Connection, sql: &str, g: &mut Dag) -> Result<()> {
    let mut stmt = db.prepare(sql)?;
    let edges = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    for edge in edges {
        let (dep, target) = edge?;
        g.add_edge(&dep, &target);
    }
    Ok(())
}

fn build_claim_graph(db: &Connection, edges: &str) -> Result<Dag> {
    let mut g = Dag::default();
    let mut stmt = db.prepare("SELECT claim_id FROM claims")?;
    for id in stmt.query_map([], |r| r.get::<_, String>(0))? {
        g.add_node(&id?);
    }
    graph_edges(db, edges, &mut g)?;
    Ok(g)
}
fn graph_order(db: &Connection) -> Result<Vec<String>> {
    build_claim_graph(db,"SELECT d.dep_claim_id,p.target_claim_id FROM plan_dependencies d JOIN proof_plans p USING(plan_id)")?.ordered()
}
fn live_claim_graph(db: &Connection) -> Result<Dag> {
    build_claim_graph(db,"SELECT d.dep_claim_id,p.target_claim_id FROM plan_dependencies d JOIN proof_plans p USING(plan_id)
       LEFT JOIN proof_plan_route_blocks b USING(plan_id) WHERE b.plan_id IS NULL AND p.status IN ('ACTIVE','PROVED','SUSPENDED_SOFT_PRUNE')")
}
fn module_graph(db: &Connection) -> Result<Dag> {
    let mut g = Dag::default();
    graph_edges(
        db,
        "SELECT imports_module_path,module_path FROM module_dependencies",
        &mut g,
    )?;
    Ok(g)
}

fn recompute(tx: &Transaction<'_>, change: Change<'_>) -> Result<()> {
    recompute_graph(tx, change, true)
}

/// An approved route archive can expose existing refuted alternatives. Record
/// derived exhaustion, but do not open a new critical refutation alert for the ADR.
fn recompute_without_route_alerts(tx: &Transaction<'_>, change: Change<'_>) -> Result<()> {
    recompute_graph(tx, change, false)
}

fn recompute_graph(
    tx: &Transaction<'_>,
    change: Change<'_>,
    alert_on_exhaustion: bool,
) -> Result<()> {
    let mut claims = all_claims(tx)?;
    let order = graph_order(tx)?;
    let blocked = refresh_route_blocks(tx, &order, &claims, change, alert_on_exhaustion)?;
    let mut plans = tx.prepare("SELECT plan_id,status,effective_trust_level FROM proof_plans WHERE target_claim_id=?1 ORDER BY plan_id")?;
    let mut deps = tx.prepare(
        "SELECT dep_claim_id FROM plan_dependencies WHERE plan_id=?1 ORDER BY dep_claim_id",
    )?;
    for id in order {
        let c = &claims[&id];
        let mut stale = c.stale;
        let mut scores = Vec::new();
        let mut has_plans = false;
        for row in plans.query_map([&id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, trust(r, 2)?))
        })? {
            let (plan, status, old) = row?;
            has_plans = true;
            let mut score = None;
            if matches!(status.as_str(), "ACTIVE" | "PROVED")
                && !blocked.contains_key(&plan)
                && !matches!(c.local_trust, Trust::L2a | Trust::L2b)
            {
                let mut levels = Vec::new();
                for dep in deps.query_map([&plan], |r| r.get::<_, String>(0))? {
                    let d = &claims[&dep?];
                    stale |= d.effective_stale;
                    levels.push(d.effective_trust);
                }
                score = aggregate_plan(c.local_trust, levels);
            }
            scores.extend(score);
            let new = score.unwrap_or(Trust::L0);
            if old != new {
                tx.execute(&format!("UPDATE proof_plans SET effective_trust_level=?2,updated_at={NOW} WHERE plan_id=?1"), params![plan,new.as_str()])?;
                event(
                    tx,
                    change,
                    "PLAN_TRUST_CHANGED",
                    "proof_plan",
                    &plan,
                    json!({"previous":old,"effective":new}),
                )?;
            }
        }
        let level = if !has_plans || matches!(c.local_trust, Trust::L2a | Trust::L2b) {
            c.local_trust
        } else {
            best_proof_plan(scores).unwrap_or(Trust::L0)
        };
        if c.effective_trust != level || c.effective_stale != stale {
            tx.execute(&format!("UPDATE claims SET effective_trust_level=?2,effective_stale=?3,updated_at={NOW} WHERE claim_id=?1"), params![id,level.as_str(),stale])?;
            event(
                tx,
                change,
                "CLAIM_EFFECTIVE_CHANGED",
                "claim",
                &id,
                json!({"previous":c.effective_trust,"effective":level,"effective_stale":stale}),
            )?;
            let c = claims.get_mut(&id).expect("claim from graph exists");
            c.effective_trust = level;
            c.effective_stale = stale;
        }
    }
    Ok(())
}

#[cfg(all(test, target_os = "macos"))]
mod native_t3;
#[cfg(test)]
mod tests;
