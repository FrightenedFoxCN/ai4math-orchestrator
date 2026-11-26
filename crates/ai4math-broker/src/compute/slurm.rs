//! Unified SLURM execution backend (`SlurmComputeBackend<T>`) and narrow,
//! non-shell `SlurmTransport` trait implemented by both `LocalSlurmCli`
//! (login-node direct CLI) and `SshSlurmTransport` (pinned BatchMode OpenSSH).

use super::backend::{
    ComputeBackend, ComputeJobPoll, SubmittedComputeJob, TaskExecutionObservation,
};
use ai4math_core::{ComputeJobStatus, ExecutionTargetKind, SlurmClusterPolicy};
use ai4math_evidence::{
    FetchedJobOutputs, SlurmFlag, SlurmJobId, SlurmJobObservation, SlurmJobState, StagedJobBundle,
    is_safe_compute_token, is_safe_relative_bundle_path, is_safe_remote_abs_path,
};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Mutex,
};

/// Narrow, non-shell SLURM cluster transport seam shared by local login-node
/// CLI execution, remote BatchMode OpenSSH, and deterministic test doubles.
///
/// Deliberately omits any arbitrary shell execution method so untrusted data
/// cannot break out into shell commands.
pub trait SlurmTransport: Send + Sync {
    /// Atomically stage Broker-rendered immutable job files into an isolated
    /// `0700` directory on the cluster and return its opaque staging reference.
    fn stage_bundle(&self, job_dir_name: &str, files: &BTreeMap<String, String>) -> Result<String>;

    /// Invoke `sbatch --parsable` on the staged batch script with typed flags.
    fn submit_batch(
        &self,
        staging_ref: &str,
        script_rel_path: &str,
        flags: &[SlurmFlag],
    ) -> Result<SlurmJobId>;

    /// Batch-query job/array statuses (`sacct` / `squeue`) in one transport call.
    fn query_status(&self, job_ids: &[SlurmJobId]) -> Result<Vec<SlurmJobObservation>>;

    /// Fetch ONLY allowlisted, size-bounded regular output files back to broker.
    fn fetch_outputs(
        &self,
        staging_ref: &str,
        allowlist: &[String],
        max_total_bytes: u64,
    ) -> Result<FetchedJobOutputs>;

    /// Cancel one or more validated numeric SLURM job IDs via `scancel`.
    fn cancel_jobs(&self, job_ids: &[SlurmJobId]) -> Result<()>;
}

/// Container or pinned-binary runtime launched inside the Broker-rendered
/// `job.sbatch` script.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "runtime", rename_all = "snake_case")]
pub enum SlurmContainerRuntime {
    /// Immutable Apptainer/Singularity `.sif` image verified by SHA-256 on node.
    Apptainer {
        sif_remote_path: String,
        expected_sif_sha256: String,
    },
    /// Pinned native cluster binary (permitted only when `require_container_digest == false`).
    PinnedBinary { binary_remote_path: String },
}

/// Operator-configured SLURM submission profile bound to a cluster policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SlurmExecutionProfile {
    pub partition: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qos: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub constraint: Option<String>,
    /// Optional SLURM GRES GPU sub-type (e.g. `"a100"`, `"h100"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu_gres_kind: Option<String>,
    pub runtime: SlurmContainerRuntime,
    /// Frozen entrypoint arguments executed inside the container/binary.
    pub entrypoint_args: Vec<String>,
}

fn is_safe_argv_token(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 256
        && s.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'/' | b':' | b'=')
        })
}

fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

impl SlurmExecutionProfile {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            is_safe_compute_token(&self.partition, 64),
            "invalid SLURM partition {:?}",
            self.partition
        );
        if let Some(acc) = &self.account {
            ensure!(is_safe_compute_token(acc, 64), "invalid SLURM account");
        }
        if let Some(qos) = &self.qos {
            ensure!(is_safe_compute_token(qos, 64), "invalid SLURM qos");
        }
        if let Some(c) = &self.constraint {
            ensure!(is_safe_compute_token(c, 64), "invalid SLURM constraint");
        }
        if let Some(kind) = &self.gpu_gres_kind {
            ensure!(
                !kind.is_empty()
                    && kind.len() <= 32
                    && kind
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-')),
                "invalid SLURM gpu_gres_kind"
            );
        }
        match &self.runtime {
            SlurmContainerRuntime::Apptainer {
                sif_remote_path,
                expected_sif_sha256,
            } => {
                ensure!(
                    is_safe_remote_abs_path(sif_remote_path),
                    "unsafe Apptainer .sif remote path"
                );
                ensure!(
                    is_sha256_hex(expected_sif_sha256),
                    "Apptainer expected_sif_sha256 must be lowercase 64-hex"
                );
            }
            SlurmContainerRuntime::PinnedBinary { binary_remote_path } => {
                ensure!(
                    is_safe_remote_abs_path(binary_remote_path),
                    "unsafe pinned binary remote path"
                );
            }
        }
        ensure!(
            !self.entrypoint_args.is_empty() && self.entrypoint_args.len() <= 32,
            "entrypoint_args must contain 1..=32 validated tokens"
        );
        for arg in &self.entrypoint_args {
            ensure!(
                is_safe_argv_token(arg),
                "unsafe SLURM entrypoint token {arg:?}"
            );
        }
        Ok(())
    }

    pub fn container_digest(&self) -> Option<&str> {
        match &self.runtime {
            SlurmContainerRuntime::Apptainer {
                expected_sif_sha256,
                ..
            } => Some(expected_sif_sha256.as_str()),
            SlurmContainerRuntime::PinnedBinary { .. } => None,
        }
    }
}

/// Build typed, validated `sbatch` flags from a frozen cluster profile + job bundle.
pub fn build_slurm_flags(
    profile: &SlurmExecutionProfile,
    bundle: &StagedJobBundle,
) -> Result<Vec<SlurmFlag>> {
    profile.validate()?;
    bundle.validate()?;
    let mut flags = vec![
        SlurmFlag::JobName(format!("a4m-{}", bundle.job_id).chars().take(64).collect()),
        SlurmFlag::Partition(profile.partition.clone()),
        SlurmFlag::CpusPerTask(bundle.resources.cpu_cores),
        SlurmFlag::MemMib(bundle.resources.memory_mib),
        SlurmFlag::TimeSeconds(bundle.resources.max_wall_seconds),
    ];
    if let Some(acc) = &profile.account {
        flags.push(SlurmFlag::Account(acc.clone()));
    }
    if let Some(qos) = &profile.qos {
        flags.push(SlurmFlag::Qos(qos.clone()));
    }
    if let Some(c) = &profile.constraint {
        flags.push(SlurmFlag::Constraint(c.clone()));
    }
    if let Some(gpu) = &bundle.resources.gpu {
        flags.push(SlurmFlag::GresGpu {
            kind: profile.gpu_gres_kind.clone(),
            count: gpu.count,
        });
    }
    if bundle.array_len > 1 {
        flags.push(SlurmFlag::ArrayRange {
            start: 0,
            end: bundle.array_len - 1,
        });
    }
    for flag in &flags {
        flag.validate()?;
    }
    Ok(flags)
}

