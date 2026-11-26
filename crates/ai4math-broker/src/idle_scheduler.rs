//! Idle-capacity verification scheduler enforcing zero foreground starvation,
//! independent idle compute budgets, and dual-queue (Human + BP VoI) selection
//! (ADR-0007 D33).

use crate::BrokerLoad;
use ai4math_core::{ComputePoolSnapshot, ExecutionTargetKind, ReproducibilityClass};
use serde::{Deserialize, Serialize};

/// Independent compute budget capping total idle-time verification expenditure.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdleBudgetPolicy {
    pub max_total_jobs: u32,
    pub max_total_wall_seconds: f64,
    pub max_total_gpu_seconds: f64,
}

impl Default for IdleBudgetPolicy {
    fn default() -> Self {
        Self {
            max_total_jobs: 50,
            max_total_wall_seconds: 8.0 * 3600.0,
            max_total_gpu_seconds: 4.0 * 3600.0,
        }
    }
}

/// Zero-starvation idle admission watermarks and queue prioritization policy.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdleAdmissionPolicy {
    /// Maximum local CPU core utilization ratio allowed before deferring idle work.
    pub max_cpu_utilization: f64,
    /// Maximum local host RAM utilization ratio allowed before deferring idle work.
    pub max_memory_utilization: f64,
    /// Maximum local GPU utilization ratio allowed before deferring idle work.
    pub max_gpu_utilization: f64,
    /// Maximum SLURM active job/GPU quota ratio allowed before deferring idle work.
    pub max_slurm_utilization: f64,
    /// When true, ANY running or queued foreground Heavy lane job blocks idle dispatch.
    pub require_empty_heavy_lane: bool,
    /// When true, explicit Human-submitted verification tickets preempt automatic VoI tickets.
    pub prefer_human_queue: bool,
    pub budget: IdleBudgetPolicy,
}

impl Default for IdleAdmissionPolicy {
    fn default() -> Self {
        Self {
            max_cpu_utilization: 0.50,
            max_memory_utilization: 0.50,
            max_gpu_utilization: 0.50,
            max_slurm_utilization: 0.50,
            require_empty_heavy_lane: true,
            prefer_human_queue: true,
            budget: IdleBudgetPolicy::default(),
        }
    }
}

/// Live system load & idle budget snapshot evaluated by `IdleVerificationScheduler`.
#[derive(Debug, Clone)]
pub struct IdleSystemSnapshot {
    pub broker_load: BrokerLoad,
    pub host_memory_green: bool,
    pub local_pool: Option<ComputePoolSnapshot>,
    pub slurm_active_jobs: u32,
    pub slurm_max_jobs: u32,
    pub slurm_active_gpus: u32,
    pub slurm_max_gpus: u32,
    pub idle_jobs_executed: u32,
    pub idle_wall_seconds_used: f64,
    pub idle_gpu_seconds_used: f64,
}

/// Candidate idle ticket presented to the scheduler.
#[derive(Debug, Clone, PartialEq)]
pub struct IdleCandidate {
    pub ticket_id: String,
    pub is_human_queued: bool,
    pub priority_score: f64,
    pub reproducibility_class: ReproducibilityClass,
    pub target_kind: ExecutionTargetKind,
    pub requires_gpu: bool,
}

/// Deterministic scheduling decision emitted by `IdleVerificationScheduler`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum IdleDispatchDecision {
    Admit { ticket_id: String, reason: String },
    Defer { reasons: Vec<String> },
}

/// Pure, deterministic idle-capacity verification gate and selector.
#[derive(Debug, Clone)]
pub struct IdleVerificationScheduler {
    policy: IdleAdmissionPolicy,
}

impl IdleVerificationScheduler {
    pub fn new(policy: IdleAdmissionPolicy) -> Self {
        Self { policy }
    }

    pub fn policy(&self) -> &IdleAdmissionPolicy {
        &self.policy
    }

