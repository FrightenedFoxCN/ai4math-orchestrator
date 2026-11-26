//! IO-free compute job bundle, blinded SLURM Job Array permutation, cluster
//! read-only mount bindings, and injection-proof SLURM wire primitives.

use crate::{CompiledControlSet, CompiledProtocolRun, ProtocolSpec, ordered_source_set_sha256};
use ai4math_core::{ComputeJobStatus, ExecutionTargetKind, ResourceRequest};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

pub const MAX_STAGED_FILES: usize = 512;
pub const MAX_STAGED_BUNDLE_BYTES: usize = 8 * 1024 * 1024; // 8 MiB text/config bundle
pub const MAX_OUTPUT_ALLOWLIST: usize = 256;
pub const MAX_CLUSTER_MOUNTS: usize = 64;
pub const MAX_ARRAY_LEN: u32 = 128;

fn sha256_json(domain: &str, value: impl Serialize) -> Result<String> {
    let mut hash = Sha256::new();
    hash.update(domain.as_bytes());
    hash.update([0u8]);
    hash.update(serde_json::to_vec(&value)?);
    Ok(format!("{:x}", hash.finalize()))
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn is_git_oid(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub fn is_safe_compute_token(value: &str, max_len: usize) -> bool {
    !value.is_empty()
        && value.len() <= max_len
        && value.starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_')
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b':'))
}

pub fn is_safe_relative_bundle_path(path: &str) -> bool {
    if path.is_empty() || path.len() > 256 || path.starts_with('/') || path.ends_with('/') {
        return false;
    }
    path.split('/').all(|seg| {
        !seg.is_empty()
            && seg != "."
            && seg != ".."
            && seg.starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_')
            && seg
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
    })
}

pub fn is_safe_remote_abs_path(path: &str) -> bool {
    if path.len() < 2 || path.len() > 512 || !path.starts_with('/') || path.ends_with('/') {
        return false;
    }
    path[1..].split('/').all(|seg| {
        !seg.is_empty()
            && seg != "."
            && seg != ".."
            && seg
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
    })
}

/// Read-only mapping of a heavy pinned input (PDB mirror, force field, MSA DB,
/// weights) pre-staged on a cluster so it is bind-mounted read-only rather than
/// uploaded per job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClusterMountEntry {
    pub input_name: String,
    pub remote_ro_path: String,
    pub expected_sha256: String,
    pub container_mount_path: String,
}

impl ClusterMountEntry {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            is_safe_compute_token(&self.input_name, 128),
            "invalid cluster mount input_name {:?}",
            self.input_name
        );
        ensure!(
            is_safe_remote_abs_path(&self.remote_ro_path),
            "unsafe cluster remote_ro_path {:?}",
            self.remote_ro_path
        );
        ensure!(
            is_safe_remote_abs_path(&self.container_mount_path),
            "unsafe container_mount_path {:?}",
            self.container_mount_path
        );
        ensure!(
            is_sha256_hex(&self.expected_sha256),
            "cluster mount expected_sha256 must be lowercase 64-hex"
        );
        Ok(())
    }
}

/// Validated collection of cluster read-only bind mounts checked against a frozen `ProtocolSpec`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClusterMountMap {
    pub entries: Vec<ClusterMountEntry>,
}

impl ClusterMountMap {
    pub fn validate_against_protocol(&self, protocol: &ProtocolSpec) -> Result<()> {
        ensure!(
            self.entries.len() <= MAX_CLUSTER_MOUNTS,
            "too many cluster mount entries"
        );
        let mut seen_inputs = BTreeSet::new();
        let mut seen_targets = BTreeSet::new();
        for entry in &self.entries {
            entry.validate()?;
            ensure!(
                seen_inputs.insert(entry.input_name.as_str()),
                "duplicate cluster mount for input {:?}",
                entry.input_name
            );
            ensure!(
                seen_targets.insert(entry.container_mount_path.as_str()),
                "duplicate container mount destination {:?}",
                entry.container_mount_path
            );
            let Some(expected) = protocol.pinned_inputs.get(&entry.input_name) else {
                bail!(
                    "cluster mount {:?} is not declared in frozen protocol.pinned_inputs",
                    entry.input_name
                );
            };
            ensure!(
                expected == &entry.expected_sha256,
                "cluster mount {:?} SHA-256 differs from frozen protocol.pinned_inputs",
                entry.input_name
            );
        }
        Ok(())
    }
}