/// Deterministically render a tamper-evident `job.sbatch` script from strictly
/// validated tokens (never interpolating raw subagent strings).
pub fn render_sbatch_script(
    profile: &SlurmExecutionProfile,
    bundle: &StagedJobBundle,
) -> Result<String> {
    profile.validate()?;
    bundle.validate()?;
    let flags = build_slurm_flags(profile, bundle)?;

    let mut script = String::from("#!/usr/bin/env bash\n");
    for flag in &flags {
        let arg = flag.to_sbatch_arg()?;
        script.push_str(&format!("#SBATCH {arg}\n"));
    }
    script.push_str("#SBATCH --output=slurm-%A_%a.out\n");
    script.push_str("#SBATCH --error=slurm-%A_%a.err\n\n");
    script.push_str("set -euo pipefail\n");
    script.push_str("umask 077\n");
    script.push_str("sha256sum -c manifest.sha256 >/dev/null\n");
    script.push_str("TASK_IDX=\"${SLURM_ARRAY_TASK_ID:-0}\"\n");
    script.push_str("TASK_DIR=\"tasks/${TASK_IDX}\"\n");
    script.push_str("test -d \"${TASK_DIR}\"\n");

    let quoted_args: Vec<String> = profile
        .entrypoint_args
        .iter()
        .map(|a| format!("'{a}'"))
        .collect();
    let args_line = quoted_args.join(" ");

    match &profile.runtime {
        SlurmContainerRuntime::Apptainer {
            sif_remote_path,
            expected_sif_sha256,
        } => {
            if let Some(bundle_sha) = &bundle.container_image_sha256 {
                ensure!(
                    bundle_sha == expected_sif_sha256,
                    "bundle container_image_sha256 differs from profile Apptainer digest"
                );
            }
            script.push_str(&format!(
                "printf '%s  %s\\n' '{expected_sif_sha256}' '{sif_remote_path}' | sha256sum -c - >/dev/null\n"
            ));
            let mut bind_args = Vec::new();
            bind_args.push("--bind \"$(pwd)/${TASK_DIR}:/work:rw\"".to_owned());
            for mount in &bundle.cluster_mounts {
                mount.validate()?;
                bind_args.push(format!(
                    "--bind '{}:{}:ro'",
                    mount.remote_ro_path, mount.container_mount_path
                ));
            }
            let nv = if bundle.resources.gpu.is_some() {
                " --nv"
            } else {
                ""
            };
            script.push_str(&format!(
                "exec apptainer exec --containall --no-home{nv} {} --pwd /work '{}' {args_line}\n",
                bind_args.join(" "),
                sif_remote_path
            ));
        }
        SlurmContainerRuntime::PinnedBinary { binary_remote_path } => {
            script.push_str("cd \"${TASK_DIR}\"\n");
            script.push_str(&format!("exec '{binary_remote_path}' {args_line}\n"));
        }
    }

    Ok(script)
}

/// Parse pipe-delimited `sacct -P -n -X -o JobID,State,ExitCode,ElapsedRaw,AllocTRES,NodeList`.
pub fn parse_sacct_output(stdout: &str) -> Result<Vec<SlurmJobObservation>> {
    let mut observations = Vec::new();
    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let cols: Vec<&str> = trimmed.split('|').collect();
        ensure!(
            cols.len() >= 4,
            "malformed sacct line (expected >=4 pipe columns): {trimmed:?}"
        );
        let raw_job_col = cols[0].trim();
        // Skip substep rows like 12345.batch or 12345_0.extern if -X was ignored.
        if raw_job_col.contains('.') {
            continue;
        }
        let (job_id, array_task_id) = match raw_job_col.split_once('_') {
            Some((base, idx_str)) => {
                // Skip unexpanded array range specs such as 12345_[0-3]
                if idx_str.starts_with('[') {
                    continue;
                }
                let idx: u32 = idx_str
                    .parse()
                    .with_context(|| format!("invalid SLURM array task id {idx_str:?}"))?;
                (SlurmJobId::new(base, None)?, Some(idx))
            }
            None => (SlurmJobId::new(raw_job_col, None)?, None),
        };
        let state = SlurmJobState::parse_slurm_token(cols[1]);
        let exit_code = cols[2]
            .trim()
            .split(':')
            .next()
            .filter(|s| !s.is_empty())
            .and_then(|s| s.parse::<i32>().ok());
        let elapsed_seconds = cols[3]
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|v| v.is_finite() && *v >= 0.0)
            .unwrap_or(0.0);
        let allocated_tres = cols
            .get(4)
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        let node_list = cols
            .get(5)
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(str::to_owned);

        observations.push(SlurmJobObservation {
            job_id,
            array_task_id,
            state,
            exit_code,
            elapsed_seconds,
            allocated_tres,
            node_list,
        });
    }
    Ok(observations)
}

/// Parse pipe-delimited `squeue -h -o "%i|%T|%N"` fallback for newly queued jobs.
pub fn parse_squeue_output(stdout: &str) -> Result<Vec<SlurmJobObservation>> {
    let mut observations = Vec::new();
    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let cols: Vec<&str> = trimmed.split('|').collect();
        ensure!(
            cols.len() >= 2,
            "malformed squeue line (expected >=2 pipe columns): {trimmed:?}"
        );
        let raw_job_col = cols[0].trim();
        let (job_id, array_task_id) = match raw_job_col.split_once('_') {
            Some((base, idx_str)) => {
                if idx_str.starts_with('[') {
                    (SlurmJobId::new(base, None)?, None)
                } else {
                    let idx: u32 = idx_str
                        .parse()
                        .with_context(|| format!("invalid squeue array task id {idx_str:?}"))?;
                    (SlurmJobId::new(base, None)?, Some(idx))
                }
            }
            None => (SlurmJobId::new(raw_job_col, None)?, None),
        };
        let state = SlurmJobState::parse_slurm_token(cols[1]);
        let node_list = cols
            .get(2)
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        observations.push(SlurmJobObservation {
            job_id,
            array_task_id,
            state,
            exit_code: None,
            elapsed_seconds: 0.0,
            allocated_tres: None,
            node_list,
        });
    }
    Ok(observations)
}

/// Pure-Rust POSIX ustar TAR packer for regular files (zero external crates).
pub(crate) fn ustar_pack(files: &BTreeMap<String, String>) -> Result<Vec<u8>> {
    let mut archive = Vec::new();
    for (path, content) in files {
        ensure!(
            is_safe_relative_bundle_path(path),
            "unsafe tar entry path {path:?}"
        );
        let bytes = content.as_bytes();
        let (prefix, name) = split_ustar_path(path)?;
        let mut header = [0u8; 512];
        header[..name.len()].copy_from_slice(name.as_bytes());
        write_octal(&mut header[100..108], 0o600)?;
        write_octal(&mut header[108..116], 0)?;
        write_octal(&mut header[116..124], 0)?;
        write_octal(&mut header[124..136], bytes.len() as u64)?;
        write_octal(&mut header[136..148], 0)?;
        header[148..156].fill(b' ');
        header[156] = b'0'; // regular file
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");
        if !prefix.is_empty() {
            header[345..345 + prefix.len()].copy_from_slice(prefix.as_bytes());
        }
        let checksum: u32 = header.iter().map(|&b| u32::from(b)).sum();
        let chk = format!("{checksum:06o}\0 ");
        header[148..156].copy_from_slice(chk.as_bytes());

        archive.extend_from_slice(&header);
        archive.extend_from_slice(bytes);
        let rem = bytes.len() % 512;
        if rem != 0 {
            archive.resize(archive.len() + (512 - rem), 0);
        }
    }
    archive.resize(archive.len() + 1024, 0);
    Ok(archive)
}

