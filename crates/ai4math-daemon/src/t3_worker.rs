//! Opt-in operator sweep. The CLI, not this timer or its checkpoint, holds proof authority.
use ai4math_store::{StateStore, T3SweepState, T3SweepStatus};
use anyhow::{Result, ensure};
use serde::{Deserialize, de::DeserializeOwned};
use std::{
    fs::{self, Metadata, OpenOptions},
    io::{self, Read},
    os::unix::{
        ffi::OsStrExt,
        fs::{MetadataExt, OpenOptionsExt},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const MAX_CONFIG_BYTES: u64 = 16 * 1024;
const MILLIS: std::ops::RangeInclusive<u64> = 100..=3_600_000;
const DEFAULT_TIMEOUT_MS: u64 = 120_000;
// Healthy but repeatedly rewound pages still advance revision. Limit work per daemon run.
const MAX_STEPS: usize = 256;
pub(super) const POLL_INTERVAL: Duration = Duration::from_millis(20);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    version: u8,
    cli_executable: PathBuf,
    bare_repo: PathBuf,
    plan_sha256: String,
    native_executable: PathBuf,
    page_limit: usize,
    interval_ms: u64,
    #[serde(default = "default_timeout_ms")]
    step_timeout_ms: u64,
}

fn default_timeout_ms() -> u64 {
    DEFAULT_TIMEOUT_MS
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        // Never include parser output, OS diagnostics or configured paths in a startup error.
        Self::read(path).map_err(|_| anyhow::anyhow!("invalid --t3-autosweep configuration"))
    }

    fn read(path: &Path) -> Result<Self> {
        let config: Self = read_trusted_config(path)?;
        ensure!(
            config.version == 1
                && config.plan_sha256.len() == 64
                && config
                    .plan_sha256
                    .bytes()
                    .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
                && (1..=8).contains(&config.page_limit)
                && MILLIS.contains(&config.interval_ms)
                && MILLIS.contains(&config.step_timeout_ms)
                && config.cli_executable != config.native_executable,
            "invalid worker configuration"
        );
        config.check_paths()?;
        Ok(config)
    }

    fn check_paths(&self) -> Result<()> {
        trusted_directory(&self.bare_repo)?;
        for executable in [&self.cli_executable, &self.native_executable] {
            trusted_executable(executable)?;
        }
        Ok(())
    }

    pub fn start(self, root: PathBuf) {
        if thread::Builder::new()
            .name("ai4math-t3-autosweep".into())
            .spawn(move || self.run(&root).log())
            .is_err()
        {
            Stop::StepFailed.log();
        }
    }

    fn run(&self, root: &Path) -> Stop {
        match self.sweep(root) {
            Ok(()) => Stop::Complete,
            Err(stop) => stop,
        }
    }

    fn sweep(&self, root: &Path) -> std::result::Result<(), Stop> {
        for step in 1..=MAX_STEPS {
            self.check_paths().map_err(|_| Stop::Unavailable)?;
            let before = self.progress(root).map_err(|_| Stop::Unavailable)?;
            if before.status == T3SweepState::Complete {
                return Ok(());
            }
            let expected = before
                .revision
                .unwrap_or(0)
                .checked_add(1)
                .ok_or(Stop::NoProgress)?;
            self.step(root)?;
            let after = self.progress(root).map_err(|_| Stop::Unavailable)?;
            // One readback, no retry. A successful exit alone says nothing about progress.
            if after.revision != Some(expected) || after.reviewed_commit != before.reviewed_commit {
                return Err(Stop::NoProgress);
            }
            if after.status == T3SweepState::Complete {
                return Ok(());
            }
            if step != MAX_STEPS {
                thread::sleep(Duration::from_millis(self.interval_ms));
            }
        }
        Err(Stop::StepLimit)
    }

    fn progress(&self, root: &Path) -> Result<T3SweepStatus> {
        let path = trusted_state_path(root)?;
        // Each observation uses a separate read-only connection, dropped before a child runs.
        let state = StateStore::open_readonly(path)?;
        let repo = state.managed_repo()?;
        ensure!(
            repo.as_deref().map(str::as_bytes) == Some(self.bare_repo.as_os_str().as_bytes()),
            "project is not bound to the supplied repository"
        );
        // This read also requires an applied plan. Current reviewed main and other authority
        // are checked by the CLI every time, under its own native and Git gates.
        state.t3_sweep_status(&self.plan_sha256)
    }

    fn step(&self, root: &Path) -> std::result::Result<(), Stop> {
        let started = Instant::now();
        let mut child = Command::new(&self.cli_executable)
            .current_dir(root)
            .arg("t3-sweep-step")
            .arg(root)
            .arg(&self.bare_repo)
            .arg(&self.plan_sha256)
            .arg(self.page_limit.to_string())
            .arg(&self.native_executable)
            .arg("--daemon-parent")
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .map_err(|_| Stop::StepFailed)?;
        // Keep the pipe writer independent of Child::wait, which would close child.stdin.
        // This writer's death tells the guarded CLI to kill its own process group.
        let Some(_parent_pipe) = child.stdin.take() else {
            kill_and_reap(&mut child);
            return Err(Stop::StepFailed);
        };
        let timeout = Duration::from_millis(self.step_timeout_ms);
        loop {
            match child.try_wait() {
                Ok(Some(status)) if status.success() => return Ok(()),
                Ok(Some(_)) => return Err(Stop::StepFailed),
                Ok(None) => (),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => (),
                // If an external reaper already took the child, its PID is no longer ours.
                Err(e) if e.raw_os_error() == Some(libc::ECHILD) => return Err(Stop::StepFailed),
                Err(_) => {
                    kill_and_reap(&mut child);
                    return Err(Stop::StepFailed);
                }
            }
            let remaining = timeout.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                kill_and_reap(&mut child);
                return Err(Stop::TimedOut);
            }
            thread::sleep(remaining.min(POLL_INTERVAL));
        }
    }
}