/// Private role assigned to one opaque index inside a blinded SLURM Job Array.
/// Stored ONLY inside local broker / SQLite state; NEVER uploaded to cluster nodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum BlindedArrayRole {
    Candidate,
    PositiveControl { control_index: u32 },
    NegativeControl { control_index: u32 },
}

/// Broker-private unblinding table mapping opaque array task indices `0..array_len`
/// back to the candidate vs. hidden positive/negative controls.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlindedArrayPlan {
    pub version: u8,
    pub protocol_sha256: String,
    pub candidate_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control_pack_sha256: Option<String>,
    /// Ordered by opaque array task index `0..slots.len()`.
    pub slots: Vec<BlindedArrayRole>,
    pub plan_sha256: String,
}

impl BlindedArrayPlan {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.version == 1, "unsupported BlindedArrayPlan version");
        ensure!(
            is_sha256_hex(&self.protocol_sha256)
                && is_sha256_hex(&self.candidate_sha256)
                && is_sha256_hex(&self.plan_sha256),
            "BlindedArrayPlan contains malformed SHA-256 digest"
        );
        if let Some(ref cp) = self.control_pack_sha256 {
            ensure!(is_sha256_hex(cp), "invalid control_pack_sha256");
        }
        ensure!(
            !self.slots.is_empty() && self.slots.len() <= MAX_ARRAY_LEN as usize,
            "BlindedArrayPlan slot count out of bounds"
        );
        let candidate_count = self
            .slots
            .iter()
            .filter(|r| matches!(r, BlindedArrayRole::Candidate))
            .count();
        ensure!(
            candidate_count == 1,
            "BlindedArrayPlan must contain exactly one Candidate slot"
        );
        let expected = sha256_json(
            "ai4math:blinded-array-plan:v1",
            (
                self.version,
                &self.protocol_sha256,
                &self.candidate_sha256,
                &self.control_pack_sha256,
                &self.slots,
            ),
        )?;
        ensure!(
            expected == self.plan_sha256,
            "BlindedArrayPlan digest mismatch"
        );
        Ok(())
    }

    pub fn array_len(&self) -> u32 {
        u32::try_from(self.slots.len()).unwrap_or(0)
    }

    pub fn candidate_task_index(&self) -> Option<u32> {
        self.slots
            .iter()
            .position(|r| matches!(r, BlindedArrayRole::Candidate))
            .and_then(|i| u32::try_from(i).ok())
    }
}

/// Typed request to pack 1 candidate plus optional hidden positive/negative
/// controls into an opaque blinded Job Array bundle.
pub struct BlindedArrayBundleRequest<'a> {
    pub job_id: &'a str,
    pub base_commit: &'a str,
    pub protocol: &'a ProtocolSpec,
    pub candidate: &'a CompiledProtocolRun,
    pub controls: Option<&'a CompiledControlSet>,
    pub resources: ResourceRequest,
    pub container_image_sha256: Option<String>,
    pub cluster_mounts: Vec<ClusterMountEntry>,
    pub output_file_per_task: &'a str,
    pub shuffle_nonce: &'a [u8; 32],
}

