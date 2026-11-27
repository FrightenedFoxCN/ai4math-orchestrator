use crate::{
    Trust, open_db,
    refutation::{self, RefutationRecord, WitnessInput, WitnessSpecBinding},
};
use ai4math_core::{aggregate_plan, best_proof_plan};
use ai4math_evidence::{CandidateEvidence, RegressionEvidence, WitnessEvidence};
use anyhow::{Context, Result, ensure};
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Row, Transaction, TransactionBehavior,
    config::DbConfig, params,
};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
    time::Duration,
};

mod claim_replay;
mod compute_jobs;
mod epistemic_graph;
mod graph_audit;
mod graph_check;
mod graph_replay;
mod idle_queue;
mod inbox;
mod infra_rfc;
mod ledger_replay;
mod migrations;
mod oracle;
mod pivot;
mod plan_replay;
mod probes;
mod promotion;
mod protocol;
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
mod trust_transition;
pub use claim_replay::{ClaimReplay, ReplayedClaim};
pub use compute_jobs::{ActiveComputeQuota, NewComputeJob, RetainedComputeJob};
pub use epistemic_graph::{
    NewConflictEdge, NewEpistemicNode, NewEpistemicPlan, RetainedBeliefSnapshot,
    RetainedEpistemicNode,
};
pub use graph_audit::{GraphAudit, GraphMismatch, GraphMismatchKind};
pub use graph_check::{GraphSemanticCheck, GraphSemanticMismatch};
pub use graph_replay::GraphReplay;
pub use idle_queue::{
    IdleTicketSource, IdleTicketStatus, IdleUsageTotals, NewIdleVerificationTicket,
    RetainedIdleTicket,
};
pub use inbox::NewDecision;
pub use infra_rfc::{INFRA_RFC_MAX_BYTES, InfraRfcRecord, InfraRfcStatus, parse_infra_rfc_draft};
pub use oracle::{OracleApproval, RetainedOracleObservation};
pub use pivot::{
    BreakerTripReport, OwnerArchiveRequest, OwnerArchiveRequestRecord, OwnerArchiveRequestStatus,
    PivotAction, PivotArchivePlan, PivotIntent, PivotLessonDraft, PivotLessonPage,
    PivotLessonWarning, PivotPlan, PivotResult,
};
pub use plan_replay::{PlanReplay, ReplayedPlan, ReplayedPlanDependency};
pub use probes::{BreakerProbeAttempt, BreakerProbeBatch, BreakerProbePlan, BreakerProbeScope};
pub use promotion::{
    PromotionClaimSummary, PromotionIntentSummary, PromotionPreview, PromotionRecoverySummary,
    PromotionStatus, RecordedPromotionSummary,
};
pub use protocol::{RetainedPreRegistration, RetainedProtocolTrial};
pub use reviewed_lean::{LeanSpecComparison, LeanSpecRecheck, ReviewedLeanSpec};
pub use sediment_adaptation::{SedimentAdaptationApproval, SedimentAdaptationPlan};
pub use sediment_lineage::{SedimentCoreEvidence, SedimentLineage, SedimentLineageInput};
pub use sediment_recheck::{RetainedSedimentReplay, SedimentReplayScope};
pub use sweep::{T3SweepState, T3SweepStatus, T3SweepStep};
pub use t3_baseline::T3DiscoveryBaseline;
pub(crate) use tasks::scheduler_snapshot;
pub use tasks::workers::{
    WorkerAttempt, WorkerAttemptError, WorkerAttemptStatus, WorkerSuggestion,
};
pub use tasks::{
    ModelDispatchApproval, ModelDispatchState, NewModelDispatchApproval, NewTask, OracleTaskHold,
    TaskCandidate, TaskCandidateSource, TaskLease, TaskRunOutcome, TaskSchedulerStatus, TaskTrack,
    TaskTrackStatus,
};
pub use trust_debt::{
    TrustDebtCounts, TrustDebtFinding, TrustDebtKind, TrustDebtRole, TrustDebtScan,
};
pub use trust_transition::{FormalSketchEvidence, TrustTransitionError};

const NOW: &str = "strftime('%Y-%m-%dT%H:%M:%fZ','now')";
const DDL: &str = include_str!("../../../schemas/state-store.sql");
const T3_OPERATOR: &str = "operator:t3";

pub struct StateStore {
    pub(crate) db: Connection,
}

#[derive(Debug)]
pub struct IdempotencyConflict;

impl std::fmt::Display for IdempotencyConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ID already identifies different content")
    }
}
impl std::error::Error for IdempotencyConflict {}

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RefutationOutboxPhase {
    /// Intent committed; no private staged recipe is acknowledged yet.
    PendingFile,
    /// Exact Markdown is durable outside every Wiki stratum; DB is not authoritative yet.
    Staged,
    /// verified_refutations/trust/events committed; final Markdown publication is pending.
    DbCommitted,
    /// Authoritative DB and verified final archive agree; the recipe is retained for repair.
    Complete,
}