/// Pure-Rust bounded POSIX ustar TAR extractor enforcing regular-file-only
/// typeflags, strict allowlist membership, and `max_total_bytes`.
pub(crate) fn ustar_unpack_allowlisted(
    archive: &[u8],
    allowlist: &[String],
    max_total_bytes: u64,
) -> Result<FetchedJobOutputs> {
    let allowed: BTreeSet<&str> = allowlist.iter().map(String::as_str).collect();
    let mut files = BTreeMap::new();
    let mut total_bytes = 0u64;
    let mut offset = 0usize;

    while offset + 512 <= archive.len() {
        let header = &archive[offset..offset + 512];
        offset += 512;
        if header.iter().all(|&b| b == 0) {
            break;
        }
        let stored_chk = parse_octal(&header[148..156])?;
        let computed_chk: u64 = header
            .iter()
            .enumerate()
            .map(|(i, &b)| u64::from(if (148..156).contains(&i) { b' ' } else { b }))
            .sum();
        ensure!(
            stored_chk == computed_chk,
            "corrupted tar header checksum in fetched outputs"
        );
        let typeflag = header[156];
        ensure!(
            typeflag == b'0' || typeflag == 0,
            "refusing non-regular tar entry (typeflag={typeflag}); symlinks/devices are forbidden"
        );
        let name = read_cstr(&header[0..100])?;
        let prefix = read_cstr(&header[345..500])?;
        let full_path = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        let normalized = full_path.strip_prefix("./").unwrap_or(&full_path);
        ensure!(
            is_safe_relative_bundle_path(normalized),
            "unsafe tar output path {normalized:?}"
        );
        ensure!(
            allowed.contains(normalized),
            "unexpected tar output {normalized:?} not in allowlist"
        );
        let size = parse_octal(&header[124..136])?;
        let size_usize = usize::try_from(size).context("tar entry size overflow")?;
        ensure!(
            offset
                .checked_add(size_usize)
                .is_some_and(|end| end <= archive.len()),
            "truncated tar archive entry"
        );
        total_bytes = total_bytes
            .checked_add(size)
            .context("total fetched bytes overflow")?;
        ensure!(
            total_bytes <= max_total_bytes,
            "fetched tar outputs exceed max_total_bytes cap ({max_total_bytes})"
        );
        files.insert(
            normalized.to_owned(),
            archive[offset..offset + size_usize].to_vec(),
        );
        let padded = size_usize.div_ceil(512) * 512;
        offset = offset
            .checked_add(padded)
            .context("tar padding offset overflow")?;
    }

    let fetched = FetchedJobOutputs { files, total_bytes };
    fetched.validate(allowlist, max_total_bytes)?;
    Ok(fetched)
}

fn split_ustar_path(path: &str) -> Result<(&str, &str)> {
    if path.len() <= 100 {
        return Ok(("", path));
    }
    if let Some((prefix, name)) = path.rsplit_once('/')
        && prefix.len() <= 155
        && name.len() <= 100
    {
        return Ok((prefix, name));
    }
    bail!("path {path:?} exceeds POSIX ustar header length limits");
}

fn write_octal(field: &mut [u8], value: u64) -> Result<()> {
    let width = field.len() - 1;
    let formatted = format!("{value:0width$o}");
    ensure!(formatted.len() <= width, "tar octal field overflow");
    field[..width].copy_from_slice(formatted.as_bytes());
    field[width] = 0;
    Ok(())
}

fn parse_octal(field: &[u8]) -> Result<u64> {
    let s = std::str::from_utf8(field)
        .context("non-UTF8 tar octal field")?
        .trim_matches(|c: char| c == '\0' || c == ' ');
    if s.is_empty() {
        return Ok(0);
    }
    u64::from_str_radix(s, 8).context("invalid tar octal field")
}

fn read_cstr(field: &[u8]) -> Result<String> {
    let end = field.iter().position(|&b| b == 0).unwrap_or(field.len());
    Ok(std::str::from_utf8(&field[..end])
        .context("non-UTF8 tar path")?
        .to_owned())
}

/// Local login-node SLURM CLI transport invoking pinned `sbatch`, `squeue`,
/// `sacct`, and `scancel` binaries directly with `env_clear()` and zero shell.
#[derive(Debug, Clone)]
pub struct LocalSlurmCli {
    sbatch: PathBuf,
    squeue: PathBuf,
    sacct: PathBuf,
    scancel: PathBuf,
    staging_root: PathBuf,
}

impl LocalSlurmCli {
    pub fn new(
        sbatch: impl AsRef<Path>,
        squeue: impl AsRef<Path>,
        sacct: impl AsRef<Path>,
        scancel: impl AsRef<Path>,
        staging_root: impl AsRef<Path>,
    ) -> Result<Self> {
        for (name, p) in [
            ("sbatch", sbatch.as_ref()),
            ("squeue", squeue.as_ref()),
            ("sacct", sacct.as_ref()),
            ("scancel", scancel.as_ref()),
        ] {
            ensure!(p.is_absolute(), "{name} path must be absolute");
        }
        fs::create_dir_all(staging_root.as_ref()).context("create local SLURM staging root")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(staging_root.as_ref(), fs::Permissions::from_mode(0o700))?;
        }
        Ok(Self {
            sbatch: sbatch.as_ref().to_path_buf(),
            squeue: squeue.as_ref().to_path_buf(),
            sacct: sacct.as_ref().to_path_buf(),
            scancel: scancel.as_ref().to_path_buf(),
            staging_root: staging_root.as_ref().canonicalize()?,
        })
    }
}

