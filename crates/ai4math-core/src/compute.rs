//! Domain-agnostic multi-dimensional compute resource models, GPU requests,
//! SLURM cluster quota policies, and compute pool telemetry snapshots.
//!
//! Zero-IO pure logic shared across `ai4math-evidence`, `ai4math-broker`,
//! `ai4math-store`, and `ai4math-daemon`.

use crate::wire_enum;
use serde::{Deserialize, Serialize};
use std::fmt;

pub const MAX_CPU_CORES: u16 = 1024;
pub const MAX_MEMORY_MIB: u64 = 16 * 1024 * 1024; // 16 TiB
pub const MAX_SCRATCH_MIB: u64 = 64 * 1024 * 1024; // 64 TiB
pub const MAX_GPU_COUNT: u8 = 16;
pub const MAX_GPU_VRAM_MIB: u64 = 2 * 1024 * 1024; // 2 TiB per card
pub const MAX_WALL_SECONDS: u64 = 30 * 24 * 3600; // 30 days
pub const MAX_SLURM_ACTIVE_JOBS: u32 = 10_000;
pub const MAX_FETCH_BYTES_CAP: u64 = 512 * 1024 * 1024; // 512 MiB

wire_enum!(
    /// GPU accelerator architecture family requested or provided by a host/cluster.
    GpuKind {
        Cuda => "cuda",
        Rocm => "rocm",
        Metal => "metal",
        Any => "any",
    }
);

impl GpuKind {
    /// Whether a device of kind `device` satisfies a request for `self`.
    pub const fn matches_device(self, device: Self) -> bool {
        matches!(
            (self, device),
            (Self::Any, _)
                | (_, Self::Any)
                | (Self::Cuda, Self::Cuda)
                | (Self::Rocm, Self::Rocm)
                | (Self::Metal, Self::Metal)
        )
    }
}

wire_enum!(
    /// Execution carrier hosting a compiled verification job.
    ExecutionTargetKind {
        LocalSandbox => "local_sandbox",
        Slurm => "slurm",
    }
);

wire_enum!(
    /// Lifecycle state of an asynchronous local or external compute job.
    ComputeJobStatus {
        Queued => "queued",
        Running => "running",
        Completed => "completed",
        Failed => "failed",
        TimedOut => "timed_out",
        Cancelled => "cancelled",
    }
);

impl ComputeJobStatus {
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::TimedOut | Self::Cancelled
        )
    }

    pub const fn is_active(self) -> bool {
        !self.is_terminal()
    }
}

/// GPU allocation requirement within a `ResourceRequest`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GpuRequest {
    pub count: u8,
    pub min_vram_mib: u64,
    pub kind: GpuKind,
    /// When `true`, each allocated card is exclusively owned; when `false`,
    /// cards may be shared by VRAM quota accounting on local hosts.
    pub exclusive: bool,
}

impl GpuRequest {
    pub fn validate(&self) -> Result<(), ResourcePolicyError> {
        if self.count == 0 || self.count > MAX_GPU_COUNT {
            return Err(ResourcePolicyError::InvalidField(
                "gpu.count must be in 1..=16",
            ));
        }
        if self.min_vram_mib == 0 || self.min_vram_mib > MAX_GPU_VRAM_MIB {
            return Err(ResourcePolicyError::InvalidField(
                "gpu.min_vram_mib must be positive and within cap",
            ));
        }
        Ok(())
    }
}

/// Multi-dimensional resource request for one verification job or array task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceRequest {
    pub cpu_cores: u16,
    pub memory_mib: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu: Option<GpuRequest>,
    #[serde(default)]
    pub scratch_mib: u64,
    pub max_wall_seconds: u64,
}

impl ResourceRequest {
    pub fn cpu_only(cpu_cores: u16, memory_mib: u64, max_wall_seconds: u64) -> Self {
        Self {
            cpu_cores,
            memory_mib,
            gpu: None,
            scratch_mib: 0,
            max_wall_seconds,
        }
    }

    pub fn gpu_count(&self) -> u32 {
        self.gpu.as_ref().map_or(0, |g| u32::from(g.count))
    }

