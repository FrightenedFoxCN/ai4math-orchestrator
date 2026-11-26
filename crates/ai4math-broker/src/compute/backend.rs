//! Unified `ComputeBackend` lifecycle trait and `LocalComputeBackend`
//! backed by `LocalComputePool` (CPU / RAM / per-card GPU isolation).

use super::pool::{ComputeLease, LocalComputePool};
use ai4math_core::{ComputeJobStatus, ExecutionTargetKind};
use ai4math_evidence::{
    FetchedJobOutputs, StagedJobBundle, is_safe_compute_token, is_safe_relative_bundle_path,
};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap},
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

/// Handle returned by `ComputeBackend::submit` and persisted in `ai4math-store`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubmittedComputeJob {
    pub job_id: String,
    pub target_kind: ExecutionTargetKind,
    pub cluster_or_host_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_job_id: Option<String>,
    pub staging_ref: String,
    pub bundle_sha256: String,
    #[serde(default)]
    pub allocated_gpu_indices: Vec<u32>,
}

/// Per-array-task observation returned by `ComputeBackend::poll`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskExecutionObservation {
    pub array_task_id: u32,
    pub status: ComputeJobStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    pub elapsed_seconds: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Unified job status snapshot across local and SLURM execution backends.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComputeJobPoll {
    pub status: ComputeJobStatus,
    pub task_observations: Vec<TaskExecutionObservation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Upper execution carrier trait unifying local sandbox hosts and external
/// SLURM clusters under one submit / poll / fetch / cancel lifecycle.
pub trait ComputeBackend: Sync {
    fn target_kind(&self) -> ExecutionTargetKind;
    fn cluster_or_host_id(&self) -> &str;
    fn submit(&self, bundle: &StagedJobBundle) -> Result<SubmittedComputeJob>;
    fn poll(&self, job: &SubmittedComputeJob) -> Result<ComputeJobPoll>;
    fn fetch_outputs(
        &self,
        job: &SubmittedComputeJob,
        allowlist: &[String],
        max_total_bytes: u64,
    ) -> Result<FetchedJobOutputs>;
    fn cancel(&self, job: &SubmittedComputeJob) -> Result<()>;
}

/// Local task execution hook invoked inside `LocalComputeBackend` while holding
/// an active `ComputeLease` (with concrete GPU device indices & env overrides).
pub trait LocalTaskExecutor: Send + Sync {
    fn execute_task(
        &self,
        task_dir: &Path,
        array_task_id: u32,
        env_vars: &[(&'static str, String)],
        cancelled: &AtomicBool,
    ) -> Result<i32>;
}

struct LocalJobRecord {
    staging_dir: PathBuf,
    status: ComputeJobStatus,
    task_observations: Vec<TaskExecutionObservation>,
    detail: Option<String>,
    cancelled: Arc<AtomicBool>,
}

/// Local workstation execution backend backed by `LocalComputePool`.
///
/// Acquires an atomic multi-dimensional `ComputeLease` (CPU cores, RAM,
/// scratch, and per-card GPU / unified-memory budget), injects mandatory
/// `CUDA_VISIBLE_DEVICES` / `ROCR_VISIBLE_DEVICES` / `OMP_NUM_THREADS` env
/// overrides, stages inputs in a private `0700` directory, and enforces bounded
/// allowlisted output retrieval without following symlinks.
pub struct LocalComputeBackend {
    host_id: String,
    pool: Arc<LocalComputePool>,
    staging_root: PathBuf,
    executor: Arc<dyn LocalTaskExecutor>,
    jobs: Mutex<HashMap<String, LocalJobRecord>>,
}

impl LocalComputeBackend {
    pub fn new(
        host_id: impl Into<String>,
        pool: Arc<LocalComputePool>,
        staging_root: impl AsRef<Path>,
        executor: Arc<dyn LocalTaskExecutor>,
    ) -> Result<Self> {
        let host_id = host_id.into();
        ensure!(
            is_safe_compute_token(&host_id, 64),
            "invalid local host_id {host_id:?}"
        );
        fs::create_dir_all(staging_root.as_ref()).context("create local compute staging root")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(staging_root.as_ref(), fs::Permissions::from_mode(0o700))?;
        }
        let canonical = staging_root.as_ref().canonicalize()?;
        Ok(Self {
            host_id,
            pool,
            staging_root: canonical,
            executor,
            jobs: Mutex::new(HashMap::new()),
        })
    }

    pub fn pool(&self) -> &Arc<LocalComputePool> {
        &self.pool
    }

    fn stage_job_dir(&self, bundle: &StagedJobBundle) -> Result<PathBuf> {
        let job_dir = self.staging_root.join(&bundle.job_id);
        ensure!(
            !job_dir.exists(),
            "local staging directory for job {:?} already exists",
            bundle.job_id
        );
        fs::create_dir(&job_dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&job_dir, fs::Permissions::from_mode(0o700))?;
        }
        for (rel_path, content) in &bundle.task_files {
            ensure!(
                is_safe_relative_bundle_path(rel_path),
                "unsafe staged file path {rel_path:?}"
            );
            let full = job_dir.join(rel_path);
            if let Some(parent) = full.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&full, content)?;
        }
        fs::write(
            job_dir.join("manifest.sha256"),
            bundle.render_sha256sum_manifest(),
        )?;
        Ok(job_dir)
    }

    fn read_bounded_outputs(
        staging_dir: &Path,
        allowlist: &[String],
        max_total_bytes: u64,
    ) -> Result<FetchedJobOutputs> {
        let canonical_root = staging_dir.canonicalize()?;
        let mut files = BTreeMap::new();
        let mut total_bytes = 0u64;
        for rel in allowlist {
            ensure!(
                is_safe_relative_bundle_path(rel),
                "unsafe output allowlist path {rel:?}"
            );
            let target = canonical_root.join(rel);
            let mut opts = fs::OpenOptions::new();
            opts.read(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.custom_flags(libc::O_NOFOLLOW);
            }
            let Ok(file) = opts.open(&target) else {
                continue;
            };
            let meta = file.metadata()?;
            ensure!(
                meta.is_file(),
                "allowlisted output {rel:?} is not a regular file"
            );
            let canonical_file = target.canonicalize()?;
            ensure!(
                canonical_file.starts_with(&canonical_root),
                "allowlisted output {rel:?} escaped staging root"
            );
            let remaining = max_total_bytes.saturating_sub(total_bytes);
            let mut buf = Vec::new();
            file.take(remaining.saturating_add(1))
                .read_to_end(&mut buf)?;
            ensure!(
                u64::try_from(buf.len())? <= remaining,
                "fetched local outputs exceed max_total_bytes cap ({max_total_bytes})"
            );
            total_bytes += u64::try_from(buf.len())?;
            files.insert(rel.clone(), buf);
        }
        let fetched = FetchedJobOutputs { files, total_bytes };
        fetched.validate(allowlist, max_total_bytes)?;
        Ok(fetched)
    }
}