/// Deterministic SHA-256-keyed Fisher-Yates shuffle packing 1 candidate plus
/// optional hidden positive/negative controls into opaque `tasks/{i}/...` paths
/// without disclosing which index is the candidate or any control ID.
pub fn build_blinded_array_bundle(
    req: BlindedArrayBundleRequest<'_>,
) -> Result<(StagedJobBundle, BlindedArrayPlan)> {
    let BlindedArrayBundleRequest {
        job_id,
        base_commit,
        protocol,
        candidate,
        controls,
        resources,
        container_image_sha256,
        cluster_mounts,
        output_file_per_task,
        shuffle_nonce,
    } = req;
    ensure!(is_safe_compute_token(job_id, 128), "invalid job_id");
    ensure!(is_git_oid(base_commit), "invalid base_commit");
    ensure!(
        is_safe_relative_bundle_path(output_file_per_task),
        "invalid output_file_per_task"
    );
    resources
        .validate()
        .map_err(|e| anyhow::anyhow!("invalid resources: {e}"))?;
    if let Some(ceiling) = &protocol.resource_ceiling {
        resources
            .check_within_ceiling(ceiling)
            .map_err(|e| anyhow::anyhow!("protocol resource ceiling violation: {e}"))?;
    }
    let mount_map = ClusterMountMap {
        entries: cluster_mounts.clone(),
    };
    mount_map.validate_against_protocol(protocol)?;

    let protocol_sha256 = protocol.protocol_sha256()?;
    ensure!(
        candidate.protocol_sha256() == protocol_sha256,
        "compiled candidate does not match protocol_sha256"
    );

    // Collect (Role, CompiledProtocolRun) entries.
    let mut items: Vec<(BlindedArrayRole, &CompiledProtocolRun)> =
        vec![(BlindedArrayRole::Candidate, candidate)];
    let control_pack_sha256 = if let Some(ctrl) = controls {
        for (i, pos) in ctrl.positive.iter().enumerate() {
            items.push((
                BlindedArrayRole::PositiveControl {
                    control_index: u32::try_from(i)?,
                },
                pos,
            ));
        }
        for (i, neg) in ctrl.negative.iter().enumerate() {
            items.push((
                BlindedArrayRole::NegativeControl {
                    control_index: u32::try_from(i)?,
                },
                neg,
            ));
        }
        Some(ctrl.control_pack_sha256.clone())
    } else {
        None
    };

    ensure!(
        items.len() <= MAX_ARRAY_LEN as usize,
        "total array tasks exceed cap of {MAX_ARRAY_LEN}"
    );

    // Fisher-Yates shuffle keyed by SHA-256(domain || nonce || protocol || candidate || step).
    for i in (1..items.len()).rev() {
        let mut h = Sha256::new();
        h.update(b"ai4math:blinded-array-shuffle:v1");
        h.update(shuffle_nonce);
        h.update(protocol_sha256.as_bytes());
        h.update(candidate.candidate_sha256().as_bytes());
        h.update((i as u64).to_be_bytes());
        let digest = h.finalize();
        let raw = u64::from_be_bytes(digest[..8].try_into().expect("8 bytes"));
        let j = (raw as usize) % (i + 1);
        items.swap(i, j);
    }

    let mut slots = Vec::with_capacity(items.len());
    let mut task_files = BTreeMap::new();
    let mut output_allowlist = Vec::with_capacity(items.len());
    let mut total_bytes: usize = 0;

    for (array_idx, (role, compiled)) in items.into_iter().enumerate() {
        slots.push(role);
        for (rel_path, content) in compiled.artifacts() {
            let staged_path = format!("tasks/{array_idx}/{rel_path}");
            ensure!(
                is_safe_relative_bundle_path(&staged_path),
                "invalid staged path {staged_path:?}"
            );
            total_bytes = total_bytes
                .checked_add(content.len())
                .context("staged bundle size overflow")?;
            ensure!(
                total_bytes <= MAX_STAGED_BUNDLE_BYTES,
                "staged bundle exceeds byte cap"
            );
            task_files.insert(staged_path, content.clone());
        }
        let out_path = format!("tasks/{array_idx}/{output_file_per_task}");
        ensure!(
            is_safe_relative_bundle_path(&out_path),
            "invalid output allowlist path {out_path:?}"
        );
        output_allowlist.push(out_path);
    }

    let plan_sha256 = sha256_json(
        "ai4math:blinded-array-plan:v1",
        (
            1u8,
            &protocol_sha256,
            candidate.candidate_sha256(),
            &control_pack_sha256,
            &slots,
        ),
    )?;
    let plan = BlindedArrayPlan {
        version: 1,
        protocol_sha256: protocol_sha256.clone(),
        candidate_sha256: candidate.candidate_sha256().to_owned(),
        control_pack_sha256,
        slots,
        plan_sha256,
    };
    plan.validate()?;

    let bundle = StagedJobBundle::new(
        job_id.to_owned(),
        protocol.protocol_id.clone(),
        protocol_sha256,
        base_commit.to_owned(),
        task_files,
        output_allowlist,
        container_image_sha256,
        cluster_mounts,
        resources,
        plan.array_len(),
    )?;

    Ok((bundle, plan))
}

/// Immutable, self-verifying job bundle handed to `ComputeBackend::submit`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StagedJobBundle {
    pub job_id: String,
    pub protocol_id: String,
    pub protocol_sha256: String,
    pub base_commit: String,
    pub task_files: BTreeMap<String, String>,
    pub manifest_sha256: String,
    pub output_allowlist: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container_image_sha256: Option<String>,
    #[serde(default)]
    pub cluster_mounts: Vec<ClusterMountEntry>,
    pub resources: ResourceRequest,
    pub array_len: u32,
}

