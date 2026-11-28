use anyhow::{Result, ensure};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub(super) const FILE_CAP: u64 = 32 * 1024 * 1024;

#[derive(Debug)]
pub(crate) struct ChildExit {
    pub status: std::process::ExitStatus,
    pub diagnostics: String,
}
impl std::fmt::Display for ChildExit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "sandboxed verifier exited {}: {}",
            self.status, self.diagnostics
        )
    }
}
impl std::error::Error for ChildExit {}

/// A launcher that could not be spawned never yields a verifier exit.
#[cfg(any(target_os = "macos", test))]
#[derive(Debug)]
struct LauncherStartError {
    launcher: &'static str,
    source: std::io::Error,
}

#[cfg(any(target_os = "macos", test))]
impl std::fmt::Display for LauncherStartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "starting {}; no unsandboxed fallback: {}",
            self.launcher, self.source
        )
    }
}

#[cfg(any(target_os = "macos", test))]
impl std::error::Error for LauncherStartError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum ProcessGroup {
    #[default]
    OwnSession,
    Supervisor,
}

pub(crate) struct Invocation<'a> {
    pub executable: &'a Path,
    pub args: &'a [String],
    pub room: &'a Path,
    pub writable: &'a Path,
    pub reads: &'a [PathBuf],
    pub env: &'a [(&'a str, &'a Path)],
    pub timeout: Duration,
}

#[cfg(target_os = "macos")]
fn quoted(path: &Path) -> Result<String> {
    let path = path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("Seatbelt requires UTF-8 paths"))?;
    ensure!(
        !path.chars().any(char::is_control),
        "control character in Seatbelt path"
    );
    Ok(format!(
        "\"{}\"",
        path.replace('\\', "\\\\").replace('"', "\\\"")
    ))
}

#[cfg(target_os = "macos")]
fn profile(
    executable: &Path,
    room: &Path,
    writable: &Path,
    reads: &[PathBuf],
    data_reads: &[PathBuf],
    literal_data_reads: &[PathBuf],
) -> Result<String> {
    use std::collections::BTreeSet;
    let mut text = String::from(
        r#"(version 1)
(deny default)
(deny network*)
(deny process-fork)
(allow sysctl-read)
(allow system-info (literal "vfs.disk-space"))
; dyld ignition opens the root directory; literal does not grant child paths.
(allow file-read* (literal "/"))
(allow file-read* file-map-executable (subpath "/System/Volumes/Preboot/Cryptexes/OS/System/Library/dyld"))
(allow file-map-executable (subpath "/usr/lib") (subpath "/System/Library"))
(allow file-read* (subpath "/usr/lib") (subpath "/System/Library") (literal "/dev/null") (literal "/dev/urandom") (literal "/dev/random"))
"#,
    );
    text += &format!(
        "(allow process-exec (literal {0}))\n(allow file-map-executable (literal {0}))\n",
        quoted(executable)?
    );
    for path in reads {
        let filter = if path.is_dir() && !std::fs::symlink_metadata(path)?.file_type().is_symlink()
        {
            "subpath"
        } else {
            "literal"
        };
        text += &format!("(allow file-map-executable ({filter} {}))\n", quoted(path)?);
    }
    text += &format!("(allow file-read* (subpath {}))\n", quoted(room)?);
    text += &format!("(allow file-write* (subpath {}))\n", quoted(writable)?);
    let mut metadata = BTreeSet::new();
    for path in [executable, room, writable]
        .into_iter()
        .chain(reads.iter().chain(data_reads).map(PathBuf::as_path))
    {
        for parent in path.ancestors().skip(1) {
            metadata.insert(parent);
        }
        let filter = if path.is_dir() && !std::fs::symlink_metadata(path)?.file_type().is_symlink()
        {
            "subpath"
        } else {
            "literal"
        };
        text += &format!("(allow file-read* ({filter} {}))\n", quoted(path)?);
    }
    for path in literal_data_reads {
        // Allow listing the exact directory, but not reading any children.
        text += &format!("(allow file-read* (literal {}))\n", quoted(path)?);
        metadata.extend(path.ancestors().skip(1));
    }
    for parent in metadata {
        text += &format!("(allow file-read-metadata (literal {}))\n", quoted(parent)?);
    }
    Ok(text)
}