impl RefutationOutboxPhase {
    fn sql(self) -> &'static str {
        match self {
            Self::PendingFile => "PENDING_FILE",
            Self::Staged => "STAGED",
            Self::DbCommitted => "DB_COMMITTED",
            Self::Complete => "COMPLETE",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "PENDING_FILE" => Ok(Self::PendingFile),
            "STAGED" => Ok(Self::Staged),
            "DB_COMMITTED" => Ok(Self::DbCommitted),
            "COMPLETE" => Ok(Self::Complete),
            _ => anyhow::bail!("invalid refutation outbox phase"),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct PendingRefutation {
    pub claim_id: String,
    pub wiki_id: String,
    pub witness_sha256: String,
    pub phase: RefutationOutboxPhase,
    pub created_at: String,
}

#[derive(Debug, Serialize)]
pub struct RefutationReconcileFailure {
    pub claim_id: String,
    pub wiki_id: String,
    pub error: String,
}

#[derive(Debug, Default, Serialize)]
pub struct RefutationReconcileReport {
    /// Includes newly completed and already-complete/repaired recipes.
    pub records: Vec<RefutationRecord>,
    /// Failures leave the durable recipe in place for a later targeted retry.
    pub failures: Vec<RefutationReconcileFailure>,
}

#[derive(Debug, Clone)]
struct RefutationOutboxRow {
    claim_id: String,
    wiki_id: String,
    witness_sha256: String,
    commit_id: String,
    file_path: String,
    proposition_name: String,
    source_sha256: String,
    repo: String,
    project_root: String,
    proof_body: String,
    receipt_json: String,
    markdown: String,
    markdown_sha256: String,
    phase: RefutationOutboxPhase,
    completed_event_seq: Option<i64>,
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
    // Copied only from the sealed broker candidate, never the serialized receipt.
    broker_assessed_t3: bool,
    lean_comparison: Option<reviewed_lean::ComparedSpec>,
}

impl VerifiedReceipt {
    /// Project a sealed in-process broker result, not a caller-supplied JSON receipt.
    pub fn from_promotion(
        candidate: &dyn CandidateEvidence,
        artifacts: &dyn RegressionEvidence,
    ) -> Result<Self> {
        ensure!(
            artifacts.matches_candidate(candidate),
            "regression did not check this candidate"
        );
        let mut receipt = Self::from_candidate(candidate)?;
        receipt.regression_checked = true;
        Ok(receipt)
    }

    pub fn from_candidate(candidate: &dyn CandidateEvidence) -> Result<Self> {
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
            raw: r.raw_json()?,
            regression_checked: false,
            broker_assessed_t3: candidate.is_assessed_t3(),
            lean_comparison: None,
        })
    }