    /// Evaluate whether the system currently has idle headroom & remaining budget
    /// for a background verification ticket targeting `target_kind`.
    pub fn check_headroom(
        &self,
        snapshot: &IdleSystemSnapshot,
        target_kind: ExecutionTargetKind,
        requires_gpu: bool,
    ) -> Result<(), Vec<String>> {
        let mut reasons = Vec::new();

        if !snapshot.host_memory_green {
            reasons.push("host memory pressure is not Green".to_owned());
        }
        if self.policy.require_empty_heavy_lane
            && (snapshot.broker_load.heavy_running > 0 || snapshot.broker_load.heavy_queued > 0)
        {
            reasons.push(format!(
                "foreground Heavy lane busy (running={}, queued={})",
                snapshot.broker_load.heavy_running, snapshot.broker_load.heavy_queued
            ));
        }

        // Independent idle budget guardrails.
        if snapshot.idle_jobs_executed >= self.policy.budget.max_total_jobs {
            reasons.push(format!(
                "idle job budget exhausted ({}/{})",
                snapshot.idle_jobs_executed, self.policy.budget.max_total_jobs
            ));
        }
        if snapshot.idle_wall_seconds_used >= self.policy.budget.max_total_wall_seconds {
            reasons.push(format!(
                "idle wall-second budget exhausted ({:.1}/{:.1})",
                snapshot.idle_wall_seconds_used, self.policy.budget.max_total_wall_seconds
            ));
        }
        if requires_gpu
            && snapshot.idle_gpu_seconds_used >= self.policy.budget.max_total_gpu_seconds
        {
            reasons.push(format!(
                "idle GPU-second budget exhausted ({:.1}/{:.1})",
                snapshot.idle_gpu_seconds_used, self.policy.budget.max_total_gpu_seconds
            ));
        }

        match target_kind {
            ExecutionTargetKind::LocalSandbox => {
                if let Some(pool) = &snapshot.local_pool {
                    if pool.total_cpu_cores > 0 {
                        let cpu_util =
                            f64::from(pool.used_cpu_cores) / f64::from(pool.total_cpu_cores);
                        if cpu_util > self.policy.max_cpu_utilization {
                            reasons.push(format!(
                                "local CPU utilization {:.0}% exceeds idle watermark {:.0}%",
                                cpu_util * 100.0,
                                self.policy.max_cpu_utilization * 100.0
                            ));
                        }
                    }
                    if pool.total_memory_mib > 0 {
                        let mem_util =
                            (pool.used_memory_mib as f64) / (pool.total_memory_mib as f64);
                        if mem_util > self.policy.max_memory_utilization {
                            reasons.push(format!(
                                "local RAM utilization {:.0}% exceeds idle watermark {:.0}%",
                                mem_util * 100.0,
                                self.policy.max_memory_utilization * 100.0
                            ));
                        }
                    }
                    if requires_gpu {
                        let total_vram: u64 = pool.gpus.iter().map(|g| g.total_vram_mib).sum();
                        let free_vram: u64 = pool.gpus.iter().map(|g| g.free_vram_mib).sum();
                        if total_vram == 0 {
                            reasons.push("no local GPU available for GPU idle ticket".to_owned());
                        } else {
                            let gpu_util = ((total_vram - free_vram) as f64) / (total_vram as f64);
                            if gpu_util > self.policy.max_gpu_utilization {
                                reasons.push(format!(
                                    "local GPU VRAM utilization {:.0}% exceeds idle watermark {:.0}%",
                                    gpu_util * 100.0,
                                    self.policy.max_gpu_utilization * 100.0
                                ));
                            }
                        }
                    }
                }
            }
            ExecutionTargetKind::Slurm => {
                if snapshot.slurm_max_jobs == 0 {
                    reasons.push("SLURM cluster quota is zero".to_owned());
                } else {
                    let job_util =
                        f64::from(snapshot.slurm_active_jobs) / f64::from(snapshot.slurm_max_jobs);
                    if job_util > self.policy.max_slurm_utilization {
                        reasons.push(format!(
                            "SLURM active job utilization {:.0}% exceeds idle watermark {:.0}%",
                            job_util * 100.0,
                            self.policy.max_slurm_utilization * 100.0
                        ));
                    }
                }
                if requires_gpu {
                    if snapshot.slurm_max_gpus == 0 {
                        reasons.push("SLURM GPU quota is zero".to_owned());
                    } else {
                        let gpu_util = f64::from(snapshot.slurm_active_gpus)
                            / f64::from(snapshot.slurm_max_gpus);
                        if gpu_util > self.policy.max_slurm_utilization {
                            reasons.push(format!(
                                "SLURM GPU utilization {:.0}% exceeds idle watermark {:.0}%",
                                gpu_util * 100.0,
                                self.policy.max_slurm_utilization * 100.0
                            ));
                        }
                    }
                }
            }
        }

        if reasons.is_empty() {
            Ok(())
        } else {
            Err(reasons)
        }
    }