pub(crate) fn run(spec: Invocation<'_>) -> Result<String> {
    run_with_data(spec, &[], &[])
}

/// Unlike native runtime grants, these trusted data paths cannot be mapped as executable.
pub(crate) fn run_with_data(
    spec: Invocation<'_>,
    data_reads: &[PathBuf],
    literal_data_reads: &[PathBuf],
) -> Result<String> {
    run_grouped(
        spec,
        data_reads,
        literal_data_reads,
        ProcessGroup::OwnSession,
    )
}

pub(crate) fn run_grouped(
    spec: Invocation<'_>,
    data_reads: &[PathBuf],
    literal_data_reads: &[PathBuf],
    group: ProcessGroup,
) -> Result<String> {
    run_grouped_until(spec, data_reads, literal_data_reads, group, None)
}

/// Run with an explicitly selected current directory inside the granted room.
/// This is needed by standalone checkers that initialize state in getcwd();
/// the selected directory must be the private writable scratch, never a data root.
pub(crate) fn run_grouped_until_in_dir(
    spec: Invocation<'_>,
    data_reads: &[PathBuf],
    literal_data_reads: &[PathBuf],
    group: ProcessGroup,
    deadline: Option<Instant>,
    cwd: &Path,
) -> Result<String> {
    let bytes = run_inner(
        spec,
        data_reads,
        literal_data_reads,
        group,
        None,
        deadline,
        cwd,
    )?;
    let log = String::from_utf8_lossy(&bytes).into_owned();
    deadline_remaining(deadline)?;
    Ok(log)
}

/// Apply an absolute wall-clock bound in addition to the invocation timeout.
pub(crate) fn run_grouped_until(
    spec: Invocation<'_>,
    data_reads: &[PathBuf],
    literal_data_reads: &[PathBuf],
    group: ProcessGroup,
    deadline: Option<Instant>,
) -> Result<String> {
    let cwd = spec.room;
    let bytes = run_inner(
        spec,
        data_reads,
        literal_data_reads,
        group,
        None,
        deadline,
        cwd,
    )?;
    let log = String::from_utf8_lossy(&bytes).into_owned();
    deadline_remaining(deadline)?;
    Ok(log)
}

fn deadline_remaining(deadline: Option<Instant>) -> Result<Option<Duration>> {
    deadline
        .map(|deadline| {
            let remaining = deadline.saturating_duration_since(Instant::now());
            ensure!(
                !remaining.is_zero(),
                "sandbox timeout: absolute wall-clock budget exhausted"
            );
            Ok(remaining)
        })
        .transpose()
}

const MAX_CAPTURE_STDOUT: usize = 16 * 1024 * 1024;

/// Capture stdout verbatim; stderr is reserved for bounded diagnostics.
pub(crate) fn run_capture_bytes(
    spec: Invocation<'_>,
    data_reads: &[PathBuf],
    literal_data_reads: &[PathBuf],
    group: ProcessGroup,
    max_stdout: usize,
) -> Result<Vec<u8>> {
    ensure!(
        max_stdout <= MAX_CAPTURE_STDOUT,
        "sandbox stdout capture limit exceeds 16 MiB"
    );
    let cwd = spec.room;
    run_inner(
        spec,
        data_reads,
        literal_data_reads,
        group,
        Some(max_stdout),
        None,
        cwd,
    )
}

#[cfg(any(target_os = "macos", test))]
fn check_capture_size(file: &std::fs::File, max: usize, overflow: &str) -> Result<()> {
    ensure!(file.metadata()?.len() <= max as u64, "{overflow}");
    Ok(())
}

