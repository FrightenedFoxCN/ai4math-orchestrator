use anyhow::{Result, ensure};
use std::path::{Path, PathBuf};
use std::time::Duration;

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

#[cfg(target_os = "macos")]
pub(crate) fn run_grouped(
    spec: Invocation<'_>,
    data_reads: &[PathBuf],
    literal_data_reads: &[PathBuf],
    group: ProcessGroup,
) -> Result<String> {
    use anyhow::Context;
    use std::{
        fs,
        io::{self, Read, Seek},
        os::unix::process::CommandExt,
        process::{Child, Command, Stdio},
        thread,
        time::Instant,
    };

    // Capture through a bounded regular file, not pipes that a stuck process could keep open.
    const FILE_CAP: u64 = 32 * 1024 * 1024;
    const LOG_CAP: u64 = 256 * 1024;
    const SEATBELT: &str = "/usr/bin/sandbox-exec";
    let executable = fs::canonicalize(spec.executable)?;
    let room = fs::canonicalize(spec.room)?;
    let writable = fs::canonicalize(spec.writable)?;
    ensure!(
        writable.starts_with(&room) && writable != room,
        "write grant outside room"
    );
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
    let policy = profile(
        &executable,
        &room,
        &writable,
        &reads,
        &data_reads,
        &literal_data_reads,
    )?;
    let mut log = tempfile::tempfile()?;
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
        .current_dir(&room)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log.try_clone()?);
    let cpu = spec.timeout.as_secs().saturating_add(1);
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
                // that group for an individual timeout. Seatbelt denies child forks.
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
    let start = Instant::now();
    let mut running = Running(
        Some(
            command
                .spawn()
                .context("starting Seatbelt; no unsandboxed fallback")?,
        ),
        own_session,
    );
    let status = loop {
        if let Some(status) = running.0.as_mut().unwrap().try_wait()? {
            break status;
        }
        ensure!(
            start.elapsed() < spec.timeout,
            "sandbox timeout after {:?}",
            spec.timeout
        );
        ensure!(
            log.metadata()?.len() <= LOG_CAP,
            "sandbox diagnostic output exceeded limit"
        );
        thread::sleep(Duration::from_millis(10));
    };
    // The sandbox denies every fork, so successful leader exit leaves no descendants.
    running.0 = None;
    log.rewind()?;
    let mut text = Vec::new();
    log.take(LOG_CAP + 1).read_to_end(&mut text)?;
    ensure!(
        text.len() as u64 <= LOG_CAP,
        "sandbox diagnostic output exceeded limit"
    );
    let text = String::from_utf8_lossy(&text);
    if !status.success() {
        return Err(ChildExit {
            status,
            diagnostics: text.into_owned(),
        }
        .into());
    }
    Ok(text.into_owned())
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn run_grouped(
    spec: Invocation<'_>,
    _data_reads: &[PathBuf],
    _literal_data_reads: &[PathBuf],
    _group: ProcessGroup,
) -> Result<String> {
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
    anyhow::bail!("Seatbelt verification is unavailable outside macOS; no unsandboxed fallback")
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