    pub fn validate(&self) -> Result<(), ResourcePolicyError> {
        if self.cpu_cores == 0 || self.cpu_cores > MAX_CPU_CORES {
            return Err(ResourcePolicyError::InvalidField(
                "cpu_cores must be in 1..=1024",
            ));
        }
        if self.memory_mib == 0 || self.memory_mib > MAX_MEMORY_MIB {
            return Err(ResourcePolicyError::InvalidField(
                "memory_mib must be positive and within cap",
            ));
        }
        if self.scratch_mib > MAX_SCRATCH_MIB {
            return Err(ResourcePolicyError::InvalidField("scratch_mib exceeds cap"));
        }
        if self.max_wall_seconds == 0 || self.max_wall_seconds > MAX_WALL_SECONDS {
            return Err(ResourcePolicyError::InvalidField(
                "max_wall_seconds must be positive and within 30 days",
            ));
        }
        if let Some(gpu) = &self.gpu {
            gpu.validate()?;
        }
        Ok(())
    }

    /// Enforce that this request does not exceed a frozen protocol or cluster ceiling.
    pub fn check_within_ceiling(&self, ceiling: &Self) -> Result<(), ResourcePolicyError> {
        self.validate()?;
        ceiling.validate()?;
        if self.cpu_cores > ceiling.cpu_cores {
            return Err(ResourcePolicyError::CeilingExceeded {
                dimension: "cpu_cores",
                requested: u64::from(self.cpu_cores),
                ceiling: u64::from(ceiling.cpu_cores),
            });
        }
        if self.memory_mib > ceiling.memory_mib {
            return Err(ResourcePolicyError::CeilingExceeded {
                dimension: "memory_mib",
                requested: self.memory_mib,
                ceiling: ceiling.memory_mib,
            });
        }
        if self.scratch_mib > ceiling.scratch_mib {
            return Err(ResourcePolicyError::CeilingExceeded {
                dimension: "scratch_mib",
                requested: self.scratch_mib,
                ceiling: ceiling.scratch_mib,
            });
        }
        if self.max_wall_seconds > ceiling.max_wall_seconds {
            return Err(ResourcePolicyError::CeilingExceeded {
                dimension: "max_wall_seconds",
                requested: self.max_wall_seconds,
                ceiling: ceiling.max_wall_seconds,
            });
        }
        match (&self.gpu, &ceiling.gpu) {
            (None, _) => Ok(()),
            (Some(req), None) => Err(ResourcePolicyError::CeilingExceeded {
                dimension: "gpu.count",
                requested: u64::from(req.count),
                ceiling: 0,
            }),
            (Some(req), Some(cap)) => {
                if req.count > cap.count {
                    return Err(ResourcePolicyError::CeilingExceeded {
                        dimension: "gpu.count",
                        requested: u64::from(req.count),
                        ceiling: u64::from(cap.count),
                    });
                }
                if req.min_vram_mib > cap.min_vram_mib {
                    return Err(ResourcePolicyError::CeilingExceeded {
                        dimension: "gpu.min_vram_mib",
                        requested: req.min_vram_mib,
                        ceiling: cap.min_vram_mib,
                    });
                }
                if cap.kind != GpuKind::Any && req.kind != cap.kind {
                    return Err(ResourcePolicyError::IncompatibleGpuKind {
                        requested: req.kind,
                        allowed: cap.kind,
                    });
                }
                if req.exclusive && !cap.exclusive {
                    return Err(ResourcePolicyError::InvalidField(
                        "exclusive GPU lock requested but ceiling only permits shared VRAM allocation",
                    ));
                }
                Ok(())
            }
        }
    }
}

fn is_safe_slurm_token(value: &str, max_len: usize) -> bool {
    !value.is_empty()
        && value.len() <= max_len
        && value.starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_')
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b':'))
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Operator-frozen SLURM cluster quota and allowlist guardrails.
///
/// Subagents cannot bypass allowed partitions/accounts/QOS, exceed per-job or
/// cluster-wide active job/GPU ceilings, or drop immutable container digest checks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SlurmClusterPolicy {
    pub cluster_id: String,
    pub allowed_partitions: Vec<String>,
    #[serde(default)]
    pub allowed_accounts: Vec<String>,
    #[serde(default)]
    pub allowed_qos: Vec<String>,
    pub per_job_ceiling: ResourceRequest,
    pub max_active_jobs: u32,
    pub max_total_active_gpus: u32,
    pub require_container_digest: bool,
    pub max_fetch_bytes: u64,
}