impl StagedJobBundle {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        job_id: String,
        protocol_id: String,
        protocol_sha256: String,
        base_commit: String,
        task_files: BTreeMap<String, String>,
        output_allowlist: Vec<String>,
        container_image_sha256: Option<String>,
        cluster_mounts: Vec<ClusterMountEntry>,
        resources: ResourceRequest,
        array_len: u32,
    ) -> Result<Self> {
        let ordered: Vec<(&str, &str)> = task_files
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let manifest_sha256 = ordered_source_set_sha256(&ordered);
        let bundle = Self {
            job_id,
            protocol_id,
            protocol_sha256,
            base_commit,
            task_files,
            manifest_sha256,
            output_allowlist,
            container_image_sha256,
            cluster_mounts,
            resources,
            array_len,
        };
        bundle.validate()?;
        Ok(bundle)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(is_safe_compute_token(&self.job_id, 128), "invalid job_id");
        ensure!(
            is_safe_compute_token(&self.protocol_id, 128),
            "invalid protocol_id"
        );
        ensure!(
            is_sha256_hex(&self.protocol_sha256),
            "invalid protocol_sha256"
        );
        ensure!(is_git_oid(&self.base_commit), "invalid base_commit");
        ensure!(
            !self.task_files.is_empty() && self.task_files.len() <= MAX_STAGED_FILES,
            "staged bundle file count must be in 1..={MAX_STAGED_FILES}"
        );
        let mut total = 0usize;
        let mut folded = BTreeSet::new();
        for (path, content) in &self.task_files {
            ensure!(
                is_safe_relative_bundle_path(path),
                "unsafe staged file path {path:?}"
            );
            ensure!(
                folded.insert(path.to_ascii_lowercase()),
                "case-insensitive collision on staged path {path:?}"
            );
            total = total
                .checked_add(content.len())
                .context("staged byte count overflow")?;
            ensure!(
                total <= MAX_STAGED_BUNDLE_BYTES,
                "staged bundle exceeds byte cap"
            );
        }
        let ordered: Vec<(&str, &str)> = self
            .task_files
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        ensure!(
            ordered_source_set_sha256(&ordered) == self.manifest_sha256,
            "staged bundle manifest_sha256 mismatch"
        );
        ensure!(
            !self.output_allowlist.is_empty()
                && self.output_allowlist.len() <= MAX_OUTPUT_ALLOWLIST,
            "output_allowlist must contain 1..={MAX_OUTPUT_ALLOWLIST} paths"
        );
        let mut seen_out = BTreeSet::new();
        for path in &self.output_allowlist {
            ensure!(
                is_safe_relative_bundle_path(path),
                "unsafe output_allowlist path {path:?}"
            );
            ensure!(
                seen_out.insert(path.as_str()),
                "duplicate output_allowlist path {path:?}"
            );
        }
        if let Some(sha) = &self.container_image_sha256 {
            ensure!(
                is_sha256_hex(sha),
                "container_image_sha256 must be lowercase 64-hex"
            );
        }
        ensure!(
            self.cluster_mounts.len() <= MAX_CLUSTER_MOUNTS,
            "too many cluster mounts"
        );
        for m in &self.cluster_mounts {
            m.validate()?;
        }
        self.resources
            .validate()
            .map_err(|e| anyhow::anyhow!("invalid bundle resources: {e}"))?;
        ensure!(
            (1..=MAX_ARRAY_LEN).contains(&self.array_len),
            "bundle array_len must be in 1..={MAX_ARRAY_LEN}"
        );
        Ok(())
    }

    /// Render standard `sha256sum -c` compatible manifest file for staged inputs.
    pub fn render_sha256sum_manifest(&self) -> String {
        let mut out = String::new();
        for (path, content) in &self.task_files {
            let digest = format!("{:x}", Sha256::digest(content.as_bytes()));
            out.push_str(&format!("{digest}  {path}\n"));
        }
        out
    }

    pub fn bundle_sha256(&self) -> Result<String> {
        self.validate()?;
        sha256_json("ai4math:staged-job-bundle:v1", self)
    }
}