#[cfg(any(target_os = "macos", test))]
fn read_capture(file: &mut std::fs::File, max: usize, overflow: &str) -> Result<Vec<u8>> {
    use std::io::{Read, Seek};

    check_capture_size(file, max, overflow)?;
    file.rewind()?;
    let mut bytes = Vec::new();
    file.take(max as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= max, "{overflow}");
    Ok(bytes)
}

#[cfg(target_os = "macos")]
fn run_inner(
    spec: Invocation<'_>,
    data_reads: &[PathBuf],
    literal_data_reads: &[PathBuf],
    group: ProcessGroup,
    max_stdout: Option<usize>,
    deadline: Option<Instant>,
    cwd: &Path,
) -> Result<Vec<u8>> {
    use std::{fs, process::Command};

    const SEATBELT: &str = "/usr/bin/sandbox-exec";
    deadline_remaining(deadline)?;
    let executable = fs::canonicalize(spec.executable)?;
    let room = fs::canonicalize(spec.room)?;
    let writable = fs::canonicalize(spec.writable)?;
    let cwd = fs::canonicalize(cwd)?;
    ensure!(
        writable.starts_with(&room) && writable != room,
        "write grant outside room"
    );
    ensure!(cwd.starts_with(&room), "current directory outside room");
    let mut reads = Vec::new();
    for requested in spec.reads {
        let real = fs::canonicalize(requested)?;
        if &real != requested {
            // dyld resolves embedded install names via lexical aliases before reaching
            // the canonical target. Both refer to an operator-approved read root.
            reads.push(requested.clone());
            for parent in requested.ancestors().skip(1) {
                if fs::symlink_metadata(parent).is_ok_and(|m| m.file_type().is_symlink()) {
                    reads.push(parent.to_path_buf());
                }
            }
        }
        reads.push(real);
    }
    let data_reads = data_reads
        .iter()
        .map(fs::canonicalize)
        .collect::<std::io::Result<Vec<_>>>()?;
    let literal_data_reads = literal_data_reads
        .iter()
        .map(fs::canonicalize)
        .collect::<std::io::Result<Vec<_>>>()?;
    deadline_remaining(deadline)?;
    let policy = profile(
        &executable,
        &room,
        &writable,
        &reads,
        &data_reads,
        &literal_data_reads,
    )?;
    let mut command = Command::new(SEATBELT);
    command
        .arg("-p")
        .arg(policy)
        .arg(&executable)
        .args(spec.args)
        .env_clear()
        .env("HOME", &writable)
        .env("TMPDIR", &writable)
        .env("LANG", "en_US.UTF-8")
        .envs(spec.env.iter().copied())
        .current_dir(&cwd);
    supervise(
        command,
        spec.timeout,
        group,
        max_stdout,
        deadline,
        "Seatbelt",
    )
}

