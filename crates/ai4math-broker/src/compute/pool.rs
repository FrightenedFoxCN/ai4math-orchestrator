//! Multi-dimensional local host resource pool (CPU cores, host RAM, scratch
//! disk, and per-card GPU VRAM / exclusive locks with Apple Silicon unified-
//! memory coupling).
//!
//! Coexists alongside the legacy ITP `Resources` fast/heavy admission pool
//! without altering any existing Lean/Agda/Rocq scheduling behavior.

use ai4math_core::{ComputePoolSnapshot, GpuDeviceSnapshot, GpuKind, ResourceRequest};
use anyhow::{Context, Result, bail, ensure};
use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex},
};

/// Static hardware description of one local GPU device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuDeviceSpec {
    pub index: u32,
    pub kind: GpuKind,
    pub total_vram_mib: u64,
    /// True for unified-memory architectures (e.g. Apple Silicon Metal) where
    /// GPU VRAM reservations must also deduct from host RAM capacity.
    pub unified_memory: bool,
}

/// Pluggable local GPU topology discovery source.
pub trait GpuInventoryProvider: Send + Sync {
    fn probe(&self) -> Result<Vec<GpuDeviceSpec>>;
}

/// Operator-pinned static GPU inventory (zero external subprocess dependency).
#[derive(Debug, Clone)]
pub struct StaticGpuInventory {
    devices: Vec<GpuDeviceSpec>,
}

impl StaticGpuInventory {
    pub fn new(devices: Vec<GpuDeviceSpec>) -> Result<Self> {
        let mut seen = std::collections::BTreeSet::new();
        for d in &devices {
            ensure!(d.total_vram_mib > 0, "GPU total_vram_mib must be positive");
            ensure!(
                d.kind != GpuKind::Any,
                "physical GPU device kind must be concrete (cuda, rocm, or metal)"
            );
            ensure!(
                seen.insert(d.index),
                "duplicate GPU device index {}",
                d.index
            );
        }
        Ok(Self { devices })
    }
}

impl GpuInventoryProvider for StaticGpuInventory {
    fn probe(&self) -> Result<Vec<GpuDeviceSpec>> {
        Ok(self.devices.clone())
    }
}

/// Apple Silicon unified-memory GPU inventory provider.
#[derive(Debug, Clone)]
pub struct AppleUnifiedGpuInventory {
    pub gpu_budget_mib: u64,
}

impl GpuInventoryProvider for AppleUnifiedGpuInventory {
    fn probe(&self) -> Result<Vec<GpuDeviceSpec>> {
        ensure!(
            self.gpu_budget_mib > 0,
            "Apple unified GPU budget must be positive"
        );
        Ok(vec![GpuDeviceSpec {
            index: 0,
            kind: GpuKind::Metal,
            total_vram_mib: self.gpu_budget_mib,
            unified_memory: true,
        }])
    }
}

/// Linux / CUDA live GPU topology discovery using an operator-pinned `nvidia-smi` binary.
#[derive(Debug, Clone)]
pub struct NvidiaSmiInventory {
    binary: PathBuf,
}

impl NvidiaSmiInventory {
    pub fn new(binary: impl AsRef<Path>) -> Result<Self> {
        let path = binary.as_ref();
        ensure!(path.is_absolute(), "nvidia-smi path must be absolute");
        Ok(Self {
            binary: path.to_path_buf(),
        })
    }

    pub fn parse_csv(output: &str) -> Result<Vec<GpuDeviceSpec>> {
        let mut devices = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        for line in output.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let mut cols = trimmed.split(',');
            let idx_str = cols
                .next()
                .context("missing GPU index in nvidia-smi output")?
                .trim();
            let vram_str = cols
                .next()
                .context("missing GPU memory.total in nvidia-smi output")?
                .trim();
            ensure!(
                cols.next().is_none(),
                "unexpected extra column in nvidia-smi output"
            );
            let index: u32 = idx_str
                .parse()
                .context("invalid GPU index in nvidia-smi output")?;
            let total_vram_mib: u64 = vram_str
                .parse()
                .context("invalid GPU memory MiB in nvidia-smi output")?;
            ensure!(total_vram_mib > 0, "GPU VRAM must be positive");
            ensure!(seen.insert(index), "duplicate GPU index {index}");
            devices.push(GpuDeviceSpec {
                index,
                kind: GpuKind::Cuda,
                total_vram_mib,
                unified_memory: false,
            });
        }
        Ok(devices)
    }
}