impl ComputeBackend for LocalComputeBackend {
    fn target_kind(&self) -> ExecutionTargetKind {
        ExecutionTargetKind::LocalSandbox
    }

    fn cluster_or_host_id(&self) -> &str {
        &self.host_id
    }

    fn submit(&self, bundle: &StagedJobBundle) -> Result<SubmittedComputeJob> {
        bundle.validate()?;
        let bundle_sha256 = bundle.bundle_sha256()?;
        let lease: ComputeLease = self.pool.try_acquire(&bundle.resources)?;
        let gpu_indices = lease.gpu_indices();
        let env_vars = lease.device_env_vars();
        let job_dir = self.stage_job_dir(bundle)?;
        let cancelled = Arc::new(AtomicBool::new(false));

        let mut observations = Vec::with_capacity(bundle.array_len as usize);
        let mut overall = ComputeJobStatus::Completed;
        let mut detail = None;

        for idx in 0..bundle.array_len {
            if cancelled.load(Ordering::Relaxed) {
                overall = ComputeJobStatus::Cancelled;
                break;
            }
            let task_dir = job_dir.join(format!("tasks/{idx}"));
            if !task_dir.exists() {
                fs::create_dir_all(&task_dir)?;
            }
            let started = Instant::now();
            match self
                .executor
                .execute_task(&task_dir, idx, &env_vars, &cancelled)
            {
                Ok(0) => observations.push(TaskExecutionObservation {
                    array_task_id: idx,
                    status: ComputeJobStatus::Completed,
                    exit_code: Some(0),
                    elapsed_seconds: started.elapsed().as_secs_f64(),
                    detail: None,
                }),
                Ok(code) => {
                    overall = ComputeJobStatus::Failed;
                    detail = Some(format!("local task {idx} exited with code {code}"));
                    observations.push(TaskExecutionObservation {
                        array_task_id: idx,
                        status: ComputeJobStatus::Failed,
                        exit_code: Some(code),
                        elapsed_seconds: started.elapsed().as_secs_f64(),
                        detail: detail.clone(),
                    });
                }
                Err(err) => {
                    overall = ComputeJobStatus::Failed;
                    detail = Some(format!("local task {idx} failed: {err}"));
                    observations.push(TaskExecutionObservation {
                        array_task_id: idx,
                        status: ComputeJobStatus::Failed,
                        exit_code: None,
                        elapsed_seconds: started.elapsed().as_secs_f64(),
                        detail: detail.clone(),
                    });
                }
            }
        }
        // Release local CPU/RAM/GPU lease immediately once local execution finishes.
        drop(lease);

        let staging_ref = job_dir.to_string_lossy().into_owned();
        let mut guard = self
            .jobs
            .lock()
            .map_err(|_| anyhow::anyhow!("local jobs lock poisoned"))?;
        guard.insert(
            bundle.job_id.clone(),
            LocalJobRecord {
                staging_dir: job_dir,
                status: overall,
                task_observations: observations,
                detail,
                cancelled,
            },
        );

        Ok(SubmittedComputeJob {
            job_id: bundle.job_id.clone(),
            target_kind: ExecutionTargetKind::LocalSandbox,
            cluster_or_host_id: self.host_id.clone(),
            external_job_id: None,
            staging_ref,
            bundle_sha256,
            allocated_gpu_indices: gpu_indices,
        })
    }