/// Strictly validated numeric SLURM job ID (prevents CLI/shell injection in
/// `sacct`, `squeue`, and `scancel`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SlurmJobId {
    raw_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cluster: Option<String>,
}

impl SlurmJobId {
    /// Parse `sbatch --parsable` output (`"<digits>"` or `"<digits>;<cluster>"`).
    pub fn parse_parsable(stdout: &str) -> Result<Self> {
        let trimmed = stdout.trim();
        let (id_part, cluster_part) = match trimmed.split_once(';') {
            Some((id, cl)) => (id, Some(cl)),
            None => (trimmed, None),
        };
        Self::new(id_part, cluster_part)
    }

    pub fn new(raw_id: &str, cluster: Option<&str>) -> Result<Self> {
        ensure!(
            !raw_id.is_empty()
                && raw_id.len() <= 19
                && !raw_id.starts_with('0')
                && raw_id.bytes().all(|b| b.is_ascii_digit()),
            "invalid numeric SLURM job ID {raw_id:?}"
        );
        let cluster = match cluster {
            Some(c) => {
                ensure!(
                    is_safe_compute_token(c, 64),
                    "invalid SLURM cluster name {c:?}"
                );
                Some(c.to_owned())
            }
            None => None,
        };
        Ok(Self {
            raw_id: raw_id.to_owned(),
            cluster,
        })
    }

    pub fn as_str(&self) -> &str {
        &self.raw_id
    }

    pub fn cluster(&self) -> Option<&str> {
        self.cluster.as_deref()
    }
}

/// Closed, typed allowlist of `sbatch` CLI flags passed across `SlurmTransport`.
/// Arbitrary raw flag strings are forbidden.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "flag", rename_all = "snake_case")]
pub enum SlurmFlag {
    Partition(String),
    Account(String),
    Qos(String),
    Constraint(String),
    JobName(String),
    CpusPerTask(u16),
    MemMib(u64),
    GresGpu { kind: Option<String>, count: u8 },
    TimeSeconds(u64),
    ArrayRange { start: u32, end: u32 },
}

impl SlurmFlag {
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Partition(v) | Self::Account(v) | Self::Qos(v) | Self::Constraint(v) => {
                ensure!(
                    is_safe_compute_token(v, 64),
                    "invalid SLURM flag value {v:?}"
                );
            }
            Self::JobName(v) => {
                ensure!(
                    !v.is_empty()
                        && v.len() <= 64
                        && v.starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_')
                        && v.bytes()
                            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.')),
                    "invalid SLURM job name {v:?}"
                );
            }
            Self::CpusPerTask(n) => {
                ensure!(*n >= 1 && *n <= 1024, "invalid SLURM cpus-per-task");
            }
            Self::MemMib(m) => {
                ensure!(*m >= 1 && *m <= 16 * 1024 * 1024, "invalid SLURM mem MiB");
            }
            Self::GresGpu { kind, count } => {
                ensure!(*count >= 1 && *count <= 16, "invalid SLURM GPU count");
                if let Some(k) = kind {
                    ensure!(
                        !k.is_empty()
                            && k.len() <= 32
                            && k.bytes()
                                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-')),
                        "invalid SLURM GPU gres kind {k:?}"
                    );
                }
            }
            Self::TimeSeconds(secs) => {
                ensure!(
                    *secs >= 1 && *secs <= 30 * 24 * 3600,
                    "invalid SLURM walltime"
                );
            }
            Self::ArrayRange { start, end } => {
                ensure!(
                    start <= end && (end - start) < MAX_ARRAY_LEN,
                    "invalid SLURM array range {start}-{end}"
                );
            }
        }
        Ok(())
    }

    /// Format into one validated `--key=value` argument for `sbatch` (never split by a shell).
    pub fn to_sbatch_arg(&self) -> Result<String> {
        self.validate()?;
        Ok(match self {
            Self::Partition(v) => format!("--partition={v}"),
            Self::Account(v) => format!("--account={v}"),
            Self::Qos(v) => format!("--qos={v}"),
            Self::Constraint(v) => format!("--constraint={v}"),
            Self::JobName(v) => format!("--job-name={v}"),
            Self::CpusPerTask(n) => format!("--cpus-per-task={n}"),
            Self::MemMib(m) => format!("--mem={m}M"),
            Self::GresGpu {
                kind: Some(k),
                count,
            } => format!("--gres=gpu:{k}:{count}"),
            Self::GresGpu { kind: None, count } => format!("--gres=gpu:{count}"),
            Self::TimeSeconds(secs) => {
                let mins = secs.div_ceil(60);
                format!("--time={mins}")
            }
            Self::ArrayRange { start, end } => format!("--array={start}-{end}"),
        })
    }
}

