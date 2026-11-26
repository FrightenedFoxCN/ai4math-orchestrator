//! Durable asynchronous compute job state for local GPU and SLURM execution.
//!
//! Enables daemon restart recovery of in-flight SLURM jobs, durable cluster
//! quota accounting, branch-scoped batch cancellation (on breaker trip / Pivot),
//! and sealed-receipt binding via `&dyn ProtocolRunEvidence`.

use super::{Change, NOW, StateStore, event};
use crate::retained::valid_id;
use ai4math_core::{ComputeJobStatus, ExecutionTargetKind, ResourceRequest};
use ai4math_evidence::{BlindedArrayPlan, ProtocolRunEvidence, is_safe_compute_token};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::Serialize;
use serde_json::json;

const JOB_SUBMITTED: &str = "COMPUTE_JOB_SUBMITTED";
const JOB_UPDATED: &str = "COMPUTE_JOB_UPDATED";

/// Input describing a newly submitted async local/SLURM compute job.
#[derive(Debug)]
pub struct NewComputeJob<'a> {
    pub job_id: &'a str,
    pub prereg_id: Option<&'a str>,
    pub protocol_id: &'a str,
    pub protocol_sha256: &'a str,
    pub base_commit: &'a str,
    pub candidate_id: &'a str,
    pub candidate_sha256: &'a str,
    pub target_kind: ExecutionTargetKind,
    pub cluster_or_host_id: &'a str,
    pub external_job_id: Option<&'a str>,
    pub staging_ref: &'a str,
    pub bundle_sha256: &'a str,
    pub resources: &'a ResourceRequest,
    pub blinded_plan: &'a BlindedArrayPlan,
    pub branch_tag: Option<&'a str>,
    pub initial_status: ComputeJobStatus,
}

/// Durable readback of one persisted compute job (including local-only unblinding plan).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RetainedComputeJob {
    pub job_id: String,
    pub prereg_id: Option<String>,
    pub protocol_id: String,
    pub protocol_sha256: String,
    pub base_commit: String,
    pub candidate_id: String,
    pub candidate_sha256: String,
    pub target_kind: ExecutionTargetKind,
    pub cluster_or_host_id: String,
    pub external_job_id: Option<String>,
    pub staging_ref: String,
    pub bundle_sha256: String,
    pub resources: ResourceRequest,
    pub requested_gpus_total: u32,
    pub array_len: u32,
    pub blinded_plan: BlindedArrayPlan,
    pub branch_tag: Option<String>,
    pub status: ComputeJobStatus,
    pub detail: Option<String>,
    pub sealed_receipt_sha256: Option<String>,
    pub submitted_event_seq: i64,
    pub updated_event_seq: i64,
    pub created_at: String,
    pub updated_at: String,
}

/// Active quota snapshot reconciled from durable SQLite job rows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct ActiveComputeQuota {
    pub active_jobs: u32,
    pub active_gpus: u32,
}

fn has_migration_8(db: &Connection) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM state_migrations WHERE version=8)",
        [],
        |r| r.get(0),
    )?)
}

fn decode_row(r: &rusqlite::Row<'_>) -> Result<RetainedComputeJob> {
    let target_str: String = r.get(7)?;
    let status_str: String = r.get(16)?;
    let res_json: String = r.get(11)?;
    let plan_json: String = r.get(14)?;
    let resources: ResourceRequest = serde_json::from_str(&res_json)?;
    let blinded_plan: BlindedArrayPlan = serde_json::from_str(&plan_json)?;
    blinded_plan.validate()?;
    Ok(RetainedComputeJob {
        job_id: r.get(0)?,
        prereg_id: r.get(1)?,
        protocol_id: r.get(2)?,
        protocol_sha256: r.get(3)?,
        base_commit: r.get(4)?,
        candidate_id: r.get(5)?,
        candidate_sha256: r.get(6)?,
        target_kind: target_str
            .parse()
            .map_err(|e| anyhow::anyhow!("stored target_kind invalid: {e}"))?,
        cluster_or_host_id: r.get(8)?,
        external_job_id: r.get(9)?,
        staging_ref: r.get(10)?,
        bundle_sha256: r.get(22)?,
        resources,
        requested_gpus_total: u32::try_from(r.get::<_, i64>(12)?)?,
        array_len: u32::try_from(r.get::<_, i64>(13)?)?,
        blinded_plan,
        branch_tag: r.get(15)?,
        status: status_str
            .parse()
            .map_err(|e| anyhow::anyhow!("stored compute job status invalid: {e}"))?,
        detail: r.get(17)?,
        sealed_receipt_sha256: r.get(18)?,
        submitted_event_seq: r.get(19)?,
        updated_event_seq: r.get(20)?,
        created_at: r.get(21)?,
        updated_at: r.get(23)?,
    })
}

