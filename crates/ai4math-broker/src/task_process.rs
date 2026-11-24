use std::{path::Path, time::Duration};

/// Run one pinned native task executable under macOS Seatbelt, with no host
/// environment or inherited broker descriptors. Input and result are opaque bytes;
/// callers own JSON schema validation. Worker stderr is discarded.
///
/// A native Mach-O is necessary but not sufficient: the worker and its runtime
/// must start with only the granted system loader and shared cache. Runtimes
/// that require additional host files or services are not supported.
///
/// Limits: 64 MiB executable, 1 MiB input, 1 MiB result, and a nonzero
/// wall timeout of at most 60 seconds (also rounded up to set the CPU limit).
pub fn run_task_process(
    executable: &Path,
    executable_sha256: &str,
    input_json: &[u8],
    timeout: Duration,
) -> anyhow::Result<Vec<u8>> {
    #[cfg(target_os = "macos")]
    {
        macos::run(executable, executable_sha256, input_json, timeout)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (executable, executable_sha256, input_json, timeout);
        anyhow::bail!("task process isolation requires macOS Seatbelt; no unsandboxed fallback")
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use anyhow::{Context, Result, bail, ensure};
    use sha2::{Digest, Sha256};
    use std::{
        fs::{self, File, OpenOptions, Permissions},
        io::{self, Read, Write},
        os::unix::{
            fs::{OpenOptionsExt, PermissionsExt},
            process::CommandExt,
        },
        path::Path,
        process::{Child, Command, Stdio},
        thread,
        time::{Duration, Instant},
    };

    const EXECUTABLE_CAP: usize = 64 * 1024 * 1024;
    const INPUT_CAP: usize = 1024 * 1024;
    const RESULT_CAP: u64 = 1024 * 1024;
    const MAX_TIMEOUT: Duration = Duration::from_secs(60);
    const SEATBELT: &str = "/usr/bin/sandbox-exec";

    // Darwin CPU_TYPE_ARM64 / CPU_TYPE_X86_64; require a slice matching the broker.
    const NATIVE_CPU: u32 = if cfg!(target_arch = "aarch64") {
        0x0100_000c
    } else {
        0x0100_0007
    };

    fn native_executable(bytes: &[u8]) -> bool {
        fn thin(bytes: &[u8]) -> bool {
            if bytes.len() < 32 || bytes[..4] != [0xcf, 0xfa, 0xed, 0xfe] {
                return false;
            }
            let word = |offset| u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());
            // MH_EXECUTE, not MH_DYLIB/MH_BUNDLE/MH_OBJECT; commands must fit the slice.
            word(4) == NATIVE_CPU && word(12) == 2 && word(20) as usize <= bytes.len() - 32
        }
        let (wide, little_endian) = match bytes.get(..4) {
            Some([0xca, 0xfe, 0xba, 0xbe]) => (false, false),
            Some([0xca, 0xfe, 0xba, 0xbf]) => (true, false),
            Some([0xbe, 0xba, 0xfe, 0xca]) => (false, true),
            Some([0xbf, 0xba, 0xfe, 0xca]) => (true, true),
            _ => return thin(bytes),
        };
        let Some(count) = bytes.get(4..8) else {
            return false;
        };
        let word = |data: &[u8]| {
            let data = data.try_into().unwrap();
            if little_endian {
                u32::from_le_bytes(data)
            } else {
                u32::from_be_bytes(data)
            }
        };
        let quad = |data: &[u8]| {
            let data = data.try_into().unwrap();
            if little_endian {
                u64::from_le_bytes(data)
            } else {
                u64::from_be_bytes(data)
            }
        };
        let entry_size = if wide { 32 } else { 20 };
        let Some(table_end) = (word(count) as usize)
            .checked_mul(entry_size)
            .and_then(|n| n.checked_add(8))
        else {
            return false;
        };
        let Some(table) = bytes.get(8..table_end) else {
            return false;
        };
        let mut has_native = false;
        for entry in table.chunks_exact(entry_size) {
            let (offset, len) = if wide {
                (quad(&entry[8..16]), quad(&entry[16..24]))
            } else {
                (
                    u64::from(word(&entry[8..12])),
                    u64::from(word(&entry[12..16])),
                )
            };
            let Some(end) = offset.checked_add(len) else {
                return false;
            };
            if offset < table_end as u64 || end > bytes.len() as u64 {
                return false;
            }
            if word(&entry[..4]) == NATIVE_CPU {
                if !thin(&bytes[offset as usize..end as usize]) {
                    return false;
                }
                has_native = true;
            }
        }
        has_native
    }

    fn stage_executable(source: &Path, pin: &str, staged: &Path) -> Result<()> {
        ensure!(
            source.is_absolute(),
            "task executable path must be absolute"
        );
        ensure!(
            pin.len() == 64 && pin.bytes().all(|b| b.is_ascii_hexdigit()),
            "task executable SHA-256 must be 64 hexadecimal digits"
        );
        ensure!(
            fs::symlink_metadata(source)
                .context("inspect task executable")?
                .is_file(),
            "task executable must be a regular file, not a symlink"
        );
        // A concurrent replacement with a FIFO must not turn open() into a wait;
        // the checks after open apply to the descriptor we actually read.
        let input = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(source)
            .context("open task executable")?;
        let metadata = input.metadata()?;
        ensure!(metadata.is_file(), "task executable must be a regular file");
        ensure!(
            metadata.len() <= EXECUTABLE_CAP as u64,
            "task executable exceeds 64 MiB"
        );
        let mut bytes = Vec::new();
        input
            .take(EXECUTABLE_CAP as u64 + 1)
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() <= EXECUTABLE_CAP,
            "task executable exceeds 64 MiB"
        );
        ensure!(
            format!("{:x}", Sha256::digest(&bytes)).eq_ignore_ascii_case(pin),
            "task executable SHA-256 mismatch"
        );
        ensure!(
            native_executable(&bytes),
            "task executable must be a native Mach-O executable, not a script or library"
        );
        let mut file = new_file(staged)?;
        file.write_all(&bytes)?;
        // Only these checked bytes execute; do not carry over original modes or xattrs.
        file.set_permissions(Permissions::from_mode(0o500))?;
        Ok(())
    }

    fn new_file(path: &Path) -> io::Result<File> {
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
    }

    fn quoted(path: &Path) -> Result<String> {
        let path = path.to_str().context("Seatbelt paths must be UTF-8")?;
        ensure!(
            !path.chars().any(char::is_control),
            "control character in Seatbelt path"
        );
        Ok(format!(
            "\"{}\"",
            path.replace('\\', "\\\\").replace('"', "\\\"")
        ))
    }

    fn profile(executable: &Path, room: &Path) -> Result<String> {
        let exe = quoted(executable)?;
        let mut text = format!(
            r#"(version 1)
(deny default)
(deny network*)
(deny process-fork)
(allow process-exec (literal {exe}))
(allow file-read* file-map-executable (literal {exe}))
; Bootstrapping the system dynamic loader and its shared cache is required
; but none of the compiler sandbox's library trees or workspaces are granted.
; dyld opens the root directory itself; a literal grant does not cover descendants.
(allow file-read* (literal "/"))
(allow file-read* file-map-executable (literal "/usr/lib/dyld")
    (subpath "/System/Library/dyld")
    (subpath "/System/Volumes/Preboot/Cryptexes/OS/System/Library/dyld"))
"#
        );
        for parent in room.ancestors().take_while(|p| *p != Path::new("/")) {
            text += &format!("(allow file-read-metadata (literal {}))\n", quoted(parent)?);
        }
        Ok(text)
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct ProcFd {
        fd: libc::c_int,
        kind: u32,
    }

    unsafe extern "C" {
        fn proc_pidinfo(
            pid: libc::c_int,
            flavor: libc::c_int,
            arg: u64,
            buffer: *mut libc::c_void,
            buffersize: libc::c_int,
        ) -> libc::c_int;
    }

    const PROC_PIDLISTFDS: libc::c_int = 1;
    const MAX_LISTED_FDS: usize = 4096;

    fn seal_descriptors(fds: &mut [ProcFd; MAX_LISTED_FDS]) -> io::Result<()> {
        // Darwin's proc_pidinfo wraps a kernel call. Enumerate *after* fork to
        // include FDs opened concurrently by other broker threads, including
        // ones above a lowered RLIMIT_NOFILE. Mark CLOEXEC rather than close so
        // Rust's exec-error channel stays usable until the launcher execs.
        let capacity = size_of_val(fds);
        let count = unsafe {
            proc_pidinfo(
                libc::getpid(),
                PROC_PIDLISTFDS,
                0,
                fds.as_mut_ptr().cast(),
                capacity as libc::c_int,
            )
        };
        if count <= 0 {
            return Err(io::Error::last_os_error());
        }
        let count = count as usize;
        if count >= capacity || count % size_of::<ProcFd>() != 0 {
            return Err(io::Error::from_raw_os_error(libc::EOVERFLOW));
        }
        for entry in &fds[..count / size_of::<ProcFd>()] {
            if entry.fd > 2
                && unsafe { libc::fcntl(entry.fd, libc::F_SETFD, libc::FD_CLOEXEC) } == -1
            {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }

    fn cap_resource(resource: libc::c_int, requested: libc::rlim_t) -> io::Result<()> {
        let mut limit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        if unsafe { libc::getrlimit(resource, &mut limit) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let capped = limit.rlim_max.min(limit.rlim_cur).min(requested);
        limit.rlim_cur = capped;
        limit.rlim_max = capped;
        if unsafe { libc::setrlimit(resource, &limit) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    struct Running(Option<Child>);
    impl Drop for Running {
        fn drop(&mut self) {
            if let Some(child) = &mut self.0 {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    pub(super) fn run(
        source: &Path,
        pin: &str,
        input: &[u8],
        timeout: Duration,
    ) -> Result<Vec<u8>> {
        ensure!(input.len() <= INPUT_CAP, "task input exceeds 1 MiB");
        ensure!(
            !timeout.is_zero() && timeout <= MAX_TIMEOUT,
            "task timeout must be greater than zero and at most 60 seconds"
        );
        let temp = tempfile::Builder::new()
            .prefix("ai4math-task-")
            .permissions(Permissions::from_mode(0o700))
            .tempdir_in("/private/tmp")?;
        let room = temp.path().canonicalize()?;
        let executable = room.join("worker");
        stage_executable(source, pin, &executable)?;

        // Unlink stdio backing files before launch; the worker receives only a
        // read-only input and write-only output descriptor, not file paths.
        let stdin_path = room.join("input");
        new_file(&stdin_path)?.write_all(input)?;
        let stdin = File::open(&stdin_path)?;
        fs::remove_file(stdin_path)?;
        let stdout_path = room.join("output");
        let stdout = new_file(&stdout_path)?;
        let result = File::open(&stdout_path)?;
        fs::remove_file(stdout_path)?;

        let mut command = Command::new(SEATBELT);
        command
            .arg("-p")
            .arg(profile(&executable, &room)?)
            .arg(&executable)
            .env_clear()
            .current_dir(&room)
            .stdin(stdin)
            .stdout(stdout)
            .stderr(Stdio::null());
        let cpu = timeout.as_secs() + u64::from(timeout.subsec_nanos() != 0);
        let mut fds = Box::new([ProcFd { fd: -1, kind: 0 }; MAX_LISTED_FDS]);
        // No allocation, I/O to Rust stdio, or locks in the post-fork child.
        unsafe {
            command.pre_exec(move || {
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                cap_resource(libc::RLIMIT_CPU, cpu)?;
                // One extra byte acts as an overflow sentinel even if the
                // worker handles EFBIG or SIGXFSZ and exits successfully.
                cap_resource(libc::RLIMIT_FSIZE, RESULT_CAP + 1)?;
                cap_resource(libc::RLIMIT_CORE, 0)?;
                seal_descriptors(&mut fds)
            });
        }

        let start = Instant::now();
        let mut running =
            Running(Some(command.spawn().context(
                "launch isolated task process; no unsandboxed fallback",
            )?));
        let status = loop {
            let remaining = timeout.saturating_sub(start.elapsed());
            ensure!(!remaining.is_zero(), "task process timed out");
            if let Some(status) = running.0.as_mut().unwrap().try_wait()? {
                break status;
            }
            thread::sleep(remaining.min(Duration::from_millis(5)));
        };
        running.0 = None;
        ensure!(
            result.metadata()?.len() <= RESULT_CAP,
            "task stdout exceeds 1 MiB"
        );
        if !status.success() {
            bail!("isolated task process failed ({status})");
        }
        let mut output = Vec::new();
        result.take(RESULT_CAP + 1).read_to_end(&mut output)?;
        ensure!(
            output.len() as u64 <= RESULT_CAP,
            "task stdout exceeds 1 MiB"
        );
        Ok(output)
    }
}