impl SlurmClusterPolicy {
    pub fn validate(&self) -> Result<(), ResourcePolicyError> {
        if !is_safe_slurm_token(&self.cluster_id, 64) {
            return Err(ResourcePolicyError::InvalidField("invalid cluster_id"));
        }
        if self.allowed_partitions.is_empty() || self.allowed_partitions.len() > 64 {
            return Err(ResourcePolicyError::InvalidField(
                "allowed_partitions must contain 1..=64 safe names",
            ));
        }
        if !self
            .allowed_partitions
            .iter()
            .all(|p| is_safe_slurm_token(p, 64))
        {
            return Err(ResourcePolicyError::InvalidField(
                "invalid partition name in allowed_partitions",
            ));
        }
        if self.allowed_accounts.len() > 64
            || !self
                .allowed_accounts
                .iter()
                .all(|a| is_safe_slurm_token(a, 64))
        {
            return Err(ResourcePolicyError::InvalidField(
                "invalid account name in allowed_accounts",
            ));
        }
        if self.allowed_qos.len() > 64
            || !self.allowed_qos.iter().all(|q| is_safe_slurm_token(q, 64))
        {
            return Err(ResourcePolicyError::InvalidField(
                "invalid qos name in allowed_qos",
            ));
        }
        self.per_job_ceiling.validate()?;
        if self.max_active_jobs == 0 || self.max_active_jobs > MAX_SLURM_ACTIVE_JOBS {
            return Err(ResourcePolicyError::InvalidField(
                "max_active_jobs must be in 1..=10000",
            ));
        }
        if self.max_fetch_bytes == 0 || self.max_fetch_bytes > MAX_FETCH_BYTES_CAP {
            return Err(ResourcePolicyError::InvalidField(
                "max_fetch_bytes must be positive and within 512 MiB cap",
            ));
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn validate_submission(
        &self,
        partition: &str,
        account: Option<&str>,
        qos: Option<&str>,
        resources: &ResourceRequest,
        array_len: u32,
        container_image_sha256: Option<&str>,
        current_active_jobs: u32,
        current_active_gpus: u32,
    ) -> Result<(), ResourcePolicyError> {
        self.validate()?;
        if !self.allowed_partitions.iter().any(|p| p == partition) {
            return Err(ResourcePolicyError::DisallowedClusterTarget {
                field: "partition",
                value: partition.to_owned(),
            });
        }
        if !self.allowed_accounts.is_empty() {
            match account {
                Some(acc) if self.allowed_accounts.iter().any(|a| a == acc) => {}
                Some(acc) => {
                    return Err(ResourcePolicyError::DisallowedClusterTarget {
                        field: "account",
                        value: acc.to_owned(),
                    });
                }
                None => {
                    return Err(ResourcePolicyError::InvalidField(
                        "cluster policy requires an allowlisted SLURM account",
                    ));
                }
            }
        } else if let Some(acc) = account
            && !is_safe_slurm_token(acc, 64)
        {
            return Err(ResourcePolicyError::InvalidField("invalid SLURM account"));
        }
        if !self.allowed_qos.is_empty() {
            match qos {
                Some(q) if self.allowed_qos.iter().any(|allowed| allowed == q) => {}
                Some(q) => {
                    return Err(ResourcePolicyError::DisallowedClusterTarget {
                        field: "qos",
                        value: q.to_owned(),
                    });
                }
                None => {
                    return Err(ResourcePolicyError::InvalidField(
                        "cluster policy requires an allowlisted SLURM qos",
                    ));
                }
            }
        } else if let Some(q) = qos
            && !is_safe_slurm_token(q, 64)
        {
            return Err(ResourcePolicyError::InvalidField("invalid SLURM qos"));
        }
        if array_len == 0 || array_len > 128 {
            return Err(ResourcePolicyError::InvalidField(
                "SLURM array_len must be in 1..=128",
            ));
        }
        resources.check_within_ceiling(&self.per_job_ceiling)?;
        if self.require_container_digest {
            match container_image_sha256 {
                Some(sha) if is_sha256_hex(sha) => {}
                _ => {
                    return Err(ResourcePolicyError::InvalidField(
                        "cluster policy requires a pinned lowercase 64-hex container image SHA-256",
                    ));
                }
            }
        } else if let Some(sha) = container_image_sha256
            && !is_sha256_hex(sha)
        {
            return Err(ResourcePolicyError::InvalidField(
                "container_image_sha256 must be a lowercase 64-hex SHA-256",
            ));
        }
        let next_jobs = current_active_jobs.saturating_add(1);
        if next_jobs > self.max_active_jobs {
            return Err(ResourcePolicyError::CeilingExceeded {
                dimension: "cluster.max_active_jobs",
                requested: u64::from(next_jobs),
                ceiling: u64::from(self.max_active_jobs),
            });
        }
        let requested_total_gpus = resources.gpu_count().saturating_mul(array_len);
        let next_gpus = current_active_gpus.saturating_add(requested_total_gpus);
        if next_gpus > self.max_total_active_gpus {
            return Err(ResourcePolicyError::CeilingExceeded {
                dimension: "cluster.max_total_active_gpus",
                requested: u64::from(next_gpus),
                ceiling: u64::from(self.max_total_active_gpus),
            });
        }
        Ok(())
    }
}

/// Errors emitted when validating resource requests, ceilings, or SLURM policies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResourcePolicyError {
    InvalidField(&'static str),
    CeilingExceeded {
        dimension: &'static str,
        requested: u64,
        ceiling: u64,
    },
    IncompatibleGpuKind {
        requested: GpuKind,
        allowed: GpuKind,
    },
    DisallowedClusterTarget {
        field: &'static str,
        value: String,
    },
}

impl fmt::Display for ResourcePolicyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidField(msg) => f.write_str(msg),
            Self::CeilingExceeded {
                dimension,
                requested,
                ceiling,
            } => write!(
                f,
                "resource ceiling exceeded for {dimension}: requested {requested}, ceiling {ceiling}"
            ),
            Self::IncompatibleGpuKind { requested, allowed } => write!(
                f,
                "requested GPU kind {requested} is incompatible with allowed {allowed}"
            ),
            Self::DisallowedClusterTarget { field, value } => {
                write!(f, "SLURM {field} {value:?} is not in cluster allowlist")
            }
        }
    }
}

