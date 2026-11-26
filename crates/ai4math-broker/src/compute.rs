//! Multi-dimensional local resource scheduling (CPU / RAM / per-card GPU /
//! unified memory) and unified local / SLURM compute execution backends.

pub mod backend;
pub mod pool;
pub mod slurm;

pub use backend::{
    ComputeBackend, ComputeJobPoll, LocalComputeBackend, LocalTaskExecutor, SubmittedComputeJob,
    TaskExecutionObservation,
};
pub use pool::{
    AppleUnifiedGpuInventory, ComputeLease, GpuDeviceSpec, GpuInventoryProvider,
    LocalComputeCapacity, LocalComputePool, NvidiaSmiInventory, StaticGpuInventory,
};
pub use slurm::{
    LocalSlurmCli, SlurmComputeBackend, SlurmContainerRuntime, SlurmExecutionProfile,
    SlurmTransport, SshSlurmConfig, SshSlurmTransport, build_slurm_flags, parse_sacct_output,
    parse_squeue_output, render_sbatch_script,
};