pub(super) fn kill_and_reap(child: &mut Child) {
    // Signal before reaping so the child's PID (and group identity) cannot be reused yet.
    // SAFETY: Negative pid targets the private group established by process_group(0).
    let pgid = child.id() as libc::pid_t;
    if pgid > 0 && unsafe { libc::kill(-pgid, libc::SIGKILL) } != 0 {
        let _ = child.kill();
    }
    let _ = child.wait();
}

enum Stop {
    Complete,
    Unavailable,
    StepFailed,
    TimedOut,
    NoProgress,
    StepLimit,
}

impl Stop {
    fn log(self) {
        let reason = match self {
            Self::Complete => "complete",
            Self::Unavailable => "authorization or state unavailable",
            Self::StepFailed => "step failed",
            Self::TimedOut => "step timed out",
            Self::NoProgress => "checkpoint did not advance",
            Self::StepLimit => "startup step limit reached",
        };
        eprintln!("ai4mathd t3 autosweep stopped: {reason}");
    }
}

pub fn verified_root(project: &Path) -> Result<PathBuf> {
    let result = (|| -> Result<PathBuf> {
        let root = project.canonicalize()?;
        trusted_directory(&root)?;
        Ok(root)
    })();
    result.map_err(|_| anyhow::anyhow!("invalid project root for --t3-autosweep"))
}

pub(super) fn trusted_directory(path: &Path) -> Result<()> {
    ensure!(
        trusted_metadata(path)?.is_dir(),
        "expected a trusted directory"
    );
    Ok(())
}

pub(super) fn trusted_metadata(path: &Path) -> Result<Metadata> {
    ensure!(
        path.is_absolute()
            && path
                .to_str()
                .is_some_and(|s| !s.chars().any(char::is_control)),
        "expected an absolute UTF-8 path"
    );
    // SAFETY: geteuid takes no arguments.
    let uid = unsafe { libc::geteuid() };
    let ancestors: Vec<_> = path.ancestors().skip(1).collect();
    for dir in ancestors.into_iter().rev() {
        let meta = fs::symlink_metadata(dir)?;
        // System parents can be root-owned. Sticky, root-owned shared parents (e.g. /tmp)
        // cannot be used to replace other owners' entries; every descendant is checked too.
        ensure!(
            meta.is_dir()
                && (meta.uid() == uid || meta.uid() == 0)
                && (meta.mode() & 0o022 == 0 || (meta.uid() == 0 && meta.mode() & 0o1000 != 0)),
            "untrusted parent directory"
        );
        reject_acl_grants(dir)?;
    }
    let meta = fs::symlink_metadata(path)?;
    ensure!(
        !meta.file_type().is_symlink() && meta.uid() == uid && meta.mode() & 0o022 == 0,
        "untrusted path"
    );
    // Byte equality also rejects alternate spellings that Path's equality normalizes.
    ensure!(
        path.canonicalize()?.as_os_str().as_bytes() == path.as_os_str().as_bytes(),
        "expected a canonical path"
    );
    reject_acl_grants(path)?;
    Ok(meta)
}

pub(super) fn read_trusted_config<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let expected = trusted_metadata(path)?;
    check_config_file(&expected)?;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let opened = file.metadata()?;
    check_config_file(&opened)?;
    ensure!(
        (expected.dev(), expected.ino()) == (opened.dev(), opened.ino()),
        "configuration replaced while opening"
    );
    let mut bytes = Vec::new();
    file.take(MAX_CONFIG_BYTES + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_CONFIG_BYTES,
        "configuration too large"
    );
    Ok(serde_json::from_slice(&bytes)?)
}