impl std::error::Error for ResourcePolicyError {}

/// Live per-device GPU telemetry snapshot for HUD/daemon observability.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GpuDeviceSnapshot {
    pub index: u32,
    pub kind: GpuKind,
    pub total_vram_mib: u64,
    pub free_vram_mib: u64,
    pub unified_memory: bool,
    pub exclusive_busy: bool,
    pub shared_allocations: u32,
}

/// Multi-dimensional compute pool telemetry snapshot (complements `BrokerLoad`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComputePoolSnapshot {
    pub total_cpu_cores: u16,
    pub used_cpu_cores: u16,
    pub total_memory_mib: u64,
    pub used_memory_mib: u64,
    pub total_scratch_mib: u64,
    pub used_scratch_mib: u64,
    pub gpus: Vec<GpuDeviceSnapshot>,
    pub active_leases: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_ceiling_checks_cpu_ram_gpu_and_exclusivity() {
        let ceiling = ResourceRequest {
            cpu_cores: 16,
            memory_mib: 65_536,
            gpu: Some(GpuRequest {
                count: 2,
                min_vram_mib: 24_576,
                kind: GpuKind::Cuda,
                exclusive: true,
            }),
            scratch_mib: 102_400,
            max_wall_seconds: 14_400,
        };
        let valid = ResourceRequest {
            cpu_cores: 8,
            memory_mib: 32_768,
            gpu: Some(GpuRequest {
                count: 1,
                min_vram_mib: 16_384,
                kind: GpuKind::Cuda,
                exclusive: false,
            }),
            scratch_mib: 20_480,
            max_wall_seconds: 3_600,
        };
        assert!(valid.check_within_ceiling(&ceiling).is_ok());

        let too_many_gpus = ResourceRequest {
            gpu: Some(GpuRequest {
                count: 4,
                min_vram_mib: 16_384,
                kind: GpuKind::Cuda,
                exclusive: true,
            }),
            ..valid.clone()
        };
        assert!(matches!(
            too_many_gpus.check_within_ceiling(&ceiling),
            Err(ResourcePolicyError::CeilingExceeded {
                dimension: "gpu.count",
                ..
            })
        ));

        let wrong_kind = ResourceRequest {
            gpu: Some(GpuRequest {
                count: 1,
                min_vram_mib: 16_384,
                kind: GpuKind::Rocm,
                exclusive: false,
            }),
            ..valid
        };
        assert!(matches!(
            wrong_kind.check_within_ceiling(&ceiling),
            Err(ResourcePolicyError::IncompatibleGpuKind { .. })
        ));

        // Shared-only ceiling rejects exclusive GPU lock.
        let shared_ceiling = ResourceRequest {
            gpu: Some(GpuRequest {
                count: 1,
                min_vram_mib: 24_576,
                kind: GpuKind::Cuda,
                exclusive: false,
            }),
            ..ceiling
        };
        let excl_req = ResourceRequest {
            cpu_cores: 4,
            memory_mib: 8192,
            gpu: Some(GpuRequest {
                count: 1,
                min_vram_mib: 8192,
                kind: GpuKind::Cuda,
                exclusive: true,
            }),
            scratch_mib: 0,
            max_wall_seconds: 600,
        };
        assert!(excl_req.check_within_ceiling(&shared_ceiling).is_err());
    }

    #[test]
    fn slurm_cluster_policy_blocks_unlisted_partitions_missing_digests_and_gpu_overflow() {
        let policy = SlurmClusterPolicy {
            cluster_id: "hpc_gpu_01".into(),
            allowed_partitions: vec!["gpu-a100".into()],
            allowed_accounts: vec!["chem-lab".into()],
            allowed_qos: vec!["normal".into()],
            per_job_ceiling: ResourceRequest {
                cpu_cores: 16,
                memory_mib: 65_536,
                gpu: Some(GpuRequest {
                    count: 1,
                    min_vram_mib: 40_960,
                    kind: GpuKind::Cuda,
                    exclusive: true,
                }),
                scratch_mib: 50_000,
                max_wall_seconds: 7_200,
            },
            max_active_jobs: 4,
            max_total_active_gpus: 4,
            require_container_digest: true,
            max_fetch_bytes: 16 * 1024 * 1024,
        };
        let req = ResourceRequest {
            cpu_cores: 8,
            memory_mib: 32_768,
            gpu: Some(GpuRequest {
                count: 1,
                min_vram_mib: 24_576,
                kind: GpuKind::Cuda,
                exclusive: true,
            }),
            scratch_mib: 10_000,
            max_wall_seconds: 3_600,
        };
        let digest = "a".repeat(64);
        assert!(
            policy
                .validate_submission(
                    "gpu-a100",
                    Some("chem-lab"),
                    Some("normal"),
                    &req,
                    3,
                    Some(&digest),
                    1,
                    1,
                )
                .is_ok()
        );
        // Missing container digest rejected.
        assert!(
            policy
                .validate_submission(
                    "gpu-a100",
                    Some("chem-lab"),
                    Some("normal"),
                    &req,
                    1,
                    None,
                    0,
                    0,
                )
                .is_err()
        );
        // Unlisted partition rejected.
        assert!(
            policy
                .validate_submission(
                    "unlimited-debug",
                    Some("chem-lab"),
                    Some("normal"),
                    &req,
                    1,
                    Some(&digest),
                    0,
                    0,
                )
                .is_err()
        );
        // Array of 4 x 1 GPU + 1 active GPU = 5 > max_total_active_gpus(4) -> rejected!
        assert!(
            policy
                .validate_submission(
                    "gpu-a100",
                    Some("chem-lab"),
                    Some("normal"),
                    &req,
                    4,
                    Some(&digest),
                    1,
                    1,
                )
                .is_err()
        );
    }
}