impl GpuInventoryProvider for NvidiaSmiInventory {
    fn probe(&self) -> Result<Vec<GpuDeviceSpec>> {
        let out = Command::new(&self.binary)
            .env_clear()
            .args([
                "--query-gpu=index,memory.total",
                "--format=csv,noheader,nounits",
            ])
            .output()
            .with_context(|| format!("run {}", self.binary.display()))?;
        ensure!(out.status.success(), "nvidia-smi exited with failure");
        let text = String::from_utf8(out.stdout).context("non-UTF8 nvidia-smi output")?;
        Self::parse_csv(&text)
    }
}

/// Host capacity configuration for `LocalComputePool`.
#[derive(Debug, Clone)]
pub struct LocalComputeCapacity {
    pub total_cpu_cores: u16,
    pub total_memory_mib: u64,
    pub total_scratch_mib: u64,
    pub gpus: Vec<GpuDeviceSpec>,
}

impl LocalComputeCapacity {
    pub fn from_provider(
        total_cpu_cores: u16,
        total_memory_mib: u64,
        total_scratch_mib: u64,
        provider: &dyn GpuInventoryProvider,
    ) -> Result<Self> {
        let gpus = provider.probe()?;
        Ok(Self {
            total_cpu_cores,
            total_memory_mib,
            total_scratch_mib,
            gpus,
        })
    }
}

#[derive(Debug)]
struct GpuSlotState {
    spec: GpuDeviceSpec,
    free_vram_mib: u64,
    exclusive_busy: bool,
    shared_allocations: u32,
}

#[derive(Debug)]
struct PoolState {
    total_cpu_cores: u16,
    used_cpu_cores: u16,
    total_memory_mib: u64,
    used_memory_mib: u64,
    total_scratch_mib: u64,
    used_scratch_mib: u64,
    gpus: Vec<GpuSlotState>,
    active_leases: u32,
}

#[derive(Debug, Clone)]
struct GpuAllocationRecord {
    slot_pos: usize,
    device_index: u32,
    kind: GpuKind,
    reserved_vram_mib: u64,
    exclusive: bool,
    unified_memory_charge_mib: u64,
}

/// Thread-safe, atomic local multi-dimensional resource allocator.
#[derive(Debug)]
pub struct LocalComputePool {
    state: Mutex<PoolState>,
}

impl LocalComputePool {
    pub fn new(capacity: LocalComputeCapacity) -> Result<Arc<Self>> {
        ensure!(
            capacity.total_cpu_cores > 0,
            "total_cpu_cores must be positive"
        );
        ensure!(
            capacity.total_memory_mib > 0,
            "total_memory_mib must be positive"
        );
        let validated = StaticGpuInventory::new(capacity.gpus)?;
        let mut gpus: Vec<GpuSlotState> = validated
            .devices
            .into_iter()
            .map(|spec| {
                let free_vram_mib = spec.total_vram_mib;
                GpuSlotState {
                    spec,
                    free_vram_mib,
                    exclusive_busy: false,
                    shared_allocations: 0,
                }
            })
            .collect();
        gpus.sort_by_key(|g| g.spec.index);

        Ok(Arc::new(Self {
            state: Mutex::new(PoolState {
                total_cpu_cores: capacity.total_cpu_cores,
                used_cpu_cores: 0,
                total_memory_mib: capacity.total_memory_mib,
                used_memory_mib: 0,
                total_scratch_mib: capacity.total_scratch_mib,
                used_scratch_mib: 0,
                gpus,
                active_leases: 0,
            }),
        }))
    }

    /// Atomically reserve CPU cores, host RAM, scratch disk, and matching GPU
    /// device(s). Returns an RAII `ComputeLease` that restores all reservations
    /// on drop. Fails without mutating pool state if any dimension cannot be met.
    pub fn try_acquire(self: &Arc<Self>, req: &ResourceRequest) -> Result<ComputeLease> {
        req.validate()
            .map_err(|e| anyhow::anyhow!("invalid resource request: {e}"))?;

        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("compute pool lock poisoned"))?;

        let avail_cpu = state.total_cpu_cores.saturating_sub(state.used_cpu_cores);
        ensure!(
            req.cpu_cores <= avail_cpu,
            "insufficient CPU cores: requested {}, available {}",
            req.cpu_cores,
            avail_cpu
        );
        let avail_scratch = state
            .total_scratch_mib
            .saturating_sub(state.used_scratch_mib);
        ensure!(
            req.scratch_mib <= avail_scratch,
            "insufficient scratch space: requested {} MiB, available {} MiB",
            req.scratch_mib,
            avail_scratch
        );

        let mut gpu_allocs = Vec::new();
        let mut unified_ram_extra = 0u64;