impl SlurmTransport for LocalSlurmCli {
    fn stage_bundle(&self, job_dir_name: &str, files: &BTreeMap<String, String>) -> Result<String> {
        ensure!(
            is_safe_compute_token(job_dir_name, 128),
            "invalid SLURM job_dir_name"
        );
        let dir = self.staging_root.join(job_dir_name);
        ensure!(!dir.exists(), "SLURM staging directory already exists");
        fs::create_dir(&dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
        }
        for (rel, content) in files {
            ensure!(is_safe_relative_bundle_path(rel), "unsafe staged path");
            let dest = dir.join(rel);
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&dest, content)?;
        }
        Ok(dir.canonicalize()?.to_string_lossy().into_owned())
    }

    fn submit_batch(
        &self,
        staging_ref: &str,
        script_rel_path: &str,
        flags: &[SlurmFlag],
    ) -> Result<SlurmJobId> {
        ensure!(
            is_safe_relative_bundle_path(script_rel_path),
            "unsafe script_rel_path"
        );
        let dir = Path::new(staging_ref).canonicalize()?;
        ensure!(
            dir.starts_with(&self.staging_root),
            "staging_ref escaped local SLURM staging root"
        );
        let mut cmd = Command::new(&self.sbatch);
        cmd.env_clear().current_dir(&dir).arg("--parsable");
        for flag in flags {
            cmd.arg(flag.to_sbatch_arg()?);
        }
        cmd.arg(script_rel_path);
        let out = cmd.output().context("execute local sbatch")?;
        ensure!(
            out.status.success(),
            "local sbatch failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        SlurmJobId::parse_parsable(&String::from_utf8(out.stdout)?)
    }

    fn query_status(&self, job_ids: &[SlurmJobId]) -> Result<Vec<SlurmJobObservation>> {
        if job_ids.is_empty() {
            return Ok(Vec::new());
        }
        let joined = job_ids
            .iter()
            .map(SlurmJobId::as_str)
            .collect::<Vec<_>>()
            .join(",");
        let out = Command::new(&self.sacct)
            .env_clear()
            .args([
                "-P",
                "-n",
                "-X",
                "-j",
                &joined,
                "-o",
                "JobID,State,ExitCode,ElapsedRaw,AllocTRES,NodeList",
            ])
            .output()
            .context("execute local sacct")?;
        ensure!(
            out.status.success(),
            "local sacct failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let mut obs = parse_sacct_output(&String::from_utf8(out.stdout)?)?;
        let seen: BTreeSet<String> = obs.iter().map(|o| o.job_id.as_str().to_owned()).collect();
        let missing: Vec<&str> = job_ids
            .iter()
            .map(SlurmJobId::as_str)
            .filter(|id| !seen.contains(*id))
            .collect();
        if !missing.is_empty() {
            let sq = Command::new(&self.squeue)
                .env_clear()
                .args(["-h", "-j", &missing.join(","), "-o", "%i|%T|%N"])
                .output()
                .context("execute local squeue fallback")?;
            if sq.status.success() {
                obs.extend(parse_squeue_output(&String::from_utf8(sq.stdout)?)?);
            }
        }
        Ok(obs)
    }

    fn fetch_outputs(
        &self,
        staging_ref: &str,
        allowlist: &[String],
        max_total_bytes: u64,
    ) -> Result<FetchedJobOutputs> {
        let dir = Path::new(staging_ref).canonicalize()?;
        ensure!(
            dir.starts_with(&self.staging_root),
            "staging_ref escaped local SLURM staging root"
        );
        let mut files = BTreeMap::new();
        let mut total_bytes = 0u64;
        for rel in allowlist {
            ensure!(is_safe_relative_bundle_path(rel), "unsafe allowlist path");
            let target = dir.join(rel);
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
            ensure!(meta.is_file(), "output {rel:?} is not a regular file");
            ensure!(
                target.canonicalize()?.starts_with(&dir),
                "output {rel:?} escaped job dir"
            );
            let remaining = max_total_bytes.saturating_sub(total_bytes);
            let mut buf = Vec::new();
            file.take(remaining.saturating_add(1))
                .read_to_end(&mut buf)?;
            ensure!(
                u64::try_from(buf.len())? <= remaining,
                "fetched outputs exceed max_total_bytes"
            );
            total_bytes += u64::try_from(buf.len())?;
            files.insert(rel.clone(), buf);
        }
        let fetched = FetchedJobOutputs { files, total_bytes };
        fetched.validate(allowlist, max_total_bytes)?;
        Ok(fetched)
    }

    fn cancel_jobs(&self, job_ids: &[SlurmJobId]) -> Result<()> {
        if job_ids.is_empty() {
            return Ok(());
        }
        let mut cmd = Command::new(&self.scancel);
        cmd.env_clear();
        for id in job_ids {
            cmd.arg(id.as_str());
        }
        let out = cmd.output().context("execute local scancel")?;
        ensure!(
            out.status.success(),
            "local scancel failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        Ok(())
    }
}

/// Operator-pinned SSH configuration for remote SLURM clusters.
///
/// Always forces `BatchMode=yes`, `StrictHostKeyChecking=yes`,
/// `IdentitiesOnly=yes`, and an explicit `UserKnownHostsFile`.
#[derive(Debug, Clone)]
pub struct SshSlurmConfig {
    pub ssh_binary: PathBuf,
    pub host: String,
    pub user: String,
    pub port: u16,
    pub identity_file: PathBuf,
    pub known_hosts_file: PathBuf,
    pub control_path: Option<PathBuf>,
    pub remote_staging_root: String,
    pub connect_timeout_secs: u16,
    pub remote_sbatch: String,
    pub remote_squeue: String,
    pub remote_sacct: String,
    pub remote_scancel: String,
}

impl SshSlurmConfig {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.ssh_binary.is_absolute(), "ssh_binary must be absolute");
        ensure!(
            self.identity_file.is_absolute(),
            "identity_file must be absolute"
        );
        ensure!(
            self.known_hosts_file.is_absolute(),
            "known_hosts_file must be absolute"
        );
        ensure!(
            !self.host.is_empty()
                && self.host.len() <= 253
                && !self.host.starts_with('-')
                && self
                    .host
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-')),
            "invalid SSH host {:?}",
            self.host
        );
        ensure!(
            !self.user.is_empty()
                && self.user.len() <= 64
                && !self.user.starts_with('-')
                && self
                    .user
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.')),
            "invalid SSH user {:?}",
            self.user
        );
        ensure!(self.port > 0, "SSH port must be positive");
        ensure!(
            self.connect_timeout_secs >= 1 && self.connect_timeout_secs <= 120,
            "SSH connect_timeout_secs must be in 1..=120"
        );
        ensure!(
            is_safe_remote_abs_path(&self.remote_staging_root),
            "unsafe remote_staging_root {:?}",
            self.remote_staging_root
        );
        for (label, bin) in [
            ("remote_sbatch", &self.remote_sbatch),
            ("remote_squeue", &self.remote_squeue),
            ("remote_sacct", &self.remote_sacct),
            ("remote_scancel", &self.remote_scancel),
        ] {
            ensure!(
                is_safe_remote_abs_path(bin) || is_safe_compute_token(bin, 64),
                "unsafe {label} binary {bin:?}"
            );
        }
        Ok(())
    }
}

/// Remote SLURM transport over pinned BatchMode OpenSSH using pure-Rust ustar
/// tar streaming for bundle staging and allowlisted output retrieval.
#[derive(Debug, Clone)]
pub struct SshSlurmTransport {
    config: SshSlurmConfig,
}

impl SshSlurmTransport {
    pub fn new(config: SshSlurmConfig) -> Result<Self> {
        config.validate()?;
        Ok(Self { config })
    }

    /// Construct the deterministic, non-interactive OpenSSH base argv.
    pub fn build_ssh_base_args(&self) -> Vec<String> {
        let mut args = vec![
            "-o".into(),
            "BatchMode=yes".into(),
            "-o".into(),
            "StrictHostKeyChecking=yes".into(),
            "-o".into(),
            "IdentitiesOnly=yes".into(),
            "-o".into(),
            format!("ConnectTimeout={}", self.config.connect_timeout_secs),
            "-o".into(),
            format!(
                "UserKnownHostsFile={}",
                self.config.known_hosts_file.display()
            ),
            "-i".into(),
            self.config.identity_file.display().to_string(),
            "-p".into(),
            self.config.port.to_string(),
        ];
        if let Some(ctrl) = &self.config.control_path {
            args.extend([
                "-o".into(),
                "ControlMaster=auto".into(),
                "-o".into(),
                "ControlPersist=60".into(),
                "-o".into(),
                format!("ControlPath={}", ctrl.display()),
            ]);
        }
        args.push("--".into());
        args.push(format!("{}@{}", self.config.user, self.config.host));
        args
    }

