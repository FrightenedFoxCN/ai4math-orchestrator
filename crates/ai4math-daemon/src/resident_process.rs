//! Bounded supervision for trusted resident child commands started in a private process group.
use anyhow::{Context, Result, ensure};
use std::{
    io::{self, Read},
    os::fd::AsRawFd,
    process::Child,
    thread,
    time::{Duration, Instant},
};

pub(super) const POLL_INTERVAL: Duration = Duration::from_millis(20);
const MAX_STDOUT_BYTES: usize = 4096;

pub(super) fn kill_and_reap(child: &mut Child) {
    // Signal before reaping so the child's PID (and group identity) cannot be reused yet.
    // SAFETY: Negative pid targets the private group established by process_group(0).
    let pgid = child.id() as libc::pid_t;
    if pgid > 0 && unsafe { libc::kill(-pgid, libc::SIGKILL) } != 0 {
        let _ = child.kill();
    }
    let _ = child.wait();
}

pub(super) fn capture_stdout(
    child: &mut Child,
    started: Instant,
    timeout: Duration,
) -> Result<Vec<u8>> {
    // Keep this writer outside Child; wait must not close it before the child exits.
    let lifeline = child.stdin.take();
    let mut may_signal = true;
    let result = (|| {
        ensure!(lifeline.is_some(), "missing resident worker lifeline");
        let mut pipe = child
            .stdout
            .take()
            .context("missing resident worker output pipe")?;
        set_nonblocking(&pipe)?;
        let mut bytes = Vec::with_capacity(MAX_STDOUT_BYTES);
        let mut buffer = [0u8; 1024];
        let mut eof = false;
        loop {
            ensure!(started.elapsed() < timeout, "resident worker timed out");
            if !eof {
                let len = buffer.len().min(MAX_STDOUT_BYTES + 1 - bytes.len());
                match pipe.read(&mut buffer[..len]) {
                    Ok(0) => eof = true,
                    Ok(n) => {
                        ensure!(
                            bytes.len() + n <= MAX_STDOUT_BYTES,
                            "resident worker output too large"
                        );
                        bytes.extend_from_slice(&buffer[..n]);
                        continue;
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => (),
                    Err(error) => return Err(error.into()),
                }
            }
            // An exited leader's descendant may still own stdout. Do not reap
            // the leader before EOF so we can safely kill its group on timeout.
            if eof {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        may_signal = false;
                        ensure!(status.success(), "resident worker failed");
                        return Ok(bytes);
                    }
                    Ok(None) => (),
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => {
                        may_signal = error.raw_os_error() != Some(libc::ECHILD);
                        return Err(error.into());
                    }
                }
            }
            thread::sleep(timeout.saturating_sub(started.elapsed()).min(POLL_INTERVAL));
        }
    })();
    if may_signal {
        kill_and_reap(child);
    }
    result
}

fn set_nonblocking(pipe: &impl AsRawFd) -> io::Result<()> {
    let fd = pipe.as_raw_fd();
    // SAFETY: the pipe owns a live fd for both fcntl operations.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        os::unix::process::CommandExt,
        process::{Command, Stdio},
    };

    fn shell(script: &str) -> io::Result<Child> {
        Command::new("/bin/sh")
            .args(["-c", script])
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
    }

    #[test]
    fn captures_limit_and_reaps_oversized_or_failed_children() -> Result<()> {
        let timeout = Duration::from_secs(3);
        let mut child = shell(&format!("printf '%0{MAX_STDOUT_BYTES}d' 0"))?;
        assert_eq!(
            capture_stdout(&mut child, Instant::now(), timeout)?.len(),
            MAX_STDOUT_BYTES
        );
        assert!(child.try_wait()?.unwrap().success());

        let mut child = shell(&format!(
            "printf '%0{}d' 0; exec /bin/sleep 30",
            MAX_STDOUT_BYTES + 1
        ))?;
        let error = capture_stdout(&mut child, Instant::now(), timeout).unwrap_err();
        assert_eq!(error.to_string(), "resident worker output too large");
        assert!(!child.try_wait()?.unwrap().success());

        let mut child = shell("printf 'output'; exit 3")?;
        assert!(capture_stdout(&mut child, Instant::now(), timeout).is_err());
        assert_eq!(child.try_wait()?.unwrap().code(), Some(3));
        Ok(())
    }

    #[test]
    fn timeout_reaps_a_leader_even_when_a_descendant_holds_stdout() -> Result<()> {
        let mut child = shell("/bin/sleep 30 & exit 0")?;
        let error =
            capture_stdout(&mut child, Instant::now(), Duration::from_millis(100)).unwrap_err();
        assert_eq!(error.to_string(), "resident worker timed out");
        assert!(child.try_wait()?.is_some());
        Ok(())
    }
}