/// Normalized SLURM job/task state parsed from `sacct` / `squeue`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SlurmJobState {
    Pending,
    Running,
    Completing,
    Completed,
    Failed,
    Timeout,
    OutOfMemory,
    NodeFail,
    Preempted,
    Cancelled,
    Unknown(String),
}

impl SlurmJobState {
    pub fn parse_slurm_token(raw: &str) -> Self {
        let head = raw.trim().split_ascii_whitespace().next().unwrap_or("");
        let base = head.trim_end_matches('+');
        match base.to_ascii_uppercase().as_str() {
            "PENDING" | "PD" | "CONFIGURING" | "CF" | "RESIZING" => Self::Pending,
            "RUNNING" | "R" | "STAGE_OUT" | "SO" => Self::Running,
            "COMPLETING" | "CG" => Self::Completing,
            "COMPLETED" | "CD" => Self::Completed,
            "TIMEOUT" | "TO" => Self::Timeout,
            "OUT_OF_MEMORY" | "OOM" => Self::OutOfMemory,
            "NODE_FAIL" | "NF" => Self::NodeFail,
            "PREEMPTED" | "PR" => Self::Preempted,
            "CANCELLED" | "CA" => Self::Cancelled,
            "FAILED" | "F" | "BOOT_FAIL" | "BF" | "DEADLINE" | "DL" => Self::Failed,
            other => Self::Unknown(other.to_owned()),
        }
    }

    pub fn to_compute_status(&self) -> ComputeJobStatus {
        match self {
            Self::Pending => ComputeJobStatus::Queued,
            Self::Running | Self::Completing => ComputeJobStatus::Running,
            Self::Completed => ComputeJobStatus::Completed,
            Self::Timeout => ComputeJobStatus::TimedOut,
            Self::Cancelled => ComputeJobStatus::Cancelled,
            Self::Failed
            | Self::OutOfMemory
            | Self::NodeFail
            | Self::Preempted
            | Self::Unknown(_) => ComputeJobStatus::Failed,
        }
    }
}

/// One job or array-task observation returned by `SlurmTransport::query_status`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SlurmJobObservation {
    pub job_id: SlurmJobId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub array_task_id: Option<u32>,
    pub state: SlurmJobState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    pub elapsed_seconds: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allocated_tres: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_list: Option<String>,
}

/// Allowlisted, size-bounded raw output files fetched back to the broker clean-room.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FetchedJobOutputs {
    pub files: BTreeMap<String, Vec<u8>>,
    pub total_bytes: u64,
}

impl FetchedJobOutputs {
    pub fn validate(&self, allowlist: &[String], max_total_bytes: u64) -> Result<()> {
        let allowed: BTreeSet<&str> = allowlist.iter().map(String::as_str).collect();
        let mut computed = 0u64;
        for (path, bytes) in &self.files {
            ensure!(
                is_safe_relative_bundle_path(path),
                "unsafe fetched output path {path:?}"
            );
            ensure!(
                allowed.contains(path.as_str()),
                "fetched output {path:?} is outside the frozen output_allowlist"
            );
            computed = computed
                .checked_add(u64::try_from(bytes.len())?)
                .context("fetched bytes overflow")?;
            ensure!(
                computed <= max_total_bytes,
                "fetched output exceeds max_total_bytes cap"
            );
        }
        ensure!(
            computed == self.total_bytes,
            "fetched output total_bytes mismatch"
        );
        Ok(())
    }
}