    fn validate_remote_job_dir(&self, staging_ref: &str) -> Result<()> {
        ensure!(
            is_safe_remote_abs_path(staging_ref),
            "unsafe remote staging_ref {staging_ref:?}"
        );
        let prefix = format!("{}/", self.config.remote_staging_root);
        ensure!(
            staging_ref.starts_with(&prefix),
            "remote staging_ref escaped remote_staging_root"
        );
        Ok(())
    }

    fn run_remote(&self, remote_cmd: &str, stdin_bytes: Option<&[u8]>) -> Result<Vec<u8>> {
        let mut cmd = Command::new(&self.config.ssh_binary);
        cmd.env_clear()
            .args(self.build_ssh_base_args())
            .arg(remote_cmd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if stdin_bytes.is_some() {
            cmd.stdin(Stdio::piped());
        } else {
            cmd.stdin(Stdio::null());
        }
        let mut child = cmd.spawn().context("spawn pinned ssh binary")?;
        if let Some(bytes) = stdin_bytes {
            let mut stdin = child.stdin.take().context("open ssh stdin")?;
            stdin.write_all(bytes)?;
            drop(stdin);
        }
        let out = child.wait_with_output()?;
        ensure!(
            out.status.success(),
            "remote ssh command failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        Ok(out.stdout)
    }
}

impl SlurmTransport for SshSlurmTransport {
    fn stage_bundle(&self, job_dir_name: &str, files: &BTreeMap<String, String>) -> Result<String> {
        ensure!(
            is_safe_compute_token(job_dir_name, 128),
            "invalid remote job_dir_name"
        );
        let tar_bytes = ustar_pack(files)?;
        let remote_dir = format!("{}/{}", self.config.remote_staging_root, job_dir_name);
        self.validate_remote_job_dir(&remote_dir)?;
        let cmd = format!(
            "mkdir -p '{remote_dir}' && chmod 0700 '{remote_dir}' && tar -xf - --no-same-owner --no-same-permissions -C '{remote_dir}'"
        );
        self.run_remote(&cmd, Some(&tar_bytes))?;
        Ok(remote_dir)
    }

    fn submit_batch(
        &self,
        staging_ref: &str,
        script_rel_path: &str,
        flags: &[SlurmFlag],
    ) -> Result<SlurmJobId> {
        self.validate_remote_job_dir(staging_ref)?;
        ensure!(
            is_safe_relative_bundle_path(script_rel_path),
            "unsafe script_rel_path"
        );
        let mut arg_parts = Vec::with_capacity(flags.len() + 2);
        arg_parts.push("'--parsable'".to_owned());
        for flag in flags {
            let arg = flag.to_sbatch_arg()?;
            arg_parts.push(format!("'{arg}'"));
        }
        arg_parts.push(format!("'{script_rel_path}'"));
        let cmd = format!(
            "cd '{staging_ref}' && '{}' {}",
            self.config.remote_sbatch,
            arg_parts.join(" ")
        );
        let stdout = self.run_remote(&cmd, None)?;
        SlurmJobId::parse_parsable(&String::from_utf8(stdout)?)
    }

    fn query_status(&self, job_ids: &[SlurmJobId]) -> Result<Vec<SlurmJobObservation>> {
        if job_ids.is_empty() {
            return Ok(Vec::new());
        }
        let joined = job_ids
            .iter()
            .map(SlurmJobId::as_str)
            .collect::<Vec<_>>()
            .join(",");
        let cmd = format!(
            "'{}' -P -n -X -j '{joined}' -o 'JobID,State,ExitCode,ElapsedRaw,AllocTRES,NodeList'",
            self.config.remote_sacct
        );
        let stdout = self.run_remote(&cmd, None)?;
        let mut obs = parse_sacct_output(&String::from_utf8(stdout)?)?;
        let seen: BTreeSet<String> = obs.iter().map(|o| o.job_id.as_str().to_owned()).collect();
        let missing: Vec<&str> = job_ids
            .iter()
            .map(SlurmJobId::as_str)
            .filter(|id| !seen.contains(*id))
            .collect();
        if !missing.is_empty() {
            let sq_cmd = format!(
                "'{}' -h -j '{}' -o '%i|%T|%N'",
                self.config.remote_squeue,
                missing.join(",")
            );
            if let Ok(sq_out) = self.run_remote(&sq_cmd, None)
                && let Ok(text) = String::from_utf8(sq_out)
            {
                obs.extend(parse_squeue_output(&text)?);
            }
        }
        Ok(obs)
    }

    fn fetch_outputs(
        &self,
        staging_ref: &str,
        allowlist: &[String],
        max_total_bytes: u64,
    ) -> Result<FetchedJobOutputs> {
        self.validate_remote_job_dir(staging_ref)?;
        ensure!(!allowlist.is_empty(), "empty output allowlist");
        let mut quoted = Vec::with_capacity(allowlist.len());
        for rel in allowlist {
            ensure!(is_safe_relative_bundle_path(rel), "unsafe allowlist path");
            quoted.push(format!("'{rel}'"));
        }
        let cmd = format!("tar -cf - -C '{staging_ref}' -- {}", quoted.join(" "));
        let mut child = Command::new(&self.config.ssh_binary)
            .env_clear()
            .args(self.build_ssh_base_args())
            .arg(&cmd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("spawn ssh tar fetch")?;
        let tar_cap = max_total_bytes
            .saturating_add((allowlist.len() as u64).saturating_mul(1024))
            .saturating_add(4096);
        let mut archive = Vec::new();
        {
            let stdout = child.stdout.take().context("read ssh tar stdout")?;
            stdout
                .take(tar_cap.saturating_add(1))
                .read_to_end(&mut archive)?;
        }
        let status = child.wait()?;
        ensure!(status.success(), "remote ssh tar fetch failed");
        ensure!(
            u64::try_from(archive.len())? <= tar_cap,
            "remote tar stream exceeded size cap"
        );
        ustar_unpack_allowlisted(&archive, allowlist, max_total_bytes)
    }

    fn cancel_jobs(&self, job_ids: &[SlurmJobId]) -> Result<()> {
        if job_ids.is_empty() {
            return Ok(());
        }
        let ids = job_ids
            .iter()
            .map(|id| format!("'{}'", id.as_str()))
            .collect::<Vec<_>>()
            .join(" ");
        let cmd = format!("'{}' {ids}", self.config.remote_scancel);
        let _ = self.run_remote(&cmd, None)?;
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct TrackedSlurmJob {
    array_len: u32,
    requested_total_gpus: u32,
    active: bool,
}

/// Unified SLURM compute backend parameterized over `SlurmTransport`.
///
/// Enforces `SlurmClusterPolicy` guardrails, renders tamper-evident `job.sbatch`
/// scripts with container SHA-256 verification and `manifest.sha256` checks,
/// tracks active job/GPU quota (with external reconciliation after restarts),
/// and aggregates array-task states conservatively.
pub struct SlurmComputeBackend<T: SlurmTransport> {
    policy: SlurmClusterPolicy,
    profile: SlurmExecutionProfile,
    transport: T,
    tracked: Mutex<HashMap<String, TrackedSlurmJob>>,
    reconciled_quota: Mutex<Option<(u32, u32)>>,
}

impl<T: SlurmTransport> SlurmComputeBackend<T> {
    pub fn new(
        policy: SlurmClusterPolicy,
        profile: SlurmExecutionProfile,
        transport: T,
    ) -> Result<Self> {
        policy
            .validate()
            .map_err(|e| anyhow::anyhow!("invalid SLURM cluster policy: {e}"))?;
        profile.validate()?;
        Ok(Self {
            policy,
            profile,
            transport,
            tracked: Mutex::new(HashMap::new()),
            reconciled_quota: Mutex::new(None),
        })
    }

    pub fn policy(&self) -> &SlurmClusterPolicy {
        &self.policy
    }

    pub fn profile(&self) -> &SlurmExecutionProfile {
        &self.profile
    }

    pub fn transport(&self) -> &T {
        &self.transport
    }

    /// Inject durable active `(jobs, gpus)` counts loaded from `ai4math-store`
    /// after daemon restart so cluster-wide caps hold across process lifetimes.
    pub fn set_reconciled_quota(&self, active_jobs: u32, active_gpus: u32) -> Result<()> {
        let mut guard = self
            .reconciled_quota
            .lock()
            .map_err(|_| anyhow::anyhow!("reconciled quota lock poisoned"))?;
        *guard = Some((active_jobs, active_gpus));
        Ok(())
    }

    /// Register a recovered in-flight job's array length after process restart
    /// so `poll_batch` enforces complete array coverage.
    pub fn register_recovered_job(
        &self,
        job_id: impl Into<String>,
        array_len: u32,
        requested_total_gpus: u32,
    ) -> Result<()> {
        let mut tracked = self
            .tracked
            .lock()
            .map_err(|_| anyhow::anyhow!("tracked SLURM jobs lock poisoned"))?;
        tracked.insert(
            job_id.into(),
            TrackedSlurmJob {
                array_len,
                requested_total_gpus,
                active: true,
            },
        );
        Ok(())
    }

    fn current_active_quota(&self) -> Result<(u32, u32)> {
        let reconciled = *self
            .reconciled_quota
            .lock()
            .map_err(|_| anyhow::anyhow!("reconciled quota lock poisoned"))?;
        let tracked = self
            .tracked
            .lock()
            .map_err(|_| anyhow::anyhow!("tracked SLURM jobs lock poisoned"))?;
        let local_jobs = tracked.values().filter(|j| j.active).count() as u32;
        let local_gpus: u32 = tracked
            .values()
            .filter(|j| j.active)
            .map(|j| j.requested_total_gpus)
            .sum();
        Ok(match reconciled {
            Some((rj, rg)) => (rj.max(local_jobs), rg.max(local_gpus)),
            None => (local_jobs, local_gpus),
        })
    }

    /// Batch-poll multiple SLURM jobs in a single `SlurmTransport::query_status`
    /// call to avoid hammering `slurmctld` / `slurmdbd`.
    pub fn poll_batch(
        &self,
        jobs: &[&SubmittedComputeJob],
    ) -> Result<BTreeMap<String, ComputeJobPoll>> {
        if jobs.is_empty() {
            return Ok(BTreeMap::new());
        }
        let mut id_to_job: HashMap<String, &SubmittedComputeJob> = HashMap::new();
        let mut slurm_ids = Vec::with_capacity(jobs.len());
        for job in jobs {
            let raw_id = job
                .external_job_id
                .as_deref()
                .with_context(|| format!("SLURM job {:?} missing external_job_id", job.job_id))?;
            let sid = SlurmJobId::new(raw_id, None)?;
            id_to_job.insert(sid.as_str().to_owned(), *job);
            slurm_ids.push(sid);
        }
        let all_obs = self.transport.query_status(&slurm_ids)?;
        let mut grouped: HashMap<String, Vec<SlurmJobObservation>> = HashMap::new();
        for obs in all_obs {
            grouped
                .entry(obs.job_id.as_str().to_owned())
                .or_default()
                .push(obs);
        }

        let mut results = BTreeMap::new();
        let mut tracked = self
            .tracked
            .lock()
            .map_err(|_| anyhow::anyhow!("tracked SLURM jobs lock poisoned"))?;
        for (raw_id, job) in id_to_job {
            let expected_len = tracked.get(&job.job_id).map_or(1, |rec| rec.array_len);
            let obs = grouped.remove(&raw_id).unwrap_or_default();
            let poll = aggregate_slurm_observations(&obs, expected_len);
            if poll.status.is_terminal()
                && let Some(rec) = tracked.get_mut(&job.job_id)
            {
                rec.active = false;
            }
            results.insert(job.job_id.clone(), poll);
        }
        Ok(results)
    }

    /// Batch-cancel multiple SLURM jobs (e.g. on circuit-breaker trip or ADR pivot).
    pub fn cancel_batch(&self, jobs: &[&SubmittedComputeJob]) -> Result<()> {
        if jobs.is_empty() {
            return Ok(());
        }
        let mut slurm_ids = Vec::with_capacity(jobs.len());
        for job in jobs {
            if let Some(raw_id) = job.external_job_id.as_deref() {
                slurm_ids.push(SlurmJobId::new(raw_id, None)?);
            }
        }
        self.transport.cancel_jobs(&slurm_ids)?;
        let mut tracked = self
            .tracked
            .lock()
            .map_err(|_| anyhow::anyhow!("tracked SLURM jobs lock poisoned"))?;
        for job in jobs {
            if let Some(rec) = tracked.get_mut(&job.job_id) {
                rec.active = false;
            }
        }
        Ok(())
    }
}

fn aggregate_slurm_observations(
    obs: &[SlurmJobObservation],
    expected_array_len: u32,
) -> ComputeJobPoll {
    if obs.is_empty() {
        return ComputeJobPoll {
            status: ComputeJobStatus::Queued,
            task_observations: Vec::new(),
            detail: Some("awaiting SLURM scheduler visibility".to_owned()),
        };
    }

    let mut by_task: BTreeMap<u32, TaskExecutionObservation> = BTreeMap::new();
    for item in obs {
        let idx = item.array_task_id.unwrap_or(0);
        let status = item.state.to_compute_status();
        let detail = match &item.state {
            SlurmJobState::Completed | SlurmJobState::Pending | SlurmJobState::Running => None,
            other => Some(format!("slurm_state={other:?}")),
        };
        by_task.insert(
            idx,
            TaskExecutionObservation {
                array_task_id: idx,
                status,
                exit_code: item.exit_code,
                elapsed_seconds: item.elapsed_seconds,
                detail,
            },
        );
    }

    let task_observations: Vec<TaskExecutionObservation> = by_task.values().cloned().collect();
    let any_running = task_observations
        .iter()
        .any(|t| t.status == ComputeJobStatus::Running);
    let any_queued = task_observations
        .iter()
        .any(|t| t.status == ComputeJobStatus::Queued);
    let missing_tasks = task_observations.len() < expected_array_len as usize;

    let overall = if any_running {
        ComputeJobStatus::Running
    } else if any_queued
        || (missing_tasks
            && !task_observations
                .iter()
                .any(|t| t.status == ComputeJobStatus::Cancelled))
    {
        ComputeJobStatus::Queued
    } else if task_observations
        .iter()
        .all(|t| t.status == ComputeJobStatus::Completed)
        && !missing_tasks
    {
        ComputeJobStatus::Completed
    } else if task_observations
        .iter()
        .any(|t| t.status == ComputeJobStatus::Cancelled)
    {
        ComputeJobStatus::Cancelled
    } else if task_observations
        .iter()
        .any(|t| t.status == ComputeJobStatus::TimedOut)
    {
        ComputeJobStatus::TimedOut
    } else {
        ComputeJobStatus::Failed
    };

    let detail = task_observations
        .iter()
        .filter_map(|t| t.detail.as_deref())
        .next()
        .map(str::to_owned);

    ComputeJobPoll {
        status: overall,
        task_observations,
        detail,
    }
}

impl<T: SlurmTransport> ComputeBackend for SlurmComputeBackend<T> {
    fn target_kind(&self) -> ExecutionTargetKind {
        ExecutionTargetKind::Slurm
    }

    fn cluster_or_host_id(&self) -> &str {
        &self.policy.cluster_id
    }

    fn submit(&self, bundle: &StagedJobBundle) -> Result<SubmittedComputeJob> {
        bundle.validate()?;
        let bundle_sha256 = bundle.bundle_sha256()?;
        let (active_jobs, active_gpus) = self.current_active_quota()?;
        let effective_container = bundle
            .container_image_sha256
            .as_deref()
            .or_else(|| self.profile.container_digest());
        self.policy
            .validate_submission(
                &self.profile.partition,
                self.profile.account.as_deref(),
                self.profile.qos.as_deref(),
                &bundle.resources,
                bundle.array_len,
                effective_container,
                active_jobs,
                active_gpus,
            )
            .map_err(|e| anyhow::anyhow!("SLURM cluster policy rejected submission: {e}"))?;

        let flags = build_slurm_flags(&self.profile, bundle)?;
        let sbatch_script = render_sbatch_script(&self.profile, bundle)?;

        let mut staged_files = bundle.task_files.clone();
        staged_files.insert(
            "manifest.sha256".to_owned(),
            bundle.render_sha256sum_manifest(),
        );
        staged_files.insert("job.sbatch".to_owned(), sbatch_script);

        let staging_ref = self.transport.stage_bundle(&bundle.job_id, &staged_files)?;
        let slurm_job_id = self
            .transport
            .submit_batch(&staging_ref, "job.sbatch", &flags)?;

        let requested_total_gpus = bundle
            .resources
            .gpu_count()
            .saturating_mul(bundle.array_len);
        let mut tracked = self
            .tracked
            .lock()
            .map_err(|_| anyhow::anyhow!("tracked SLURM jobs lock poisoned"))?;
        tracked.insert(
            bundle.job_id.clone(),
            TrackedSlurmJob {
                array_len: bundle.array_len,
                requested_total_gpus,
                active: true,
            },
        );

        Ok(SubmittedComputeJob {
            job_id: bundle.job_id.clone(),
            target_kind: ExecutionTargetKind::Slurm,
            cluster_or_host_id: self.policy.cluster_id.clone(),
            external_job_id: Some(slurm_job_id.as_str().to_owned()),
            staging_ref,
            bundle_sha256,
            allocated_gpu_indices: Vec::new(),
        })
    }

    fn poll(&self, job: &SubmittedComputeJob) -> Result<ComputeJobPoll> {
        let mut batch = self.poll_batch(&[job])?;
        batch
            .remove(&job.job_id)
            .context("missing SLURM poll result")
    }

    fn fetch_outputs(
        &self,
        job: &SubmittedComputeJob,
        allowlist: &[String],
        max_total_bytes: u64,
    ) -> Result<FetchedJobOutputs> {
        let capped = max_total_bytes.min(self.policy.max_fetch_bytes);
        let fetched = self
            .transport
            .fetch_outputs(&job.staging_ref, allowlist, capped)?;
        fetched.validate(allowlist, capped)?;
        Ok(fetched)
    }

    fn cancel(&self, job: &SubmittedComputeJob) -> Result<()> {
        self.cancel_batch(&[job])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ai4math_core::{GpuKind, GpuRequest, ResourceRequest};
    use ai4math_evidence::ClusterMountEntry;

    fn sample_policy() -> SlurmClusterPolicy {
        SlurmClusterPolicy {
            cluster_id: "hpc_chem_01".into(),
            allowed_partitions: vec!["gpu-a100".into()],
            allowed_accounts: vec!["compchem".into()],
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
            max_fetch_bytes: 1024 * 1024,
        }
    }

    fn sample_profile() -> SlurmExecutionProfile {
        SlurmExecutionProfile {
            partition: "gpu-a100".into(),
            account: Some("compchem".into()),
            qos: Some("normal".into()),
            constraint: Some("a100_80g".into()),
            gpu_gres_kind: Some("a100".into()),
            runtime: SlurmContainerRuntime::Apptainer {
                sif_remote_path: "/containers/openmm_8.1.sif".into(),
                expected_sif_sha256: "d".repeat(64),
            },
            entrypoint_args: vec![
                "/opt/verify/run_fep.py".into(),
                "--input".into(),
                "job.cfg".into(),
            ],
        }
    }

    fn sample_bundle(array_len: u32) -> Result<StagedJobBundle> {
        let mut files = BTreeMap::new();
        let mut allowlist = Vec::new();
        for i in 0..array_len {
            files.insert(format!("tasks/{i}/job.cfg"), format!("task={i}\n"));
            allowlist.push(format!("tasks/{i}/result.json"));
        }
        StagedJobBundle::new(
            "job_slurm_1".into(),
            "md_fep_v1".into(),
            "a".repeat(64),
            "b".repeat(40),
            files,
            allowlist,
            Some("d".repeat(64)),
            vec![ClusterMountEntry {
                input_name: "receptor_pdb".into(),
                remote_ro_path: "/datasets/pdb/1abc.pdb".into(),
                expected_sha256: "e".repeat(64),
                container_mount_path: "/inputs/receptor.pdb".into(),
            }],
            ResourceRequest {
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
            },
            array_len,
        )
    }

    #[test]
    fn sbatch_renderer_pins_container_sha256_manifest_check_and_ro_mounts() -> Result<()> {
        let profile = sample_profile();
        let bundle = sample_bundle(3)?;
        let script = render_sbatch_script(&profile, &bundle)?;
        assert!(script.contains("#SBATCH --partition=gpu-a100"));
        assert!(script.contains("#SBATCH --gres=gpu:a100:1"));
        assert!(script.contains("#SBATCH --array=0-2"));
        assert!(script.contains("sha256sum -c manifest.sha256"));
        assert!(script.contains(&"d".repeat(64)));
        assert!(script.contains("--containall --no-home --nv"));
        assert!(script.contains("--bind '/datasets/pdb/1abc.pdb:/inputs/receptor.pdb:ro'"));
        Ok(())
    }

    #[test]
    fn sacct_and_squeue_parsers_handle_arrays_oom_timeout_and_cancelled() -> Result<()> {
        let sacct = "\
12345_0|COMPLETED|0:0|142|cpu=8,gres/gpu=1|gpu-node-01
12345_0.batch|COMPLETED|0:0|142|cpu=8|gpu-node-01
12345_1|OUT_OF_MEMORY|137:9|88|cpu=8,gres/gpu=1|gpu-node-02
12345_2|CANCELLED by 10042|0:15|12|cpu=8,gres/gpu=1|gpu-node-03
12345_[3-5]|PENDING|0:0|0||
";
        let obs = parse_sacct_output(sacct)?;
        assert_eq!(obs.len(), 3);
        assert_eq!(obs[0].array_task_id, Some(0));
        assert_eq!(obs[0].state, SlurmJobState::Completed);
        assert_eq!(obs[0].exit_code, Some(0));
        assert_eq!(obs[1].array_task_id, Some(1));
        assert_eq!(obs[1].state, SlurmJobState::OutOfMemory);
        assert_eq!(obs[1].exit_code, Some(137));
        assert_eq!(obs[2].state, SlurmJobState::Cancelled);

        let sq = "12345_3|PENDING|\n12345_4|RUNNING|gpu-node-04\n";
        let sq_obs = parse_squeue_output(sq)?;
        assert_eq!(sq_obs.len(), 2);
        assert_eq!(sq_obs[0].state, SlurmJobState::Pending);
        assert_eq!(sq_obs[1].state, SlurmJobState::Running);
        Ok(())
    }

    #[test]
    fn ssh_transport_enforces_batch_strict_host_key_and_rejects_option_injection() -> Result<()> {
        let cfg = SshSlurmConfig {
            ssh_binary: PathBuf::from("/usr/bin/ssh"),
            host: "login.hpc.example.org".into(),
            user: "researcher".into(),
            port: 2222,
            identity_file: PathBuf::from("/etc/ai4math/id_ed25519"),
            known_hosts_file: PathBuf::from("/etc/ai4math/known_hosts"),
            control_path: Some(PathBuf::from("/tmp/a4m-ssh.sock")),
            remote_staging_root: "/scratch/researcher/ai4math".into(),
            connect_timeout_secs: 15,
            remote_sbatch: "/usr/bin/sbatch".into(),
            remote_squeue: "/usr/bin/squeue".into(),
            remote_sacct: "/usr/bin/sacct".into(),
            remote_scancel: "/usr/bin/scancel".into(),
        };
        let transport = SshSlurmTransport::new(cfg.clone())?;
        let argv = transport.build_ssh_base_args();
        assert!(argv.contains(&"BatchMode=yes".to_owned()));
        assert!(argv.contains(&"StrictHostKeyChecking=yes".to_owned()));
        assert!(argv.contains(&"IdentitiesOnly=yes".to_owned()));
        assert!(argv.contains(&"--".to_owned()));
        assert!(argv.contains(&"researcher@login.hpc.example.org".to_owned()));

        let mut bad_host = cfg.clone();
        bad_host.host = "-oProxyCommand=evil".into();
        assert!(SshSlurmTransport::new(bad_host).is_err());

        let mut bad_root = cfg;
        bad_root.remote_staging_root = "/scratch/../etc".into();
        assert!(SshSlurmTransport::new(bad_root).is_err());
        Ok(())
    }

    #[test]
    fn ustar_roundtrip_enforces_allowlist_regular_files_and_byte_caps() -> Result<()> {
        let files = BTreeMap::from([
            ("tasks/0/result.json".into(), r#"{"ddg":-2.1}"#.into()),
            ("tasks/1/result.json".into(), r#"{"ddg":-0.2}"#.into()),
        ]);
        let archive = ustar_pack(&files)?;
        let allow = vec![
            "tasks/0/result.json".to_owned(),
            "tasks/1/result.json".to_owned(),
        ];
        let unpacked = ustar_unpack_allowlisted(&archive, &allow, 1024)?;
        assert_eq!(unpacked.files.len(), 2);
        assert_eq!(unpacked.files["tasks/0/result.json"], br#"{"ddg":-2.1}"#);

        assert!(ustar_unpack_allowlisted(&archive, &allow, 10).is_err());
        assert!(ustar_unpack_allowlisted(&archive, &[allow[0].clone()], 1024).is_err());

        let mut symlink_tar = archive;
        symlink_tar[156] = b'2';
        symlink_tar[148..156].fill(b' ');
        let chk: u32 = symlink_tar[..512].iter().map(|&b| u32::from(b)).sum();
        symlink_tar[148..156].copy_from_slice(format!("{chk:06o}\0 ").as_bytes());
        assert!(ustar_unpack_allowlisted(&symlink_tar, &allow, 1024).is_err());
        Ok(())
    }

    #[test]
    fn slurm_backend_enforces_reconciled_quota_and_conservative_array_aggregation() -> Result<()> {
        struct StubTransport;
        impl SlurmTransport for StubTransport {
            fn stage_bundle(&self, dir: &str, _files: &BTreeMap<String, String>) -> Result<String> {
                Ok(format!("/scratch/{dir}"))
            }
            fn submit_batch(&self, _: &str, _: &str, _: &[SlurmFlag]) -> Result<SlurmJobId> {
                SlurmJobId::new("55501", None)
            }
            fn query_status(&self, _: &[SlurmJobId]) -> Result<Vec<SlurmJobObservation>> {
                Ok(vec![
                    SlurmJobObservation {
                        job_id: SlurmJobId::new("55501", None)?,
                        array_task_id: Some(0),
                        state: SlurmJobState::Completed,
                        exit_code: Some(0),
                        elapsed_seconds: 10.0,
                        allocated_tres: None,
                        node_list: None,
                    },
                    SlurmJobObservation {
                        job_id: SlurmJobId::new("55501", None)?,
                        array_task_id: Some(1),
                        state: SlurmJobState::OutOfMemory,
                        exit_code: Some(137),
                        elapsed_seconds: 8.0,
                        allocated_tres: None,
                        node_list: None,
                    },
                ])
            }
            fn fetch_outputs(&self, _: &str, _: &[String], _: u64) -> Result<FetchedJobOutputs> {
                Ok(FetchedJobOutputs::default())
            }
            fn cancel_jobs(&self, _: &[SlurmJobId]) -> Result<()> {
                Ok(())
            }
        }

        let backend = SlurmComputeBackend::new(sample_policy(), sample_profile(), StubTransport)?;
        backend.set_reconciled_quota(1, 2)?;
        assert!(backend.submit(&sample_bundle(3)?).is_err());

        let submitted = backend.submit(&sample_bundle(2)?)?;
        assert_eq!(submitted.external_job_id.as_deref(), Some("55501"));
        let poll = backend.poll(&submitted)?;
        assert_eq!(poll.status, ComputeJobStatus::Failed);
        Ok(())
    }
}
