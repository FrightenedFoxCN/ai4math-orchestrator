#![cfg(unix)]

use ai4math_broker::{ProjectNativeBusy, ProjectNativeGate, ProjectNativeLease};
use anyhow::{Context, Result, ensure};
use std::{
    env,
    ffi::CString,
    fs::{self, DirBuilder, OpenOptions, Permissions},
    io::{self, Read, Write},
    os::{
        fd::AsRawFd,
        unix::{
            ffi::OsStrExt,
            fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt, symlink},
            process::ExitStatusExt,
        },
    },
    path::Path,
    process::{Child, Command, Stdio},
};
use tempfile::TempDir;

const LOCK: &str = ".runtime/native-execution.lock";
const ROOT_ENV: &str = "AI4MATH_PROJECT_NATIVE_GATE_TEST_ROOT";
const LANE_ENV: &str = "AI4MATH_PROJECT_NATIVE_GATE_TEST_LANE";

fn project() -> Result<TempDir> {
    let root = tempfile::tempdir()?;
    DirBuilder::new()
        .mode(0o700)
        .create(root.path().join(".runtime"))?;
    Ok(root)
}

fn write_lock(path: &Path, bytes: &[u8]) -> Result<()> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?
        .write_all(bytes)?;
    Ok(())
}

fn assert_busy(result: Result<ProjectNativeLease>) {
    let err = result.expect_err("conflicting lease must be refused");
    assert!(
        err.is::<ProjectNativeBusy>(),
        "not a typed busy error: {err:#}"
    );
}

fn assert_invalid(result: Result<ProjectNativeLease>) {
    let err = result.expect_err("malformed lock structure must be refused");
    assert!(
        !err.is::<ProjectNativeBusy>(),
        "structural error reported as busy: {err:#}"
    );
}

// Spawn this integration test executable again. Stderr carries single-byte
// acknowledgments, separate from the test harness's stdout; stdin is control.
#[test]
fn gate_holder_child() -> Result<()> {
    let Some(root) = env::var_os(ROOT_ENV) else {
        return Ok(());
    };
    let gate = ProjectNativeGate::new(Path::new(&root));
    let lease = match env::var(LANE_ENV)?.as_str() {
        "shared" => gate.try_fast()?,
        "exclusive" => gate.try_heavy()?,
        _ => anyhow::bail!("invalid gate holder lane"),
    };
    io::stderr().write_all(b"R")?;
    let mut command = [0];
    io::stdin().read_exact(&mut command)?;
    assert_eq!(command, *b"D");
    drop(lease);
    io::stderr().write_all(b"D")?;
    // Stay alive without the lease so the parent can verify RAII release.
    io::stdin().read_exact(&mut command)?;
    assert_eq!(command, *b"Q");
    Ok(())
}

struct Holder(Child);

impl Holder {
    fn spawn(root: &Path, lane: &str) -> Result<Self> {
        let mut child = Self(
            Command::new(env::current_exe()?)
                .args(["--exact", "gate_holder_child", "--nocapture"])
                .env(ROOT_ENV, root)
                .env(LANE_ENV, lane)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()?,
        );
        child.expect(b'R')?;
        Ok(child)
    }