    /// Select the highest-priority eligible idle ticket whose execution target
    /// currently satisfies all zero-starvation and idle-budget guardrails.
    pub fn select_next(
        &self,
        snapshot: &IdleSystemSnapshot,
        candidates: &[IdleCandidate],
    ) -> IdleDispatchDecision {
        let mut eligible: Vec<&IdleCandidate> = candidates
            .iter()
            .filter(|c| c.reproducibility_class.is_automatable())
            .collect();
        if eligible.is_empty() {
            return IdleDispatchDecision::Defer {
                reasons: vec!["no automatable idle verification tickets queued".to_owned()],
            };
        }

        eligible.sort_by(|a, b| {
            if self.policy.prefer_human_queue {
                b.is_human_queued
                    .cmp(&a.is_human_queued)
                    .then_with(|| b.priority_score.total_cmp(&a.priority_score))
                    .then_with(|| a.ticket_id.cmp(&b.ticket_id))
            } else {
                b.priority_score
                    .total_cmp(&a.priority_score)
                    .then_with(|| b.is_human_queued.cmp(&a.is_human_queued))
                    .then_with(|| a.ticket_id.cmp(&b.ticket_id))
            }
        });

        let mut last_reasons = Vec::new();
        for cand in eligible {
            match self.check_headroom(snapshot, cand.target_kind, cand.requires_gpu) {
                Ok(()) => {
                    let origin = if cand.is_human_queued {
                        "human_queue"
                    } else {
                        "bp_voi_auto"
                    };
                    return IdleDispatchDecision::Admit {
                        ticket_id: cand.ticket_id.clone(),
                        reason: format!(
                            "admitted {origin} ticket (priority={:.4}, target={})",
                            cand.priority_score, cand.target_kind
                        ),
                    };
                }
                Err(reasons) => {
                    last_reasons = reasons;
                }
            }
        }
        IdleDispatchDecision::Defer {
            reasons: last_reasons,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ai4math_core::{GpuDeviceSnapshot, GpuKind};

    fn idle_snapshot() -> IdleSystemSnapshot {
        IdleSystemSnapshot {
            broker_load: BrokerLoad {
                fast_busy: 0,
                fast_capacity: 4,
                lean_advisory_busy: 0,
                lean_advisory_capacity: 4,
                heavy_running: 0,
                heavy_queued: 0,
            },
            host_memory_green: true,
            local_pool: Some(ComputePoolSnapshot {
                total_cpu_cores: 16,
                used_cpu_cores: 2,
                total_memory_mib: 65_536,
                used_memory_mib: 8_192,
                total_scratch_mib: 100_000,
                used_scratch_mib: 0,
                gpus: vec![GpuDeviceSnapshot {
                    index: 0,
                    kind: GpuKind::Cuda,
                    total_vram_mib: 24_576,
                    free_vram_mib: 24_576,
                    unified_memory: false,
                    exclusive_busy: false,
                    shared_allocations: 0,
                }],
                active_leases: 0,
            }),
            slurm_active_jobs: 0,
            slurm_max_jobs: 4,
            slurm_active_gpus: 0,
            slurm_max_gpus: 4,
            idle_jobs_executed: 0,
            idle_wall_seconds_used: 0.0,
            idle_gpu_seconds_used: 0.0,
        }
    }

    #[test]
    fn idle_scheduler_yields_to_foreground_heavy_work_and_prefers_human_queue() {
        let sched = IdleVerificationScheduler::new(IdleAdmissionPolicy::default());
        let candidates = vec![
            IdleCandidate {
                ticket_id: "voi-high".into(),
                is_human_queued: false,
                priority_score: 0.85,
                reproducibility_class: ReproducibilityClass::ReplicableByProtocol,
                target_kind: ExecutionTargetKind::LocalSandbox,
                requires_gpu: true,
            },
            IdleCandidate {
                ticket_id: "human-1".into(),
                is_human_queued: true,
                priority_score: 0.40,
                reproducibility_class: ReproducibilityClass::FormalizableInItp,
                target_kind: ExecutionTargetKind::LocalSandbox,
                requires_gpu: false,
            },
        ];

        // 1. Foreground Heavy active -> DEFER!
        let mut busy = idle_snapshot();
        busy.broker_load.heavy_running = 1;
        assert!(matches!(
            sched.select_next(&busy, &candidates),
            IdleDispatchDecision::Defer { .. }
        ));

        // 2. Host memory Yellow/Red -> DEFER!
        let mut mem_hot = idle_snapshot();
        mem_hot.host_memory_green = false;
        assert!(matches!(
            sched.select_next(&mem_hot, &candidates),
            IdleDispatchDecision::Defer { .. }
        ));

        // 3. Idle host -> Human queue preempts BP VoI by default!
        let admitted = sched.select_next(&idle_snapshot(), &candidates);
        assert!(matches!(
            admitted,
            IdleDispatchDecision::Admit { ref ticket_id, .. } if ticket_id == "human-1"
        ));

        // 4. Independent wall-second budget exhausted -> DEFER even when 100% idle!
        let mut over_budget = idle_snapshot();
        over_budget.idle_wall_seconds_used = 9.0 * 3600.0;
        assert!(matches!(
            sched.select_next(&over_budget, &candidates),
            IdleDispatchDecision::Defer { .. }
        ));
    }
}