        if let Some(gpu_req) = &req.gpu {
            let needed = usize::from(gpu_req.count);
            for (pos, slot) in state.gpus.iter().enumerate() {
                if gpu_allocs.len() == needed {
                    break;
                }
                if !gpu_req.kind.matches_device(slot.spec.kind) {
                    continue;
                }
                let eligible = if gpu_req.exclusive {
                    !slot.exclusive_busy
                        && slot.shared_allocations == 0
                        && slot.free_vram_mib >= gpu_req.min_vram_mib
                } else {
                    !slot.exclusive_busy && slot.free_vram_mib >= gpu_req.min_vram_mib
                };
                if !eligible {
                    continue;
                }
                let reserved_vram_mib = if gpu_req.exclusive {
                    slot.spec.total_vram_mib
                } else {
                    gpu_req.min_vram_mib
                };
                let unified_charge = if slot.spec.unified_memory {
                    gpu_req.min_vram_mib
                } else {
                    0
                };
                unified_ram_extra = unified_ram_extra
                    .checked_add(unified_charge)
                    .context("unified memory charge overflow")?;
                gpu_allocs.push(GpuAllocationRecord {
                    slot_pos: pos,
                    device_index: slot.spec.index,
                    kind: slot.spec.kind,
                    reserved_vram_mib,
                    exclusive: gpu_req.exclusive,
                    unified_memory_charge_mib: unified_charge,
                });
            }
            if gpu_allocs.len() < needed {
                bail!(
                    "insufficient {} GPUs: requested {} x {} MiB (exclusive={}), found {} eligible",
                    gpu_req.kind,
                    needed,
                    gpu_req.min_vram_mib,
                    gpu_req.exclusive,
                    gpu_allocs.len()
                );
            }
        }

        let total_ram_needed = req
            .memory_mib
            .checked_add(unified_ram_extra)
            .context("total memory request overflow")?;
        let avail_ram = state.total_memory_mib.saturating_sub(state.used_memory_mib);
        ensure!(
            total_ram_needed <= avail_ram,
            "insufficient host RAM (including {} MiB unified GPU memory): requested {} MiB, available {} MiB",
            unified_ram_extra,
            total_ram_needed,
            avail_ram
        );

        // Commit all dimensions atomically.
        state.used_cpu_cores += req.cpu_cores;
        state.used_memory_mib += total_ram_needed;
        state.used_scratch_mib += req.scratch_mib;
        state.active_leases = state.active_leases.saturating_add(1);
        for alloc in &gpu_allocs {
            let slot = &mut state.gpus[alloc.slot_pos];
            if alloc.exclusive {
                slot.exclusive_busy = true;
                slot.free_vram_mib = 0;
            } else {
                slot.shared_allocations += 1;
                slot.free_vram_mib -= alloc.reserved_vram_mib;
            }
        }
        let host_has_cuda = state.gpus.iter().any(|g| g.spec.kind == GpuKind::Cuda);
        let host_has_rocm = state.gpus.iter().any(|g| g.spec.kind == GpuKind::Rocm);

        Ok(ComputeLease {
            pool: Arc::clone(self),
            cpu_cores: req.cpu_cores,
            memory_mib: total_ram_needed,
            scratch_mib: req.scratch_mib,
            gpu_allocs,
            host_has_cuda,
            host_has_rocm,
        })
    }

    pub fn snapshot(&self) -> Result<ComputePoolSnapshot> {
        let state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("compute pool lock poisoned"))?;
        Ok(ComputePoolSnapshot {
            total_cpu_cores: state.total_cpu_cores,
            used_cpu_cores: state.used_cpu_cores,
            total_memory_mib: state.total_memory_mib,
            used_memory_mib: state.used_memory_mib,
            total_scratch_mib: state.total_scratch_mib,
            used_scratch_mib: state.used_scratch_mib,
            gpus: state
                .gpus
                .iter()
                .map(|g| GpuDeviceSnapshot {
                    index: g.spec.index,
                    kind: g.spec.kind,
                    total_vram_mib: g.spec.total_vram_mib,
                    free_vram_mib: g.free_vram_mib,
                    unified_memory: g.spec.unified_memory,
                    exclusive_busy: g.exclusive_busy,
                    shared_allocations: g.shared_allocations,
                })
                .collect(),
            active_leases: state.active_leases,
        })
    }
}

/// RAII multi-dimensional resource lease issued by `LocalComputePool`.
///
/// Carries concrete allocated GPU device indices and generates mandatory
/// device-visibility environment variables so sandboxed jobs cannot hijack
/// unallocated GPUs.
#[derive(Debug)]
#[must_use = "dropping a ComputeLease immediately releases its CPU/RAM/GPU reservation"]
pub struct ComputeLease {
    pool: Arc<LocalComputePool>,
    cpu_cores: u16,
    memory_mib: u64,
    scratch_mib: u64,
    gpu_allocs: Vec<GpuAllocationRecord>,
    host_has_cuda: bool,
    host_has_rocm: bool,
}