/// Execution provenance metadata capturing where and how a protocol job ran.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComputeExecutionAttestation {
    pub target_kind: ExecutionTargetKind,
    pub cluster_or_host_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_job_id: Option<String>,
    #[serde(default)]
    pub allocated_gpu_indices: Vec<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container_image_sha256: Option<String>,
    pub bundle_sha256: String,
    pub attested_locally: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ArtifactTemplate, BlindedControlCase, BlindedControlPack, BlindedControlPolicy,
        CandidateSubmission, MetricRule, NamedMetricRule, OrthogonalPolicy, ParamValue, SlotKind,
        SlotSpec, SlotValue, TextProfile,
    };
    use ai4math_core::{GpuKind, GpuRequest, ScientificTrust};

    fn test_protocol_and_controls() -> Result<(ProtocolSpec, BlindedControlPack)> {
        let controls = BlindedControlPack {
            version: 1,
            protocol_id: "md_fep_gpu_v1".into(),
            positive_controls: vec![BlindedControlCase {
                control_id: "secret_positive_ctrl".into(),
                slot_values: BTreeMap::from([(
                    "ligand_smiles".into(),
                    SlotValue::Text("c1ccccc1O".into()),
                )]),
            }],
            negative_controls: vec![BlindedControlCase {
                control_id: "secret_negative_decoy".into(),
                slot_values: BTreeMap::from([(
                    "ligand_smiles".into(),
                    SlotValue::Text("CCCCCCCC".into()),
                )]),
            }],
        };
        let protocol = ProtocolSpec {
            version: 1,
            protocol_id: "md_fep_gpu_v1".into(),
            domain_id: "compchem".into(),
            backend_id: "openmm_fep".into(),
            epistemic_ceiling: ScientificTrust::E4a,
            default_debts: vec!["classical_ff_only".into()],
            pinned_inputs: BTreeMap::from([("receptor_pdb".into(), "a".repeat(64))]),
            frozen_parameters: BTreeMap::from([("temp_k".into(), ParamValue::Float(300.0))]),
            slots: vec![SlotSpec {
                name: "ligand_smiles".into(),
                kind: SlotKind::Text {
                    max_len: 128,
                    profile: TextProfile::Smiles,
                },
            }],
            artifact_templates: vec![ArtifactTemplate {
                path: "job.cfg".into(),
                template: "temp={{param:temp_k}}\nsmiles={{slot:ligand_smiles}}\n".into(),
            }],
            sanity_gates: vec![NamedMetricRule {
                rule_id: "ok".into(),
                rule: MetricRule::EqualsBool {
                    metric: "converged".into(),
                    expected: true,
                },
            }],
            acceptance_criteria: vec![NamedMetricRule {
                rule_id: "ddg".into(),
                rule: MetricRule::MaxFloat {
                    metric: "ddg_kcal_mol".into(),
                    max: -1.5,
                },
            }],
            orthogonal_policy: OrthogonalPolicy::default(),
            control_policy: BlindedControlPolicy {
                control_pack_sha256: Some(controls.fingerprint()?),
                min_positive_controls: 1,
                min_negative_controls: 1,
            },
            preregistration_required: true,
            resource_ceiling: Some(ResourceRequest {
                cpu_cores: 8,
                memory_mib: 32_768,
                gpu: Some(GpuRequest {
                    count: 1,
                    min_vram_mib: 24_576,
                    kind: GpuKind::Cuda,
                    exclusive: true,
                }),
                scratch_mib: 10_240,
                max_wall_seconds: 7_200,
            }),
        };
        Ok((protocol, controls))
    }

    #[test]
    fn blinded_array_bundle_shuffles_opaquely_and_enforces_resource_ceiling() -> Result<()> {
        let (protocol, controls) = test_protocol_and_controls()?;
        let sub = CandidateSubmission {
            version: 1,
            protocol_sha256: protocol.protocol_sha256()?,
            candidate_id: "cand_alpha".into(),
            slot_values: BTreeMap::from([(
                "ligand_smiles".into(),
                SlotValue::Text("CC(=O)O".into()),
            )]),
        };
        let compiled_cand = protocol.compile_submission(&sub)?;
        let compiled_ctrl = controls.compile_for(&protocol)?;

        // Over-ceiling GPU request is rejected before bundle creation.
        let over_budget = ResourceRequest {
            cpu_cores: 8,
            memory_mib: 32_768,
            gpu: Some(GpuRequest {
                count: 4,
                min_vram_mib: 24_576,
                kind: GpuKind::Cuda,
                exclusive: true,
            }),
            scratch_mib: 10_240,
            max_wall_seconds: 7_200,
        };
        assert!(
            build_blinded_array_bundle(BlindedArrayBundleRequest {
                job_id: "job_01",
                base_commit: &"b".repeat(40),
                protocol: &protocol,
                candidate: &compiled_cand,
                controls: Some(&compiled_ctrl),
                resources: over_budget,
                container_image_sha256: Some("c".repeat(64)),
                cluster_mounts: vec![],
                output_file_per_task: "result.json",
                shuffle_nonce: &[7u8; 32],
            })
            .is_err()
        );

        let valid_res = protocol.resource_ceiling.clone().unwrap();
        let mounts = vec![ClusterMountEntry {
            input_name: "receptor_pdb".into(),
            remote_ro_path: "/datasets/pdb/receptor_1abc.pdb".into(),
            expected_sha256: "a".repeat(64),
            container_mount_path: "/inputs/receptor.pdb".into(),
        }];
        let (bundle, plan) = build_blinded_array_bundle(BlindedArrayBundleRequest {
            job_id: "job_01",
            base_commit: &"b".repeat(40),
            protocol: &protocol,
            candidate: &compiled_cand,
            controls: Some(&compiled_ctrl),
            resources: valid_res,
            container_image_sha256: Some("c".repeat(64)),
            cluster_mounts: mounts,
            output_file_per_task: "result.json",
            shuffle_nonce: &[42u8; 32],
        })?;
        assert_eq!(bundle.array_len, 3);
        assert_eq!(plan.array_len(), 3);
        assert!(plan.candidate_task_index().is_some());

        // Staged filenames and serialized bundle MUST NOT mention candidate/control labels!
        let bundle_json = serde_json::to_string(&bundle)?;
        assert!(!bundle_json.contains("secret_positive_ctrl"));
        assert!(!bundle_json.contains("secret_negative_decoy"));
        assert!(!bundle_json.contains("cand_alpha"));
        assert!(bundle.task_files.contains_key("tasks/0/job.cfg"));
        assert!(bundle.task_files.contains_key("tasks/1/job.cfg"));
        assert!(bundle.task_files.contains_key("tasks/2/job.cfg"));
        assert_eq!(
            bundle.output_allowlist,
            vec![
                "tasks/0/result.json",
                "tasks/1/result.json",
                "tasks/2/result.json"
            ]
        );
        Ok(())
    }

    #[test]
    fn slurm_job_id_and_flags_block_injection() {
        assert_eq!(
            SlurmJobId::parse_parsable("1234567\n").unwrap().as_str(),
            "1234567"
        );
        let with_cluster = SlurmJobId::parse_parsable("98765;gpu_cluster").unwrap();
        assert_eq!(with_cluster.as_str(), "98765");
        assert_eq!(with_cluster.cluster(), Some("gpu_cluster"));
        for bad in ["0", "0123", "123;rm -rf /", "$(whoami)", "-1", "abc"] {
            assert!(SlurmJobId::parse_parsable(bad).is_err(), "{bad:?}");
        }
        assert!(
            SlurmFlag::Partition("gpu-a100;echo pwned".into())
                .to_sbatch_arg()
                .is_err()
        );
        assert!(
            SlurmFlag::Account("--wrap=evil".into())
                .to_sbatch_arg()
                .is_err()
        );
        assert_eq!(
            SlurmFlag::GresGpu {
                kind: Some("a100".into()),
                count: 2
            }
            .to_sbatch_arg()
            .unwrap(),
            "--gres=gpu:a100:2"
        );
        assert_eq!(
            SlurmFlag::TimeSeconds(61).to_sbatch_arg().unwrap(),
            "--time=2"
        );
    }

    #[test]
    fn cluster_mount_map_rejects_mismatched_hash_and_shell_paths() -> Result<()> {
        let (protocol, _) = test_protocol_and_controls()?;
        let bad_hash = ClusterMountMap {
            entries: vec![ClusterMountEntry {
                input_name: "receptor_pdb".into(),
                remote_ro_path: "/datasets/receptor.pdb".into(),
                expected_sha256: "f".repeat(64),
                container_mount_path: "/inputs/receptor.pdb".into(),
            }],
        };
        assert!(bad_hash.validate_against_protocol(&protocol).is_err());

        let bad_path = ClusterMountMap {
            entries: vec![ClusterMountEntry {
                input_name: "receptor_pdb".into(),
                remote_ro_path: "/datasets/../etc/passwd".into(),
                expected_sha256: "a".repeat(64),
                container_mount_path: "/inputs/receptor.pdb".into(),
            }],
        };
        assert!(bad_path.validate_against_protocol(&protocol).is_err());
        Ok(())
    }
}