pub(super) fn trusted_executable(path: &Path) -> Result<()> {
    let meta = trusted_metadata(path)?;
    ensure!(
        meta.is_file() && meta.nlink() == 1 && meta.mode() & 0o7100 == 0o100,
        "expected an ordinary owner-executable file"
    );
    Ok(())
}

pub(super) fn trusted_state_path(root: &Path) -> Result<PathBuf> {
    trusted_directory(root)?;
    let runtime = root.join(".runtime");
    trusted_directory(&runtime)?;
    let path = runtime.join("state.sqlite");
    check_state_file(&trusted_metadata(&path)?)?;
    for name in [
        "state.sqlite-wal",
        "state.sqlite-shm",
        "state.sqlite-journal",
    ] {
        let path = runtime.join(name);
        match fs::symlink_metadata(&path) {
            Ok(meta) => {
                check_state_file(&meta)?;
                reject_acl_grants(&path)?;
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
    }
    Ok(path)
}

fn check_config_file(meta: &Metadata) -> Result<()> {
    ensure!(
        meta.is_file()
            && meta.nlink() == 1
            && meta.mode() & 0o7177 == 0
            && meta.mode() & 0o400 != 0
            && meta.len() <= MAX_CONFIG_BYTES,
        "expected bounded owner-only configuration"
    );
    Ok(())
}

pub(super) fn check_state_file(meta: &Metadata) -> Result<()> {
    // SAFETY: geteuid takes no arguments.
    let uid = unsafe { libc::geteuid() };
    ensure!(
        meta.is_file() && meta.nlink() == 1 && meta.uid() == uid && meta.mode() & 0o022 == 0,
        "unsafe state file"
    );
    Ok(())
}

#[cfg(target_os = "macos")]
fn reject_acl_grants(path: &Path) -> Result<()> {
    // Darwin's extended ACL grants need not be reflected in group mode bits. Deny-only
    // ACLs (including the standard home-directory deny-delete entry) grant no new access.
    use std::{ffi::CString, ptr};
    const ACL_TYPE_EXTENDED: libc::c_uint = 0x100;
    const ACL_EXTENDED_DENY: libc::c_uint = 2;
    const ACL_FIRST_ENTRY: libc::c_int = 0;
    const ACL_NEXT_ENTRY: libc::c_int = -1;
    unsafe extern "C" {
        fn acl_get_link_np(path: *const libc::c_char, ty: libc::c_uint) -> *mut libc::c_void;
        fn acl_get_entry(
            acl: *mut libc::c_void,
            index: libc::c_int,
            entry: *mut *mut libc::c_void,
        ) -> libc::c_int;
        fn acl_get_tag_type(entry: *mut libc::c_void, tag: *mut libc::c_uint) -> libc::c_int;
        fn acl_free(acl: *mut libc::c_void) -> libc::c_int;
    }
    let encoded = CString::new(path.as_os_str().as_bytes())?;
    // SAFETY: encoded is NUL-terminated and lives for the call; the result is owned below.
    let acl = unsafe { acl_get_link_np(encoded.as_ptr(), ACL_TYPE_EXTENDED) };
    if acl.is_null() {
        let error = io::Error::last_os_error();
        // ENOENT is also Darwin's no-ACL result for an object known to exist.
        return match error.raw_os_error() {
            Some(libc::ENOENT | libc::ENOTSUP) => Ok(()),
            _ => Err(error.into()),
        };
    }
    let result = (|| -> Result<()> {
        let mut index = ACL_FIRST_ENTRY;
        loop {
            let mut entry = ptr::null_mut();
            // SAFETY: acl remains live; entry and tag are writable, correctly typed outputs.
            if unsafe { acl_get_entry(acl, index, &mut entry) } != 0 {
                let error = io::Error::last_os_error();
                // EINVAL for an out-of-range index is the end of an ACL obtained above.
                return if error.raw_os_error() == Some(libc::EINVAL) {
                    Ok(())
                } else {
                    Err(error.into())
                };
            }
            let mut tag = 0;
            ensure!(
                unsafe { acl_get_tag_type(entry, &mut tag) } == 0 && tag == ACL_EXTENDED_DENY,
                "ACL grants are not allowed on trusted paths"
            );
            index = ACL_NEXT_ENTRY;
        }
    })();
    // SAFETY: acl_get_link_np returned a non-null ACL that is freed just once.
    unsafe { acl_free(acl) };
    result
}

#[cfg(not(target_os = "macos"))]
fn reject_acl_grants(_path: &Path) -> Result<()> {
    // POSIX ACL grants are constrained by the group mode bits checked above.
    Ok(())
}