    fn expect(&mut self, expected: u8) -> Result<()> {
        let pipe = self
            .0
            .stderr
            .as_mut()
            .context("missing gate holder acknowledgment pipe")?;
        let mut poll = libc::pollfd {
            fd: pipe.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: poll points to one initialized, writable pollfd and pipe is live.
        let ready = unsafe { libc::poll(&mut poll, 1, 10_000) };
        if ready < 0 {
            return Err(io::Error::last_os_error()).context("poll gate holder pipe");
        }
        ensure!(ready == 1, "timed out awaiting gate holder acknowledgment");
        let mut received = [0];
        pipe.read_exact(&mut received)
            .context("read gate holder acknowledgment")?;
        assert_eq!(received, [expected]);
        Ok(())
    }

    fn send(&mut self, command: u8) -> Result<()> {
        self.0
            .stdin
            .as_mut()
            .context("missing gate holder control pipe")?
            .write_all(&[command])?;
        Ok(())
    }
}

impl Drop for Holder {
    fn drop(&mut self) {
        // Reap the child even if the test fails before the final handshake.
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn shared_across_processes_and_drop_releases_while_holder_is_alive() -> Result<()> {
    let root = project()?;
    let lock = root.path().join(LOCK);
    write_lock(&lock, b"keep the stable lock file")?;
    let before = fs::metadata(&lock)?;
    let gate = ProjectNativeGate::new(root.path());
    let mut holder = Holder::spawn(root.path(), "shared")?;
    let fast = gate.try_fast()?;
    assert_busy(gate.try_heavy());

    holder.send(b'D')?;
    holder.expect(b'D')?;
    assert_busy(gate.try_heavy());
    drop(fast);
    let heavy = gate.try_heavy()?;
    assert!(holder.0.try_wait()?.is_none());
    holder.send(b'Q')?;
    assert!(holder.0.wait()?.success());
    drop(heavy);

    let after = fs::metadata(&lock)?;
    assert_eq!((before.dev(), before.ino()), (after.dev(), after.ino()));
    assert_eq!(fs::read(lock)?, b"keep the stable lock file");
    Ok(())
}

#[test]
fn exclusive_conflicts_only_in_same_project_and_kill_releases_lock() -> Result<()> {
    let root = project()?;
    let other = project()?;
    let left = ProjectNativeGate::new(root.path());
    let right = ProjectNativeGate::new(other.path());
    let mut holder = Holder::spawn(root.path(), "exclusive")?;
    assert_busy(left.try_fast());
    assert_busy(left.try_heavy());
    let _independent = right.try_heavy()?;

    let lock = root.path().join(LOCK);
    let before = fs::metadata(&lock)?;
    assert_eq!(before.mode() & 0o777, 0o600);
    holder.0.kill()?;
    assert_eq!(holder.0.wait()?.signal(), Some(libc::SIGKILL));
    let _reacquired = left.try_heavy()?;
    let after = fs::metadata(&lock)?;
    assert_eq!((before.dev(), before.ino()), (after.dev(), after.ino()));
    Ok(())
}

#[test]
fn constructor_has_no_io_and_missing_runtime_is_not_created() -> Result<()> {
    let root = tempfile::tempdir()?;
    let absent = root.path().join("absent");
    let gate = ProjectNativeGate::new(&absent);
    assert!(!absent.exists());
    assert_invalid(gate.try_heavy());
    assert!(!absent.exists());

    let gate = ProjectNativeGate::new(root.path());
    assert_invalid(gate.try_fast());
    assert!(!root.path().join(".runtime").exists());

    let root = project()?;
    let runtime = root.path().join(".runtime");
    fs::set_permissions(&runtime, Permissions::from_mode(0o500))?;
    let _gate = ProjectNativeGate::new(root.path());
    assert_eq!(fs::read_dir(runtime)?.count(), 0);
    Ok(())
}

#[test]
fn reject_runtime_symlink_non_directory_and_writable_directory() -> Result<()> {
    let root = tempfile::tempdir()?;
    let runtime = root.path().join(".runtime");
    let other = tempfile::tempdir()?;
    let gate = ProjectNativeGate::new(root.path());
    symlink(other.path(), &runtime)?;
    assert_invalid(gate.try_heavy());
    assert_eq!(fs::read_dir(other.path())?.count(), 0);
    fs::remove_file(&runtime)?;
    fs::write(&runtime, b"not a directory")?;
    assert_invalid(gate.try_fast());
    fs::remove_file(&runtime)?;
    fs::create_dir(&runtime)?;
    for mode in [0o720, 0o702] {
        fs::set_permissions(&runtime, Permissions::from_mode(mode))?;
        assert_invalid(gate.try_heavy());
        assert!(!root.path().join(LOCK).exists());
    }
    Ok(())
}

#[test]
fn reject_file_symlink_hardlink_fifo_and_nonprivate_permissions() -> Result<()> {
    let root = project()?;
    let lock = root.path().join(LOCK);
    let target = root.path().join("target");
    let gate = ProjectNativeGate::new(root.path());
    write_lock(&target, b"never replace or truncate this")?;
    symlink(&target, &lock)?;
    assert_invalid(gate.try_fast());
    assert!(fs::symlink_metadata(&lock)?.is_symlink());
    fs::remove_file(&lock)?;
    fs::hard_link(&target, &lock)?;
    assert_invalid(gate.try_heavy());
    assert_eq!(fs::metadata(&lock)?.nlink(), 2);
    fs::remove_file(&lock)?;

    let fifo = CString::new(lock.as_os_str().as_bytes())?;
    // SAFETY: fifo is a valid NUL-terminated pathname.
    if unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) } != 0 {
        return Err(io::Error::last_os_error()).context("create project native lock FIFO fixture");
    }
    assert_invalid(gate.try_fast());
    fs::remove_file(&lock)?;

    write_lock(&lock, b"private data")?;
    for mode in [0o640, 0o601] {
        fs::set_permissions(&lock, Permissions::from_mode(mode))?;
        assert_invalid(gate.try_heavy());
        assert_eq!(fs::metadata(&lock)?.mode() & 0o777, mode);
    }
    assert_eq!(fs::read(lock)?, b"private data");
    assert_eq!(fs::read(target)?, b"never replace or truncate this");
    Ok(())
}