const SELECT_COMPUTE_JOB: &str = "SELECT
    job_id, prereg_id, protocol_id, protocol_sha256, base_commit, candidate_id,
    candidate_sha256, target_kind, cluster_or_host_id, external_job_id, staging_ref,
    resource_request_json, requested_gpus_total, array_len, blinded_plan_json,
    branch_tag, status, detail, sealed_receipt_sha256, submitted_event_seq,
    updated_event_seq, created_at, bundle_sha256, updated_at
  FROM compute_jobs";

fn read_job(db: &Connection, job_id: &str) -> Result<Option<RetainedComputeJob>> {
    let mut stmt = db.prepare(&format!("{SELECT_COMPUTE_JOB} WHERE job_id=?1"))?;
    let mut rows = stmt.query([job_id])?;
    match rows.next()? {
        Some(row) => Ok(Some(decode_row(row)?)),
        None => Ok(None),
    }
}

impl StateStore {
    /// Persist a newly submitted asynchronous compute job. Exact replays with
    /// matching `job_id` and `bundle_sha256` are idempotent; conflicts fail closed.
    pub fn record_compute_job_submitted(
        &mut self,
        change: Change<'_>,
        job: &NewComputeJob<'_>,
    ) -> Result<RetainedComputeJob> {
        ensure!(is_safe_compute_token(job.job_id, 128), "invalid job_id");
        ensure!(
            valid_id(job.protocol_sha256)
                && valid_id(job.candidate_sha256)
                && valid_id(job.bundle_sha256),
            "compute job contains malformed SHA-256 digest"
        );
        job.resources
            .validate()
            .map_err(|e| anyhow::anyhow!("invalid compute job resources: {e}"))?;
        job.blinded_plan.validate()?;
        ensure!(
            job.blinded_plan.protocol_sha256 == job.protocol_sha256
                && job.blinded_plan.candidate_sha256 == job.candidate_sha256,
            "blinded_plan bindings do not match compute job"
        );
        let res_json = serde_json::to_string(job.resources)?;
        let plan_json = serde_json::to_string(job.blinded_plan)?;
        let array_len = job.blinded_plan.array_len();
        let requested_gpus_total = job.resources.gpu_count().saturating_mul(array_len);

        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(existing) = read_job(&tx, job.job_id)? {
            ensure!(
                existing.bundle_sha256 == job.bundle_sha256
                    && existing.external_job_id.as_deref() == job.external_job_id,
                "compute job {:?} already exists with different bundle or external job ID",
                job.job_id
            );
            tx.commit()?;
            return Ok(existing);
        }

        let seq = event(
            &tx,
            change,
            JOB_SUBMITTED,
            "compute_job",
            job.job_id,
            json!({
                "prereg_id": job.prereg_id,
                "protocol_id": job.protocol_id,
                "protocol_sha256": job.protocol_sha256,
                "target_kind": job.target_kind.as_str(),
                "cluster_or_host_id": job.cluster_or_host_id,
                "external_job_id": job.external_job_id,
                "bundle_sha256": job.bundle_sha256,
                "requested_gpus_total": requested_gpus_total,
                "array_len": array_len,
                "branch_tag": job.branch_tag,
                "status": job.initial_status.as_str(),
            }),
        )?;

        tx.execute(
            &format!(
                "INSERT INTO compute_jobs(
                    job_id, prereg_id, protocol_id, protocol_sha256, base_commit,
                    candidate_id, candidate_sha256, target_kind, cluster_or_host_id,
                    external_job_id, staging_ref, bundle_sha256, resource_request_json,
                    requested_gpus_total, array_len, blinded_plan_json, branch_tag,
                    status, detail, sealed_receipt_sha256, submitted_event_seq,
                    updated_event_seq, created_at, updated_at
                 ) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,NULL,NULL,?19,?19,{NOW},{NOW})"
            ),
            params![
                job.job_id,
                job.prereg_id,
                job.protocol_id,
                job.protocol_sha256,
                job.base_commit,
                job.candidate_id,
                job.candidate_sha256,
                job.target_kind.as_str(),
                job.cluster_or_host_id,
                job.external_job_id,
                job.staging_ref,
                job.bundle_sha256,
                res_json,
                i64::from(requested_gpus_total),
                i64::from(array_len),
                plan_json,
                job.branch_tag,
                job.initial_status.as_str(),
                seq,
            ],
        )?;

        let saved = read_job(&tx, job.job_id)?.context("compute job missing after insert")?;
        tx.commit()?;
        Ok(saved)
    }

    /// Monotonically advance the lifecycle status of a persisted compute job and
    /// optionally bind a broker-sealed `ProtocolRunEvidence` receipt digest.
    pub fn update_compute_job_status(
        &mut self,
        change: Change<'_>,
        job_id: &str,
        status: ComputeJobStatus,
        detail: Option<&str>,
        sealed_receipt: Option<&dyn ProtocolRunEvidence>,
    ) -> Result<RetainedComputeJob> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing =
            read_job(&tx, job_id)?.with_context(|| format!("unknown compute job {job_id:?}"))?;

        let receipt_sha = if let Some(ev) = sealed_receipt {
            ensure!(
                valid_id(ev.receipt_sha256()),
                "sealed receipt has malformed receipt_sha256"
            );
            ensure!(
                ev.protocol_sha256() == existing.protocol_sha256
                    && ev.candidate_sha256() == existing.candidate_sha256,
                "sealed receipt bindings do not match compute job {job_id:?}"
            );
            Some(ev.receipt_sha256().to_owned())
        } else {
            existing.sealed_receipt_sha256.clone()
        };

        let seq = event(
            &tx,
            change,
            JOB_UPDATED,
            "compute_job",
            job_id,
            json!({
                "from_status": existing.status.as_str(),
                "to_status": status.as_str(),
                "detail": detail,
                "sealed_receipt_sha256": receipt_sha,
            }),
        )?;

        tx.execute(
            &format!(
                "UPDATE compute_jobs
                 SET status=?2, detail=?3, sealed_receipt_sha256=?4, updated_event_seq=?5, updated_at={NOW}
                 WHERE job_id=?1"
            ),
            params![job_id, status.as_str(), detail, receipt_sha, seq],
        )?;

        let updated = read_job(&tx, job_id)?.context("compute job missing after update")?;
        tx.commit()?;
        Ok(updated)
    }

    pub fn read_compute_job(&self, job_id: &str) -> Result<Option<RetainedComputeJob>> {
        if !has_migration_8(&self.db)? {
            return Ok(None);
        }
        read_job(&self.db, job_id)
    }

    /// List all non-terminal (`queued` / `running`) compute jobs for restart
    /// recovery, batch SLURM polling, or branch-scoped breaker/Pivot cancellation.
    pub fn list_active_compute_jobs(
        &self,
        target_kind: Option<ExecutionTargetKind>,
        branch_tag: Option<&str>,
    ) -> Result<Vec<RetainedComputeJob>> {
        if !has_migration_8(&self.db)? {
            return Ok(Vec::new());
        }
        let mut stmt = self.db.prepare(&format!(
            "{SELECT_COMPUTE_JOB}
             WHERE status IN ('queued','running')
               AND (?1 IS NULL OR target_kind=?1)
               AND (?2 IS NULL OR branch_tag=?2)
             ORDER BY submitted_event_seq ASC"
        ))?;
        let target_str = target_kind.map(ExecutionTargetKind::as_str);
        let mut rows = stmt.query(params![target_str, branch_tag])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(decode_row(row)?);
        }
        Ok(out)
    }

    /// Reconcile active job count and GPU count for one cluster/host directly
    /// from durable SQLite state (fed to `SlurmComputeBackend::set_reconciled_quota`).
    pub fn active_compute_quota(&self, cluster_or_host_id: &str) -> Result<ActiveComputeQuota> {
        if !has_migration_8(&self.db)? {
            return Ok(ActiveComputeQuota::default());
        }
        let (jobs, gpus): (i64, i64) = self
            .db
            .query_row(
                "SELECT COUNT(*), COALESCE(SUM(requested_gpus_total), 0)
                 FROM compute_jobs
                 WHERE cluster_or_host_id=?1 AND status IN ('queued','running')",
                [cluster_or_host_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .unwrap_or((0, 0));
        Ok(ActiveComputeQuota {
            active_jobs: u32::try_from(jobs)?,
            active_gpus: u32::try_from(gpus)?,
        })
    }
}