    fn poll(&self, job: &SubmittedComputeJob) -> Result<ComputeJobPoll> {
        let guard = self
            .jobs
            .lock()
            .map_err(|_| anyhow::anyhow!("local jobs lock poisoned"))?;
        let Some(rec) = guard.get(&job.job_id) else {
            bail!("unknown local compute job {:?}", job.job_id);
        };
        Ok(ComputeJobPoll {
            status: rec.status,
            task_observations: rec.task_observations.clone(),
            detail: rec.detail.clone(),
        })
    }

    fn fetch_outputs(
        &self,
        job: &SubmittedComputeJob,
        allowlist: &[String],
        max_total_bytes: u64,
    ) -> Result<FetchedJobOutputs> {
        let staging_dir = {
            let guard = self
                .jobs
                .lock()
                .map_err(|_| anyhow::anyhow!("local jobs lock poisoned"))?;
            guard
                .get(&job.job_id)
                .map(|r| r.staging_dir.clone())
                .with_context(|| format!("unknown local compute job {:?}", job.job_id))?
        };
        Self::read_bounded_outputs(&staging_dir, allowlist, max_total_bytes)
    }

    fn cancel(&self, job: &SubmittedComputeJob) -> Result<()> {
        let mut guard = self
            .jobs
            .lock()
            .map_err(|_| anyhow::anyhow!("local jobs lock poisoned"))?;
        if let Some(rec) = guard.get_mut(&job.job_id) {
            rec.cancelled.store(true, Ordering::Relaxed);
            if rec.status.is_active() {
                rec.status = ComputeJobStatus::Cancelled;
            }
        }
        Ok(())
    }
}