impl ComputeLease {
    pub fn cpu_cores(&self) -> u16 {
        self.cpu_cores
    }

    pub fn reserved_memory_mib(&self) -> u64 {
        self.memory_mib
    }

    pub fn reserved_scratch_mib(&self) -> u64 {
        self.scratch_mib
    }

    pub fn gpu_indices(&self) -> Vec<u32> {
        self.gpu_allocs.iter().map(|a| a.device_index).collect()
    }

    /// Generate mandatory GPU isolation environment variables for sandboxed child
    /// processes. When no GPU was requested on a GPU-equipped host, explicitly
    /// sets `CUDA_VISIBLE_DEVICES=""` / `ROCR_VISIBLE_DEVICES=""` to block
    /// accidental GPU hijacking.
    pub fn device_env_vars(&self) -> Vec<(&'static str, String)> {
        let mut envs = Vec::new();
        let cuda: Vec<String> = self
            .gpu_allocs
            .iter()
            .filter(|a| a.kind == GpuKind::Cuda)
            .map(|a| a.device_index.to_string())
            .collect();
        let rocm: Vec<String> = self
            .gpu_allocs
            .iter()
            .filter(|a| a.kind == GpuKind::Rocm)
            .map(|a| a.device_index.to_string())
            .collect();

        if !cuda.is_empty() {
            let joined = cuda.join(",");
            envs.push(("CUDA_VISIBLE_DEVICES", joined.clone()));
            envs.push(("NVIDIA_VISIBLE_DEVICES", joined));
        } else if self.host_has_cuda {
            envs.push(("CUDA_VISIBLE_DEVICES", String::new()));
            envs.push(("NVIDIA_VISIBLE_DEVICES", "none".to_owned()));
        }

        if !rocm.is_empty() {
            let joined = rocm.join(",");
            envs.push(("ROCR_VISIBLE_DEVICES", joined.clone()));
            envs.push(("HIP_VISIBLE_DEVICES", joined));
        } else if self.host_has_rocm {
            envs.push(("ROCR_VISIBLE_DEVICES", String::new()));
            envs.push(("HIP_VISIBLE_DEVICES", String::new()));
        }

        envs.push(("OMP_NUM_THREADS", self.cpu_cores.to_string()));
        envs
    }
}