// This function is intentionally private. Backends must construct a trusted
// sandbox launcher first, and ensure that killing the launcher stops the
// payload even in Supervisor mode (where we cannot kill the caller's group).
#[cfg(any(target_os = "macos", all(test, target_os = "linux")))]
fn supervise(
    mut command: std::process::Command,
    timeout: Duration,
    group: ProcessGroup,
    max_stdout: Option<usize>,
    deadline: Option<Instant>,
    launcher: &'static str,
) -> Result<Vec<u8>> {
    use std::{
        io,
        os::unix::process::CommandExt,
        process::{Child, Stdio},
        thread,
    };

    // Capture through anonymous regular files, not pipes that a stuck process could keep open.
    const LOG_CAP: usize = 256 * 1024;
    const LOG_OVERFLOW: &str = "sandbox diagnostic output exceeded limit";
    const STDOUT_OVERFLOW: &str = "sandbox stdout exceeded limit";
    let mut log = tempfile::tempfile()?;
    let mut stdout = match max_stdout {
        Some(max) => Some((tempfile::tempfile()?, max)),
        None => None,
    };
    let output = stdout.as_ref().map_or(&log, |(file, _)| file);
    command
        .stdin(Stdio::null())
        .stdout(output.try_clone()?)
        .stderr(log.try_clone()?);
    let cpu_budget = deadline_remaining(deadline)?.map_or(timeout, |left| left.min(timeout));
    let cpu = cpu_budget.as_secs().saturating_add(1);
    let own_session = group == ProcessGroup::OwnSession;
    ensure!(
        own_session || unsafe { libc::getpgrp() == libc::getpid() },
        "supervised verification requires the coordinator's dedicated process group"
    );
    // Only async-signal-safe libc functions in the post-fork child.
    unsafe {
        command.pre_exec(move || {
            if own_session && libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            for (resource, requested) in [
                (libc::RLIMIT_CPU, cpu),
                (libc::RLIMIT_FSIZE, FILE_CAP),
                (libc::RLIMIT_CORE, 0),
            ] {
                let mut limit = libc::rlimit {
                    rlim_cur: 0,
                    rlim_max: 0,
                };
                if libc::getrlimit(resource, &mut limit) != 0 {
                    return Err(io::Error::last_os_error());
                }
                let capped = limit.rlim_max.min(limit.rlim_cur).min(requested);
                limit.rlim_cur = capped;
                limit.rlim_max = capped;
                if libc::setrlimit(resource, &limit) != 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    struct Running(Option<Child>, bool);
    impl Drop for Running {
        fn drop(&mut self) {
            if let Some(child) = &mut self.0 {
                // PID belongs to the unreaped session leader, so it cannot be reused here.
                if self.1 {
                    unsafe {
                        libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
                    }
                }
                // In supervisor mode the child shares the CLI group; do not kill
                // that group for an individual timeout. The backend must contain descendants.
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
    // An absolute deadline includes policy and command setup, unlike the legacy timeout.
    deadline_remaining(deadline)?;
    let start = Instant::now();
    let mut running = Running(
        Some(
            command
                .spawn()
                .map_err(|source| LauncherStartError { launcher, source })?,
        ),
        own_session,
    );
    let status = loop {
        if let Some(status) = running.0.as_mut().unwrap().try_wait()? {
            break status;
        }
        deadline_remaining(deadline)?;
        ensure!(
            start.elapsed() < timeout,
            "sandbox timeout after {:?}",
            timeout
        );
        check_capture_size(&log, LOG_CAP, LOG_OVERFLOW)?;
        if let Some((file, max)) = &stdout {
            check_capture_size(file, *max, STDOUT_OVERFLOW)?;
        }
        let poll_interval = Duration::from_millis(10);
        let sleep_for = deadline_remaining(deadline)?.map_or(poll_interval, |left| {
            left.min(poll_interval)
                .min(timeout.saturating_sub(start.elapsed()))
        });
        thread::sleep(sleep_for);
    };
    // The backend guarantees that leader exit leaves no descendants.
    running.0 = None;
    deadline_remaining(deadline)?;
    let diagnostics = read_capture(&mut log, LOG_CAP, LOG_OVERFLOW)?;
    if !status.success() {
        let diagnostics = String::from_utf8_lossy(&diagnostics).into_owned();
        deadline_remaining(deadline)?;
        return Err(ChildExit {
            status,
            diagnostics,
        }
        .into());
    }
    let bytes = match &mut stdout {
        Some((file, max)) => {
            deadline_remaining(deadline)?;
            read_capture(file, *max, STDOUT_OVERFLOW)?
        }
        None => diagnostics,
    };
    deadline_remaining(deadline)?;
    Ok(bytes)
}

#[cfg(not(target_os = "macos"))]
#[derive(Debug)]
struct BackendUnavailable;

#[cfg(not(target_os = "macos"))]
impl std::fmt::Display for BackendUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            "native verification sandbox unavailable on this platform; no unsandboxed fallback",
        )
    }
}

#[cfg(not(target_os = "macos"))]
impl std::error::Error for BackendUnavailable {}

#[cfg(not(target_os = "macos"))]
fn run_inner(
    spec: Invocation<'_>,
    _data_reads: &[PathBuf],
    _literal_data_reads: &[PathBuf],
    _group: ProcessGroup,
    _max_stdout: Option<usize>,
    _deadline: Option<Instant>,
    _cwd: &Path,
) -> Result<Vec<u8>> {
    let Invocation {
        executable,
        args,
        room,
        writable,
        reads,
        env,
        timeout,
    } = spec;
    let _ = (executable, args, room, writable, reads, env, timeout);
    Err(BackendUnavailable.into())
}

/// Checks a successful in-room action, denied out-of-room reads/writes, denied
/// process creation, denied loopback bind, and bounded process cleanup.
pub fn sandbox_self_test() -> Result<()> {
    let room = tempfile::tempdir()?;
    let outside = tempfile::tempdir()?;
    let room = room.path().canonicalize()?;
    let external = outside.path().canonicalize()?;
    let write = room.join("output");
    std::fs::create_dir(&write)?;
    std::fs::write(external.join("protected"), "host marker")?;
    let shell = |args: &[String], timeout| {
        run(Invocation {
            executable: Path::new("/bin/bash"),
            args,
            room: &room,
            writable: &write,
            reads: &[],
            env: &[],
            timeout,
        })
    };
    let args = [
        "-c".into(),
        r#"
        printf ok > "$1/allowed" || exit 10
        if read -r content < "$2/protected"; then exit 11; fi
        if printf changed > "$2/protected"; then exit 12; fi
        if printf changed > "$2/new"; then exit 13; fi
        exit 0
    "#
        .into(),
        "self-test".into(),
        write.to_str().unwrap().into(),
        external.to_str().unwrap().into(),
    ];
    shell(&args, Duration::from_secs(5))
        .map_err(|e| anyhow::anyhow!("filesystem and process isolation: {e:#}"))?;
    ensure!(
        std::fs::read(write.join("allowed"))? == b"ok",
        "scratch write missing"
    );
    ensure!(
        std::fs::read(external.join("protected"))? == b"host marker"
            && !external.join("new").exists(),
        "host data changed"
    );
    let fork = shell(
        &["-c".into(), "(/usr/bin/true); :".into()],
        Duration::from_secs(5),
    );
    ensure!(
        fork.is_err_and(|e| e.to_string().contains("fork: Operation not permitted")),
        "fork denial self-test failed"
    );
    let args = ["-l".into(), "127.0.0.1".into(), "0".into()];
    let err = run(Invocation {
        executable: Path::new("/usr/bin/nc"),
        args: &args,
        room: &room,
        writable: &write,
        reads: &[],
        env: &[],
        timeout: Duration::from_secs(3),
    })
    .expect_err("loopback bind must be denied");
    let message = err.to_string();
    ensure!(
        !message.contains("sandbox-exec:")
            && (message.contains("Operation not permitted")
                || message.contains("Permission denied")),
        "could not establish loopback deny: {message}"
    );
    let args = ["-c".into(), "while :; do :; done".into()];
    let result = shell(&args, Duration::from_millis(120));
    ensure!(
        result.is_err_and(|err| err.to_string().contains("sandbox timeout")),
        "timeout self-test failed"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[cfg(target_os = "macos")]
    #[test]
    fn lean_predecessor_pool_has_data_read_but_no_sandbox_write_grant() -> Result<()> {
        use crate::lean_objects::LeanObjects;

        let root = tempfile::tempdir()?;
        let mut pool = LeanObjects::create(&root.path().join("objects"), 32, 64)?;
        pool.compile(
            "Target.lean",
            "def value := 0",
            false,
            |room, writable, objects| {
                assert!(!objects.starts_with(room));
                assert!(writable.starts_with(room));
                let policy = profile(
                    Path::new("/usr/bin/true"),
                    room,
                    writable,
                    &[],
                    std::slice::from_ref(objects),
                    &[],
                )?;
                let data_grant = format!("(allow file-read* (subpath {}))", quoted(objects)?);
                assert!(policy.lines().any(|line| line == data_grant));
                let writes: Vec<_> = policy
                    .lines()
                    .filter(|line| line.starts_with("(allow file-write"))
                    .collect();
                assert_eq!(
                    writes,
                    [format!(
                        "(allow file-write* (subpath {}))",
                        quoted(writable)?
                    )]
                );
                let object_path = quoted(objects)?;
                assert!(!policy.lines().any(
                    |line| line.contains("file-map-executable") && line.contains(&object_path)
                ));
                Ok(String::new())
            },
        )?;
        Ok(())
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn launcher_spawn_failure_is_not_a_verifier_exit() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let missing = std::process::Command::new(dir.path().join("absent-launcher"));
        let error = supervise(
            missing,
            Duration::from_secs(1),
            ProcessGroup::OwnSession,
            None,
            None,
            "test launcher",
        )
        .unwrap_err();
        assert!(error.downcast_ref::<LauncherStartError>().is_some());
        assert!(error.downcast_ref::<ChildExit>().is_none());
        Ok(())
    }

    #[test]
    fn expired_deadline_does_not_enter_sandbox_prep() {
        let unused = Path::new("");
        let spec = Invocation {
            executable: unused,
            args: &[],
            room: unused,
            writable: unused,
            reads: &[],
            env: &[],
            timeout: Duration::from_secs(1),
        };
        let error = run_grouped_until(
            spec,
            &[],
            &[],
            ProcessGroup::OwnSession,
            Some(Instant::now()),
        )
        .unwrap_err();
        if cfg!(target_os = "macos") {
            assert!(
                error
                    .to_string()
                    .contains("absolute wall-clock budget exhausted")
            );
        } else {
            assert!(error.to_string().contains("no unsandboxed fallback"));
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "requires a host that permits native Seatbelt startup"]
    fn absolute_deadline_caps_a_running_verifier() -> Result<()> {
        let room = tempfile::tempdir()?;
        let output = room.path().join("output");
        std::fs::create_dir(&output)?;
        let args = ["-c".into(), "while :; do :; done".into()];
        let error = run_grouped_until(
            Invocation {
                executable: Path::new("/bin/bash"),
                args: &args,
                room: room.path(),
                writable: &output,
                reads: &[],
                env: &[],
                timeout: Duration::from_secs(3),
            },
            &[],
            &[],
            ProcessGroup::OwnSession,
            Some(Instant::now() + Duration::from_millis(150)),
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("absolute wall-clock budget exhausted"),
            "unexpected sandbox result: {error:#}"
        );
        Ok(())
    }

    #[test]
    fn capture_reads_exact_limit_and_preserves_bytes() -> Result<()> {
        let mut file = tempfile::tempfile()?;
        let bytes = b"{\"name\":\"\xff\"}\n";
        file.write_all(bytes)?;
        assert_eq!(read_capture(&mut file, bytes.len(), "overflow")?, bytes);
        Ok(())
    }

    #[test]
    fn capture_rejects_file_past_limit() -> Result<()> {
        let mut file = tempfile::tempfile()?;
        assert!(read_capture(&mut file, 0, "overflow")?.is_empty());
        file.set_len(17)?;
        assert_eq!(
            read_capture(&mut file, 16, "overflow")
                .unwrap_err()
                .to_string(),
            "overflow"
        );
        Ok(())
    }

    #[test]
    fn capture_rejects_limit_above_ceiling_before_launch() {
        let unused = Path::new("");
        let spec = Invocation {
            executable: unused,
            args: &[],
            room: unused,
            writable: unused,
            reads: &[],
            env: &[],
            timeout: Duration::ZERO,
        };
        let err = run_capture_bytes(
            spec,
            &[],
            &[],
            ProcessGroup::OwnSession,
            16 * 1024 * 1024 + 1,
        )
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "sandbox stdout capture limit exceeds 16 MiB"
        );
    }
}