    /// Compatibility alias for callers migrating from the broker concrete type.
    pub fn from_broker(candidate: &dyn CandidateEvidence) -> Result<Self> {
        Self::from_candidate(candidate)
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
        db.execute_batch(include_str!("../../../schemas/worker-attempts.sql"))?;
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
        store.initialize_refutation_outbox()?;
        migrations::initialize_model_dispatch(&mut store.db)?;
        migrations::initialize_owner_archive_requests(&mut store.db)?;
        migrations::initialize_protocol_verification(&mut store.db)?;
        migrations::initialize_compute_jobs(&mut store.db)?;
        migrations::initialize_epistemic_graph(&mut store.db)?;
        migrations::initialize_idle_verification(&mut store.db)?;
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

    /// The outbox table/index are additive DDL executed by `open_db`. Record the
    /// migration only on a writable open; `open_readonly` never performs DDL.
    fn initialize_refutation_outbox(&mut self) -> Result<()> {
        let missing = self.db.query_row(
            "SELECT NOT EXISTS(SELECT 1 FROM state_migrations WHERE version=3)",
            [],
            |r| r.get::<_, bool>(0),
        )?;
        if missing {
            self.write(|tx| {
                tx.execute(
                    "INSERT OR IGNORE INTO state_migrations(version) VALUES(3)",
                    [],
                )?;
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
            if let Some(seq)=finish_recorded_promotion(tx,intent,receipt)? {
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
            !matches!(
                kind,
                infra_rfc::SUBMITTED
                    | infra_rfc::APPROVED
                    | infra_rfc::SMOKE_STARTED
                    | infra_rfc::FROZEN
                    | infra_rfc::COMPLETED
                    | infra_rfc::SUPERSEDED
                    | infra_rfc::REJECTED
            ),
            "InfraRFC events require the typed lifecycle API"
        );
        ensure!(
            !tasks::is_model_dispatch_event(kind),
            "model dispatch events require the typed approval API"
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
            let created = json!({"title": c.title, "engine": c.engine,"informal_statement":c.informal_statement,"formal_target":c.formal_target,"module_path":c.module_path,"critical":c.critical});
            if creation_event_matches(tx, "CLAIM_CREATED", "claim", c.id, &created)? {
                return Ok(());
            }
            tx.execute(&format!("INSERT INTO claims (claim_id,title,informal_statement,engine,formal_target,
                module_path,is_critical_path,local_trust_level,effective_trust_level,created_at,updated_at)
                VALUES (?1,?2,?3,?4,?5,?6,?7,'L0','L0',{NOW},{NOW})"),
                params![c.id,c.title,c.informal_statement,c.engine,c.formal_target,c.module_path,c.critical])?;
            event(tx, change, "CLAIM_CREATED", "claim", c.id, created)?;
            Ok(())
        })
    }

    pub fn create_plan(&mut self, p: NewPlan<'_>, change: Change<'_>) -> Result<()> {
        ensure!(
            matches!(p.status, PlanStatus::Active | PlanStatus::Proved),
            "new plan must be active or proved"
        );
        self.write(|tx| {
            let created = json!({"target":p.target,"name":p.name,"track":p.track,"status":p.status.sql(),"dependencies":p.dependencies});
            if creation_event_matches(tx, "PLAN_CREATED", "proof_plan", p.id, &created)? {
                return Ok(());
            }
            tx.execute(&format!("INSERT INTO proof_plans (plan_id,target_claim_id,name,track,status,effective_trust_level,created_at,updated_at)
                VALUES (?1,?2,?3,?4,?5,'L0',{NOW},{NOW})"), params![p.id,p.target,p.name,p.track,p.status.sql()])?;
            for &(dep, kind) in p.dependencies {
                tx.execute("INSERT INTO plan_dependencies (plan_id,dep_claim_id,dep_kind) VALUES (?1,?2,?3)", params![p.id,dep,kind])?;
            }
            graph_order(tx)?; // Reject cycles even in dormant plans.
            event(tx, change, "PLAN_CREATED", "proof_plan", p.id, created)?;
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

    /// Return the one active durable refutation intent, if any. Completed recipes
    /// remain queryable through reconciliation but do not block new work.
    pub fn pending_refutation(&self) -> Result<Option<PendingRefutation>> {
        if !refutation_outbox_exists(&self.db)? {
            return Ok(None);
        }
        Ok(self
            .db
            .query_row(
                "SELECT claim_id,wiki_id,witness_sha256,phase,created_at FROM refutation_outbox
                 WHERE phase<>'COMPLETE' ORDER BY created_at,claim_id LIMIT 1",
                [],
                |r| {
                    let phase: String = r.get(3)?;
                    Ok(PendingRefutation {
                        claim_id: r.get(0)?,
                        wiki_id: r.get(1)?,
                        witness_sha256: r.get(2)?,
                        phase: RefutationOutboxPhase::parse(&phase)
                            .map_err(|_| rusqlite::Error::InvalidQuery)?,
                        created_at: r.get(4)?,
                    })
                },
            )
            .optional()?)
    }

    /// Reconcile every retained refutation recipe. New intents are staged and
    /// finalized; DB-committed missing files are rebuilt; completed files are
    /// verified/repaired without appending duplicate events. Conflicts are reported
    /// per row and never replace an existing archive.
    pub fn reconcile_refutations(&mut self) -> Result<RefutationReconcileReport> {
        let mut report = RefutationReconcileReport::default();
        if !refutation_outbox_exists(&self.db)? {
            return Ok(report);
        }
        let rows = read_refutation_outboxes(&self.db)?;
        for row in rows {
            match self.reconcile_refutation_row(&row) {
                Ok(record) => report.records.push(record),
                Err(error) => report.failures.push(RefutationReconcileFailure {
                    claim_id: row.claim_id,
                    wiki_id: row.wiki_id,
                    error: format!("{error:#}"),
                }),
            }
        }
        Ok(report)
    }

    /// Retry one exact durable recipe. This is the Store entry point an operator
    /// CLI can call without re-running the broker or trusting Markdown as evidence.
    pub fn reconcile_refutation(&mut self, claim_id: &str) -> Result<RefutationRecord> {
        let row = read_refutation_outbox(&self.db, claim_id)?
            .with_context(|| format!("no durable refutation recipe for claim {claim_id}"))?;
        self.reconcile_refutation_row(&row)
    }

    fn reconcile_refutation_row(&mut self, row: &RefutationOutboxRow) -> Result<RefutationRecord> {
        match row.phase {
            RefutationOutboxPhase::PendingFile | RefutationOutboxPhase::Staged => {
                refutation::stage(Path::new(&row.project_root), &row.wiki_id, &row.markdown)
                    .with_context(|| format!("stage refutation {}", row.wiki_id))?;
                self.write(|tx| mark_refutation_staged(tx, row))?;
                let (seq, already_recorded) = self.write(|tx| finish_refutation(tx, row))?;
                self.publish_committed_refutation(row, seq, already_recorded)
            }
            RefutationOutboxPhase::DbCommitted => {
                let (seq, already_recorded) = self.write(|tx| finish_refutation(tx, row))?;
                self.publish_committed_refutation(row, seq, already_recorded)
            }
            RefutationOutboxPhase::Complete => {
                let seq = row
                    .completed_event_seq
                    .context("complete refutation has no event")?;
                self.write(|tx| verify_completed_refutation(tx, row, seq))?;
                refutation::publish(Path::new(&row.project_root), &row.wiki_id, &row.markdown)
                    .with_context(|| {
                        format!(
                            "repair completed refutation archive {} for claim {}; reconcile required",
                            row.wiki_id, row.claim_id
                        )
                    })?;
                Ok(RefutationRecord {
                    wiki_id: row.wiki_id.clone(),
                    witness_sha256: row.witness_sha256.clone(),
                    event_seq: seq,
                    already_recorded: true,
                })
            }
        }
    }

    fn publish_committed_refutation(
        &mut self,
        row: &RefutationOutboxRow,
        seq: i64,
        already_recorded: bool,
    ) -> Result<RefutationRecord> {
        let result = (|| -> Result<()> {
            refutation::publish(Path::new(&row.project_root), &row.wiki_id, &row.markdown)?;
            self.write(|tx| mark_refutation_complete(tx, row, seq))
        })();
        if let Err(error) = result {
            return Err(error).context(format!(
                "refutation ledger committed; reconcile required: claim {} wiki {}",
                row.claim_id, row.wiki_id
            ));
        }
        Ok(RefutationRecord {
            wiki_id: row.wiki_id.clone(),
            witness_sha256: row.witness_sha256.clone(),
            event_seq: seq,
            already_recorded,
        })
    }

    /// The witness is sealed by the broker; the operator coordinator checks the physical Git ref.
    /// A durable outbox recipe is committed before any filesystem work. The authoritative
    /// refutation transaction commits before final Markdown publication, so Wiki rebuilds
    /// can never index an L2b archive that has no verified ledger row.
    pub fn record_refutation(
        &mut self,
        input: WitnessInput<'_>,
        witness: &dyn WitnessEvidence,
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
        let project_root = fs::canonicalize(input.project_root)
            .with_context(|| format!("resolve project root {}", input.project_root.display()))?;
        let project_root = project_root
            .to_str()
            .context("managed project root is not UTF-8")?;
        let base = witness.base_commit();
        let path = witness.file_path();
        let proposition = witness.proposition_id();
        let stable = witness.witness_sha256();
        ensure!(
            matches!(stable.len(), 64)
                && stable
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                && matches!(witness.source_sha256().len(), 64)
                && witness
                    .source_sha256()
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "invalid sealed witness digest"
        );
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
        let rendered = refutation::render(
            &input,
            &wiki_id,
            base,
            path,
            proposition,
            witness.source_sha256(),
            stable,
            witness.source(),
        )?;
        ensure!(
            rendered.markdown.len() <= 2 * 1024 * 1024,
            "rendered Graveyard archive exceeds the durable outbox limit"
        );
        let receipt_json = witness.receipt_json()?;
        serde_json::from_str::<Value>(&receipt_json)
            .context("sealed witness receipt is not valid JSON")?;
        let row = RefutationOutboxRow {
            claim_id: id.into(),
            wiki_id,
            witness_sha256: stable.into(),
            commit_id: base.into(),
            file_path: path.into(),
            proposition_name: proposition.into(),
            source_sha256: witness.source_sha256().into(),
            repo: repo.into(),
            project_root: project_root.into(),
            proof_body: input.proof_body.into(),
            receipt_json,
            markdown: rendered.markdown,
            markdown_sha256: rendered.markdown_sha256,
            phase: RefutationOutboxPhase::PendingFile,
            completed_event_seq: None,
        };
        self.write(|tx| prepare_refutation(tx, &row))?;
        self.reconcile_refutation(id).with_context(|| {
            format!("record refutation for claim {id}; inspect pending_refutation/reconcile API")
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

    /// Recheck ordinary stale L5 proofs in logical dependency order. A successful
    /// result requires broker-assessed T3 evidence, not a legacy tier-only receipt.
    /// Claims bound to a reviewed Lean spec require an applied operator plan.
    pub fn recheck_stale(
        &mut self,
        commit: &str,
        actor: &str,
        check: impl FnMut(&ClaimState) -> Result<RecheckOutcome>,
    ) -> Result<RecheckReport> {
        self.recheck_stale_inner(RecheckContext::Commit(commit), actor, check)
    }

    /// Recheck at the review commit pinned by the durable applied plan. The trusted operator
    /// coordinator verifies the exact Git snapshot and supplies fresh broker-assessed T3
    /// evidence. Bound Lean claims also require a sealed comparison of the original proof.
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
                        ensure!(
                            receipt.broker_assessed_t3,
                            "T3 recheck requires broker-assessed T3 provenance"
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
    // Any active phase may still need to publish or repair the final Markdown.
    // Do not let a Wiki rebuild cache that generation: COMPLETE exposes the stamp
    // only after the archive matching the authoritative ledger is durable.
    let archive_pending = if refutation_outbox_exists(db)? {
        "AND NOT EXISTS (SELECT 1 FROM refutation_outbox WHERE phase<>'COMPLETE')"
    } else {
        ""
    };
    let sql = format!(
        "SELECT repo,(SELECT coalesce(max(event_seq),0) FROM events) FROM managed_repo
         WHERE singleton=1 AND NOT EXISTS (SELECT 1 FROM promotion_intent)
         AND NOT EXISTS (SELECT 1 FROM pivot_resume_intent) {archive_pending}"
    );
    Ok(db
        .query_row(&sql, [], |row| Ok((row.get(0)?, row.get(1)?)))
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

/// Recover a finalized promotion only when every durable identity still matches.
/// An identical pending intent is retired in this same transaction; a conflict is
/// fatal before that row is touched.
fn finish_recorded_promotion(
    tx: &Transaction<'_>,
    intent: &PromotionIntent,
    receipt: &VerifiedReceipt,
) -> Result<Option<i64>> {
    let Some((claim, base, receipt_id, seq)) = tx
        .query_row(
            "SELECT claim_id,base_commit,receipt_id,event_seq
             FROM recorded_promotions WHERE git_commit=?1",
            [&intent.new_commit],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                ))
            },
        )
        .optional()?
    else {
        return Ok(None);
    };
    ensure!(
        claim == intent.claim_id && base == intent.base_commit && receipt_id == receipt.id,
        "conflicting promotion replay identity"
    );
    let repo: String = tx
        .query_row("SELECT repo FROM managed_repo WHERE singleton=1", [], |r| {
            r.get(0)
        })
        .optional()?
        .context("recorded promotion has no managed repository binding")?;
    ensure!(
        repo == intent.repo,
        "conflicting promotion replay repository"
    );
    validate_promotion_receipt_context(&require_claim(tx, &intent.claim_id)?, intent, receipt)?;
    let expected_recipe = ManagedProofRecipe {
        claim_id: intent.claim_id.clone(),
        base_commit: intent.base_commit.clone(),
        promoted_commit: intent.new_commit.clone(),
        file_path: intent.file_path.clone(),
        target_decl: intent.target_decl.clone(),
        proof_body: intent.proof_body.clone(),
        source_sha256: receipt.patch_sha256.clone(),
    };
    ensure!(
        read_managed_proof_recipe(tx, &intent.claim_id, Some(&intent.new_commit))?.as_ref()
            == Some(&expected_recipe),
        "conflicting promotion replay recipe"
    );
    ensure!(
        recorded_receipt_matches(tx, &receipt_id, receipt)?,
        "conflicting promotion replay receipt"
    );
    validate_recorded_comparison(tx, intent, receipt)?;
    validate_recorded_promotion_event(tx, intent, receipt, seq)?;
    if let Some(pending) = read_intent(tx)? {
        ensure!(
            pending == *intent,
            "conflicting promotion replay pending intent"
        );
        ensure!(
            tx.execute("DELETE FROM promotion_intent WHERE singleton=1", [])? == 1,
            "failed to retire identical pending promotion intent"
        );
    }
    Ok(Some(seq))
}

fn recorded_receipt_matches(
    db: &Connection,
    receipt_id: &str,
    receipt: &VerifiedReceipt,
) -> Result<bool> {
    let stored: Option<(
        String,
        String,
        String,
        String,
        String,
        String,
        String,
        bool,
        String,
        Option<bool>,
        f64,
        Option<i64>,
        String,
        String,
    )> = db
        .query_row(
            "SELECT receipt_id,issued_at,tier,engine,base_commit,patch_sha256,target_decl,
                allowlist_passed,axioms_used_json,external_checker_passed,compile_seconds,
                max_heartbeats,receipt_sha256,raw_receipt_json
             FROM verification_receipts WHERE receipt_id=?1",
            [receipt_id],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                    r.get(8)?,
                    r.get(9)?,
                    r.get(10)?,
                    r.get(11)?,
                    r.get(12)?,
                    r.get(13)?,
                ))
            },
        )
        .optional()?;
    let Some((
        id,
        issued_at,
        tier,
        engine,
        base_commit,
        patch_sha256,
        target_decl,
        allowlist_passed,
        axioms_json,
        external_checker_passed,
        compile_seconds,
        max_heartbeats,
        receipt_sha256,
        raw_json,
    )) = stored
    else {
        return Ok(false);
    };
    let axioms: Vec<String> = serde_json::from_str(&axioms_json)
        .context("recorded promotion receipt has invalid axioms")?;
    let raw: Value = serde_json::from_str(&raw_json)
        .context("recorded promotion receipt has invalid raw JSON")?;
    Ok(id == receipt.id
        && issued_at == receipt.issued_at
        && tier == receipt.tier
        && engine == receipt.engine
        && base_commit == receipt.base_commit
        && patch_sha256 == receipt.patch_sha256
        && target_decl == receipt.target_decl
        && allowlist_passed == receipt.allowlist_passed
        && axioms == receipt.axioms_used
        && external_checker_passed == receipt.external_checker_passed
        && compile_seconds == receipt.compile_seconds
        && max_heartbeats == receipt.max_heartbeats
        && receipt_sha256 == receipt.receipt_sha256
        && raw == receipt.raw)
}

fn validate_recorded_comparison(
    tx: &Transaction<'_>,
    intent: &PromotionIntent,
    receipt: &VerifiedReceipt,
) -> Result<()> {
    let recorded = reviewed_lean::read_comparison(tx, &intent.new_commit)?;
    match (&receipt.lean_comparison, recorded) {
        (None, None) => Ok(()),
        (Some(comparison), Some(recorded)) => {
            let expected = LeanSpecComparison {
                promoted_commit: intent.new_commit.clone(),
                claim_id: intent.claim_id.clone(),
                base_commit: intent.base_commit.clone(),
                spec_file: comparison.spec_file.clone(),
                spec_decl: comparison.spec_decl.clone(),
                spec_prefix_sha256: comparison.spec_prefix_sha256.clone(),
                candidate_receipt_sha256: comparison.candidate_receipt_sha256.clone(),
                receipt_sha256: comparison.receipt_sha256.clone(),
            };
            ensure!(
                recorded == expected,
                "conflicting promotion replay comparison"
            );
            Ok(())
        }
        _ => anyhow::bail!("conflicting promotion replay comparison"),
    }
}

fn validate_recorded_promotion_event(
    tx: &Transaction<'_>,
    intent: &PromotionIntent,
    receipt: &VerifiedReceipt,
    event_seq: i64,
) -> Result<()> {
    let (kind, entity_type, entity_id, git_commit, actor, payload_json): (
        String,
        String,
        String,
        Option<String>,
        String,
        String,
    ) = tx.query_row(
        "SELECT event_type,entity_type,entity_id,git_commit,actor,payload_json
         FROM events WHERE event_seq=?1",
        [event_seq],
        |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
        },
    )?;
    ensure!(
        kind == "PROMOTION_COMMITTED"
            && entity_type == "claim"
            && entity_id == intent.claim_id
            && git_commit.as_deref() == Some(intent.new_commit.as_str())
            && actor == "broker",
        "conflicting promotion replay event"
    );
    let payload: Value = serde_json::from_str(&payload_json)
        .context("recorded promotion event has invalid payload JSON")?;
    ensure!(
        payload.get("base_commit").and_then(Value::as_str) == Some(intent.base_commit.as_str())
            && payload.get("receipt_id").and_then(Value::as_str) == Some(receipt.id.as_str())
            && payload.get("patch_sha256").and_then(Value::as_str)
                == Some(receipt.patch_sha256.as_str())
            && payload.get("file_path").and_then(Value::as_str) == Some(intent.file_path.as_str())
            && payload.get("target_decl").and_then(Value::as_str)
                == Some(intent.target_decl.as_str()),
        "conflicting promotion replay event payload"
    );
    match &receipt.lean_comparison {
        Some(comparison) => ensure!(
            payload
                .get("comparison_receipt_sha256")
                .and_then(Value::as_str)
                == Some(comparison.receipt_sha256.as_str()),
            "conflicting promotion replay comparison event"
        ),
        None => ensure!(
            payload.get("comparison_receipt_sha256").is_none(),
            "conflicting promotion replay comparison event"
        ),
    }
    Ok(())
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

fn effective_claim_module(c: &ClaimState) -> Option<&str> {
    c.module_path.as_deref().or_else(|| {
        let (path, _) = c.formal_target.as_deref()?.rsplit_once('#')?;
        (!path.is_empty()).then_some(path)
    })
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
        if let Some(m) = effective_claim_module(&c) {
            graph.add_node(m);
            seeds.insert(m.to_owned());
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
            effective_claim_module(c)
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

fn creation_event_matches(
    tx: &Transaction<'_>,
    kind: &str,
    entity: &str,
    id: &str,
    expected: &Value,
) -> Result<bool> {
    let existing: Option<String> = tx
        .query_row(
            "SELECT payload_json FROM events WHERE event_type=?1 AND entity_type=?2 AND entity_id=?3 ORDER BY event_seq LIMIT 1",
            [kind, entity, id],
            |r| r.get(0),
        )
        .optional()?;
    match existing {
        None => Ok(false),
        Some(raw) => {
            let payload: Value = serde_json::from_str(&raw)?;
            if &payload != expected {
                return Err(IdempotencyConflict.into());
            }
            Ok(true)
        }
    }
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

fn validate_promotion_receipt_context(
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
    validate_target(c, r)
}

fn validate_promotion_receipt(
    c: &ClaimState,
    intent: &PromotionIntent,
    r: &VerifiedReceipt,
) -> Result<()> {
    validate_promotion_receipt_context(c, intent, r)?;
    ensure!(
        !c.stale && c.local_trust != Trust::L2b,
        "stale or refuted claim cannot be promoted"
    );
    Ok(())
}

fn validate_target(c: &ClaimState, r: &VerifiedReceipt) -> Result<()> {
    let target = c
        .formal_target
        .as_deref()
        .context("claim has no formal target")?;
    let (path, decl) = target.rsplit_once('#').unwrap_or(("", target));
    let expected = effective_claim_module(c).unwrap_or(path);
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

#[derive(Debug)]
struct VerifiedRefutationRow {
    claim_id: String,
    wiki_id: String,
    witness_sha256: String,
    commit_id: String,
    file_path: String,
    proposition_name: String,
    source_sha256: String,
    proof_body: String,
    receipt_json: String,
    event_seq: i64,
}

fn refutation_outbox_exists(db: &Connection) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='refutation_outbox')",
        [],
        |r| r.get(0),
    )?)
}

fn read_refutation_outbox(db: &Connection, claim_id: &str) -> Result<Option<RefutationOutboxRow>> {
    if !refutation_outbox_exists(db)? {
        return Ok(None);
    }
    Ok(db
        .query_row(
            "SELECT claim_id,wiki_id,witness_sha256,commit_id,file_path,proposition_name,
                source_sha256,repo,project_root,proof_body,receipt_json,markdown,markdown_sha256,
                phase,completed_event_seq FROM refutation_outbox WHERE claim_id=?1",
            [claim_id],
            read_refutation_outbox_row,
        )
        .optional()?)
}

fn read_refutation_outboxes(db: &Connection) -> Result<Vec<RefutationOutboxRow>> {
    if !refutation_outbox_exists(db)? {
        return Ok(Vec::new());
    }
    Ok(db
        .prepare(
            "SELECT claim_id,wiki_id,witness_sha256,commit_id,file_path,proposition_name,
                source_sha256,repo,project_root,proof_body,receipt_json,markdown,markdown_sha256,
                phase,completed_event_seq FROM refutation_outbox ORDER BY claim_id",
        )?
        .query_map([], read_refutation_outbox_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

fn read_refutation_outbox_row(r: &Row<'_>) -> rusqlite::Result<RefutationOutboxRow> {
    let phase: String = r.get(13)?;
    Ok(RefutationOutboxRow {
        claim_id: r.get(0)?,
        wiki_id: r.get(1)?,
        witness_sha256: r.get(2)?,
        commit_id: r.get(3)?,
        file_path: r.get(4)?,
        proposition_name: r.get(5)?,
        source_sha256: r.get(6)?,
        repo: r.get(7)?,
        project_root: r.get(8)?,
        proof_body: r.get(9)?,
        receipt_json: r.get(10)?,
        markdown: r.get(11)?,
        markdown_sha256: r.get(12)?,
        phase: RefutationOutboxPhase::parse(&phase).map_err(|_| rusqlite::Error::InvalidQuery)?,
        completed_event_seq: r.get(14)?,
    })
}

fn read_verified_refutation(
    db: &Connection,
    claim_id: &str,
) -> Result<Option<VerifiedRefutationRow>> {
    Ok(db
        .query_row(
            "SELECT claim_id,wiki_id,witness_sha256,commit_id,file_path,proposition_name,
                source_sha256,proof_body,receipt_json,event_seq FROM verified_refutations
             WHERE claim_id=?1",
            [claim_id],
            |r| {
                Ok(VerifiedRefutationRow {
                    claim_id: r.get(0)?,
                    wiki_id: r.get(1)?,
                    witness_sha256: r.get(2)?,
                    commit_id: r.get(3)?,
                    file_path: r.get(4)?,
                    proposition_name: r.get(5)?,
                    source_sha256: r.get(6)?,
                    proof_body: r.get(7)?,
                    receipt_json: r.get(8)?,
                    event_seq: r.get(9)?,
                })
            },
        )
        .optional()?)
}

fn same_json(left: &str, right: &str) -> bool {
    match (
        serde_json::from_str::<Value>(left),
        serde_json::from_str::<Value>(right),
    ) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

fn same_refutation_identity(row: &RefutationOutboxRow, old: &VerifiedRefutationRow) -> bool {
    row.claim_id == old.claim_id
        && row.wiki_id == old.wiki_id
        && row.witness_sha256 == old.witness_sha256
        && row.commit_id == old.commit_id
        && row.file_path == old.file_path
        && row.proposition_name == old.proposition_name
        && row.source_sha256 == old.source_sha256
        && row.proof_body == old.proof_body
        && same_json(&row.receipt_json, &old.receipt_json)
}

fn same_refutation_recipe(left: &RefutationOutboxRow, right: &RefutationOutboxRow) -> bool {
    left.claim_id == right.claim_id
        && left.wiki_id == right.wiki_id
        && left.witness_sha256 == right.witness_sha256
        && left.commit_id == right.commit_id
        && left.file_path == right.file_path
        && left.proposition_name == right.proposition_name
        && left.source_sha256 == right.source_sha256
        && left.repo == right.repo
        && left.project_root == right.project_root
        && left.proof_body == right.proof_body
        && same_json(&left.receipt_json, &right.receipt_json)
        && left.markdown == right.markdown
        && left.markdown_sha256 == right.markdown_sha256
}

fn validate_refutation_context(tx: &Transaction<'_>, row: &RefutationOutboxRow) -> Result<()> {
    ensure!(
        read_intent(tx)?.is_none(),
        "recover pending promotion before recording refutation"
    );
    let claim = require_claim(tx, &row.claim_id)?;
    let expected_binding = WitnessSpecBinding {
        repo: row.repo.clone(),
        commit_id: row.commit_id.clone(),
        file_path: row.file_path.clone(),
        proposition_name: row.proposition_name.clone(),
    };
    ensure!(
        read_witness_binding(tx, &row.claim_id)?.as_ref() == Some(&expected_binding),
        "witness lacks an approved exact Git spec binding"
    );
    let expected = format!("{}#{}", row.file_path, row.proposition_name);
    ensure!(
        claim.engine == "lean4"
            && claim.formal_target.as_deref() == Some(expected.as_str())
            && claim
                .module_path
                .as_deref()
                .is_none_or(|m| m == row.file_path),
        "witness does not target the registered Lean claim"
    );
    let bound: String =
        tx.query_row("SELECT repo FROM managed_repo WHERE singleton=1", [], |r| {
            r.get(0)
        })?;
    ensure!(
        bound == row.repo,
        "project is bound to a different Git repository"
    );
    Ok(())
}

fn insert_refutation_outbox(
    tx: &Transaction<'_>,
    row: &RefutationOutboxRow,
    phase: RefutationOutboxPhase,
    completed_event_seq: Option<i64>,
) -> Result<()> {
    tx.execute(
        "INSERT INTO refutation_outbox (claim_id,wiki_id,witness_sha256,commit_id,file_path,
            proposition_name,source_sha256,repo,project_root,proof_body,receipt_json,markdown,
            markdown_sha256,phase,created_at,completed_event_seq)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,strftime('%Y-%m-%dT%H:%M:%fZ','now'),?15)",
        params![
            row.claim_id,
            row.wiki_id,
            row.witness_sha256,
            row.commit_id,
            row.file_path,
            row.proposition_name,
            row.source_sha256,
            row.repo,
            row.project_root,
            row.proof_body,
            row.receipt_json,
            row.markdown,
            row.markdown_sha256,
            phase.sql(),
            completed_event_seq
        ],
    )?;
    Ok(())
}

// Re-verifying the exact witness can change its issuance time, which is not
// part of the broker's witness identity. Preserve the first durable receipt;
// all other fields must match, and internal replay still compares the full JSON.
fn retain_first_witness_receipt(
    row: &RefutationOutboxRow,
    original: &str,
) -> Result<RefutationOutboxRow> {
    let without_issuance = |text: &str| -> Result<Value> {
        let mut value: Value = serde_json::from_str(text)?;
        let object = value
            .as_object_mut()
            .context("invalid witness receipt object")?;
        if let Some(issued) = object.remove("issued_unix_seconds") {
            ensure!(issued.is_u64(), "invalid witness issuance time");
        }
        Ok(value)
    };
    ensure!(
        without_issuance(&row.receipt_json)? == without_issuance(original)?,
        "conflicting refutation replay recipe: witness receipt differs"
    );
    let mut row = row.clone();
    row.receipt_json = original.into();
    Ok(row)
}

fn prepare_refutation(tx: &Transaction<'_>, row: &RefutationOutboxRow) -> Result<()> {
    validate_refutation_context(tx, row)?;
    let claim = require_claim(tx, &row.claim_id)?;
    let old = read_verified_refutation(tx, &row.claim_id)?;
    if let Some(existing) = read_refutation_outbox(tx, &row.claim_id)? {
        let row = retain_first_witness_receipt(row, &existing.receipt_json)?;
        ensure!(
            same_refutation_recipe(&row, &existing),
            "conflicting refutation replay recipe for claim {}",
            row.claim_id
        );
        return match (existing.phase, old) {
            (RefutationOutboxPhase::Complete, Some(old)) => {
                ensure!(
                    same_refutation_identity(&existing, &old)
                        && existing.completed_event_seq == Some(old.event_seq)
                        && claim.local_trust == Trust::L2b,
                    "conflicting completed refutation replay identity"
                );
                Ok(())
            }
            (RefutationOutboxPhase::DbCommitted, Some(old)) => {
                ensure!(
                    same_refutation_identity(&existing, &old),
                    "conflicting DB-committed refutation replay identity"
                );
                Ok(())
            }
            (RefutationOutboxPhase::PendingFile | RefutationOutboxPhase::Staged, None) => Ok(()),
            _ => anyhow::bail!(
                "inconsistent refutation outbox/ledger state for claim {}",
                row.claim_id
            ),
        };
    }
    let active: Option<String> = tx
        .query_row(
            "SELECT claim_id FROM refutation_outbox WHERE phase<>'COMPLETE' LIMIT 1",
            [],
            |r| r.get(0),
        )
        .optional()?;
    ensure!(
        active.is_none(),
        "recover pending refutation for claim {} before recording another",
        active.unwrap_or_default()
    );
    if let Some(old) = old {
        let row = retain_first_witness_receipt(row, &old.receipt_json)?;
        ensure!(
            same_refutation_identity(&row, &old) && claim.local_trust == Trust::L2b,
            "claim already has a different verified refutation"
        );
        // Additive adoption of a legacy finalized row. Its existing event is not
        // replayed. Start at PENDING_FILE so archive publication is acknowledged
        // before COMPLETE; reconcile's finish step adopts the old ledger identity.
        insert_refutation_outbox(tx, &row, RefutationOutboxPhase::PendingFile, None)?;
        return Ok(());
    }
    ensure!(
        !matches!(claim.local_trust, Trust::L2b | Trust::L5) && !claim.stale,
        "existing L5, refutation, or stale claim requires operator review"
    );
    insert_refutation_outbox(tx, row, RefutationOutboxPhase::PendingFile, None)
}

fn mark_refutation_staged(tx: &Transaction<'_>, row: &RefutationOutboxRow) -> Result<()> {
    let current = read_refutation_outbox(tx, &row.claim_id)?
        .context("refutation recipe disappeared before staging acknowledgement")?;
    ensure!(
        same_refutation_recipe(row, &current),
        "conflicting refutation recipe changed during staging"
    );
    if current.phase == RefutationOutboxPhase::PendingFile {
        tx.execute(
            "UPDATE refutation_outbox SET phase='STAGED' WHERE claim_id=?1 AND phase='PENDING_FILE'",
            [&row.claim_id],
        )?;
    }
    Ok(())
}

fn finish_refutation(tx: &Transaction<'_>, row: &RefutationOutboxRow) -> Result<(i64, bool)> {
    let current = read_refutation_outbox(tx, &row.claim_id)?
        .context("refutation recipe disappeared before ledger finalization")?;
    ensure!(
        same_refutation_recipe(row, &current),
        "conflicting refutation recipe changed before ledger finalization"
    );
    if current.phase == RefutationOutboxPhase::Complete {
        let seq = current
            .completed_event_seq
            .context("complete refutation has no event")?;
        verify_completed_refutation(tx, &current, seq)?;
        return Ok((seq, true));
    }
    if let Some(old) = read_verified_refutation(tx, &row.claim_id)? {
        ensure!(
            same_refutation_identity(&current, &old)
                && matches!(
                    current.phase,
                    RefutationOutboxPhase::Staged | RefutationOutboxPhase::DbCommitted
                ),
            "conflicting refutation ledger finalization identity"
        );
        tx.execute(
            "UPDATE refutation_outbox SET phase='DB_COMMITTED' WHERE claim_id=?1",
            [&row.claim_id],
        )?;
        return Ok((old.event_seq, true));
    }
    ensure!(
        matches!(
            current.phase,
            RefutationOutboxPhase::PendingFile | RefutationOutboxPhase::Staged
        ),
        "DB-committed refutation is missing its authoritative ledger row"
    );
    validate_refutation_context(tx, &current)?;
    let claim = require_claim(tx, &row.claim_id)?;
    ensure!(
        !matches!(claim.local_trust, Trust::L2b | Trust::L5) && !claim.stale,
        "existing L5, refutation, or stale claim requires operator review"
    );
    let change = Change {
        actor: "broker:witness",
        git_commit: Some(&row.commit_id),
    };
    let seq = event(
        tx,
        change,
        "WITNESS_VERIFIED",
        "claim",
        &row.claim_id,
        json!({"wiki_id":row.wiki_id,"witness_sha256":row.witness_sha256,"file_path":row.file_path,"target_decl":row.proposition_name}),
    )?;
    tx.execute(
        "INSERT INTO verified_refutations (claim_id,wiki_id,witness_sha256,commit_id,file_path,
            proposition_name,source_sha256,proof_body,receipt_json,event_seq)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
        params![
            row.claim_id,
            row.wiki_id,
            row.witness_sha256,
            row.commit_id,
            row.file_path,
            row.proposition_name,
            row.source_sha256,
            row.proof_body,
            row.receipt_json,
            seq
        ],
    )?;
    apply_refutation(tx, &row.claim_id, &row.wiki_id, change)?;
    tx.execute(
        "UPDATE refutation_outbox SET phase='DB_COMMITTED' WHERE claim_id=?1",
        [&row.claim_id],
    )?;
    Ok((seq, false))
}

fn mark_refutation_complete(
    tx: &Transaction<'_>,
    row: &RefutationOutboxRow,
    seq: i64,
) -> Result<()> {
    let current = read_refutation_outbox(tx, &row.claim_id)?
        .context("refutation recipe disappeared after publication")?;
    ensure!(
        same_refutation_recipe(row, &current),
        "conflicting refutation recipe changed after publication"
    );
    verify_completed_refutation(tx, &current, seq)?;
    if current.phase != RefutationOutboxPhase::Complete {
        tx.execute(
            "UPDATE refutation_outbox SET phase='COMPLETE',completed_event_seq=?2
             WHERE claim_id=?1 AND phase='DB_COMMITTED'",
            params![row.claim_id, seq],
        )?;
    }
    Ok(())
}

fn verify_completed_refutation(
    tx: &Transaction<'_>,
    row: &RefutationOutboxRow,
    seq: i64,
) -> Result<()> {
    let old = read_verified_refutation(tx, &row.claim_id)?
        .context("completed refutation has no authoritative ledger row")?;
    ensure!(
        same_refutation_identity(row, &old) && old.event_seq == seq,
        "conflicting completed refutation ledger identity"
    );
    ensure!(
        row.completed_event_seq
            .is_none_or(|completed| completed == seq),
        "conflicting completed refutation event sequence"
    );
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

#[cfg(test)]
mod tests;