impl Drop for ComputeLease {
    fn drop(&mut self) {
        let mut state = self
            .pool
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.used_cpu_cores = state.used_cpu_cores.saturating_sub(self.cpu_cores);
        state.used_memory_mib = state.used_memory_mib.saturating_sub(self.memory_mib);
        state.used_scratch_mib = state.used_scratch_mib.saturating_sub(self.scratch_mib);
        state.active_leases = state.active_leases.saturating_sub(1);
        for alloc in &self.gpu_allocs {
            let slot = &mut state.gpus[alloc.slot_pos];
            if alloc.exclusive {
                slot.exclusive_busy = false;
                slot.free_vram_mib = slot.spec.total_vram_mib;
            } else {
                slot.shared_allocations = slot.shared_allocations.saturating_sub(1);
                slot.free_vram_mib = slot
                    .free_vram_mib
                    .saturating_add(alloc.reserved_vram_mib)
                    .min(slot.spec.total_vram_mib);
            }
            let _ = alloc.unified_memory_charge_mib;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ai4math_core::GpuRequest;

    #[test]
    fn gpu_pool_allocates_exclusive_shared_and_masks_cpu_only_jobs() -> Result<()> {
        let pool = LocalComputePool::new(LocalComputeCapacity {
            total_cpu_cores: 32,
            total_memory_mib: 128 * 1024,
            total_scratch_mib: 500 * 1024,
            gpus: vec![
                GpuDeviceSpec {
                    index: 0,
                    kind: GpuKind::Cuda,
                    total_vram_mib: 24_576,
                    unified_memory: false,
                },
                GpuDeviceSpec {
                    index: 1,
                    kind: GpuKind::Cuda,
                    total_vram_mib: 24_576,
                    unified_memory: false,
                },
            ],
        })?;

        // 1. Exclusive GPU lease locks card 0 completely.
        let excl = pool.try_acquire(&ResourceRequest {
            cpu_cores: 8,
            memory_mib: 16_384,
            gpu: Some(GpuRequest {
                count: 1,
                min_vram_mib: 16_000,
                kind: GpuKind::Cuda,
                exclusive: true,
            }),
            scratch_mib: 10_000,
            max_wall_seconds: 600,
        })?;
        assert_eq!(excl.gpu_indices(), vec![0]);
        let envs = excl.device_env_vars();
        assert!(envs.contains(&("CUDA_VISIBLE_DEVICES", "0".to_owned())));

        // 2. Two shared VRAM leases fit onto card 1 (10 GiB + 10 GiB <= 24 GiB).
        let shared_req = ResourceRequest {
            cpu_cores: 4,
            memory_mib: 8_192,
            gpu: Some(GpuRequest {
                count: 1,
                min_vram_mib: 10_240,
                kind: GpuKind::Cuda,
                exclusive: false,
            }),
            scratch_mib: 0,
            max_wall_seconds: 300,
        };
        let s1 = pool.try_acquire(&shared_req)?;
        let s2 = pool.try_acquire(&shared_req)?;
        assert_eq!(s1.gpu_indices(), vec![1]);
        assert_eq!(s2.gpu_indices(), vec![1]);

        // 3. Third 10 GiB shared lease fails atomically (card 1 only has 4 GiB left).
        assert!(pool.try_acquire(&shared_req).is_err());

        // 4. CPU-only job explicitly blanks CUDA_VISIBLE_DEVICES so it cannot touch GPUs!
        let cpu_job = pool.try_acquire(&ResourceRequest::cpu_only(4, 4_096, 60))?;
        assert!(cpu_job.gpu_indices().is_empty());
        assert!(
            cpu_job
                .device_env_vars()
                .contains(&("CUDA_VISIBLE_DEVICES", String::new()))
        );

        // 5. Dropping all leases restores full capacity.
        drop((excl, s1, s2, cpu_job));
        let snap = pool.snapshot()?;
        assert_eq!(snap.used_cpu_cores, 0);
        assert_eq!(snap.used_memory_mib, 0);
        assert_eq!(snap.active_leases, 0);
        assert!(
            snap.gpus
                .iter()
                .all(|g| g.free_vram_mib == 24_576 && !g.exclusive_busy)
        );
        Ok(())
    }

    #[test]
    fn apple_unified_memory_deducts_vram_from_host_ram_budget() -> Result<()> {
        let cap = LocalComputeCapacity::from_provider(
            12,
            32_768,
            100_000,
            &AppleUnifiedGpuInventory {
                gpu_budget_mib: 24_576,
            },
        )?;
        let pool = LocalComputePool::new(cap)?;

        // Request 16 GiB RAM + 20 GiB Metal VRAM = 36 GiB > 32 GiB unified host RAM -> rejected!
        assert!(
            pool.try_acquire(&ResourceRequest {
                cpu_cores: 4,
                memory_mib: 16_384,
                gpu: Some(GpuRequest {
                    count: 1,
                    min_vram_mib: 20_480,
                    kind: GpuKind::Metal,
                    exclusive: false,
                }),
                scratch_mib: 0,
                max_wall_seconds: 300,
            })
            .is_err()
        );

        // Request 12 GiB RAM + 16 GiB Metal VRAM = 28 GiB <= 32 GiB -> succeeds and deducts 28 GiB!
        let lease = pool.try_acquire(&ResourceRequest {
            cpu_cores: 4,
            memory_mib: 12_288,
            gpu: Some(GpuRequest {
                count: 1,
                min_vram_mib: 16_384,
                kind: GpuKind::Metal,
                exclusive: false,
            }),
            scratch_mib: 0,
            max_wall_seconds: 300,
        })?;
        assert_eq!(lease.reserved_memory_mib(), 28_672);
        assert_eq!(pool.snapshot()?.used_memory_mib, 28_672);
        drop(lease);
        assert_eq!(pool.snapshot()?.used_memory_mib, 0);
        Ok(())
    }

    #[test]
    fn nvidia_smi_csv_parser_validates_strict_format() -> Result<()> {
        let parsed = NvidiaSmiInventory::parse_csv("0, 40960\n1, 81920\n")?;
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].index, 0);
        assert_eq!(parsed[0].total_vram_mib, 40_960);
        assert_eq!(parsed[1].index, 1);
        assert_eq!(parsed[1].total_vram_mib, 81_920);
        assert!(NvidiaSmiInventory::parse_csv("0, 0\n").is_err());
        assert!(NvidiaSmiInventory::parse_csv("0, 1024\n0, 1024\n").is_err());
        assert!(NvidiaSmiInventory::parse_csv("bad,line,extra\n").is_err());
        Ok(())
    }
}
