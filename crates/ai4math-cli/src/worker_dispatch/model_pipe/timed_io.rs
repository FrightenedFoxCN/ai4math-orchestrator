//! Deadline-bounded unidirectional streams. The caller owns the peer ends.
use super::RESULT_CAP;
use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use std::{
    fs::File,
    io::{self, Read, Write},
    os::fd::{AsRawFd, FromRawFd, RawFd},
    time::{Duration, Instant},
};

const PACKET_MAX: usize = 128 * 1024;
pub(super) struct Wire {
    input: File,
    output: File,
}

// Requires a private pipe (or socketpair) rather than inherited files, tty or
// stdin=/dev/null. O_NONBLOCK changes the inherited *local* descriptions; it
// does not change the opposite ends owned by the trusted parent.
fn nonblocking_stream(fd: RawFd) -> Result<File> {
    // SAFETY: the kernel duplicates the descriptor and initializes stat on success.
    let new_fd = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
    if new_fd < 0 {
        return Err(io::Error::last_os_error()).context("duplicate DSH coordinator stream");
    }
    let file = unsafe { File::from_raw_fd(new_fd) };
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe { libc::fstat(new_fd, stat.as_mut_ptr()) } < 0 {
        return Err(io::Error::last_os_error()).context("inspect DSH coordinator stream");
    }
    let mode = unsafe { stat.assume_init() }.st_mode & libc::S_IFMT;
    ensure!(
        mode == libc::S_IFIFO || mode == libc::S_IFSOCK,
        "DSH coordinator requires private pipes or socketpairs"
    );
    let flags = unsafe { libc::fcntl(new_fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(new_fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error()).context("set nonblocking DSH coordinator stream");
    }
    Ok(file)
}

fn ready(fd: RawFd, events: libc::c_short, deadline: Instant) -> Result<bool> {
    loop {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return Ok(false);
        };
        let ms = remaining
            .as_millis()
            .saturating_add(1)
            .clamp(1, i32::MAX as u128) as i32;
        let mut entry = libc::pollfd {
            fd,
            events,
            revents: 0,
        };
        // SAFETY: entry points at one pollfd and is present for the syscall.
        let result = unsafe { libc::poll(&mut entry, 1, ms) };
        if result < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e).context("poll DSH coordinator stream");
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        if result == 0 {
            continue;
        }
        ensure!(
            entry.revents & libc::POLLNVAL == 0,
            "invalid DSH coordinator stream"
        );
        return Ok(true); // On HUP/ERR, read/write observes EOF or the concrete failure.
    }
}

impl Wire {
    pub(super) fn open() -> Result<Self> {
        Ok(Self {
            input: nonblocking_stream(libc::STDIN_FILENO)?,
            output: nonblocking_stream(libc::STDOUT_FILENO)?,
        })
    }
    pub(super) fn send(&mut self, value: Value, deadline: Instant) -> Result<()> {
        let mut bytes = serde_json::to_vec(&value)?;
        bytes.push(b'\n');
        ensure!(
            bytes.len() <= PACKET_MAX,
            "oversized DSH coordinator output packet"
        );
        let mut remaining = bytes.as_slice();
        while !remaining.is_empty() {
            ensure!(
                ready(self.output.as_raw_fd(), libc::POLLOUT, deadline)?,
                "DSH model pipe deadline elapsed"
            );
            match self.output.write(remaining) {
                Ok(0) => bail!("closed DSH coordinator output"),
                Ok(n) => remaining = &remaining[n..],
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                    ) =>
                {
                    continue;
                }
                Err(e) => return Err(e).context("write DSH coordinator stream"),
            }
        }
        Ok(())
    }
    pub(super) fn receive(
        &mut self,
        deadline: Instant,
        mut check: impl FnMut() -> Result<()>,
    ) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        let mut chunk = [0u8; 4096];
        let mut next_check = Instant::now();
        loop {
            let now = Instant::now();
            ensure!(now < deadline, "DSH model pipe deadline elapsed");
            if now >= next_check {
                check()?;
                next_check = Instant::now() + Duration::from_millis(250);
            }
            if !ready(
                self.input.as_raw_fd(),
                libc::POLLIN,
                deadline.min(next_check),
            )? {
                continue;
            }
            match self.input.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    ensure!(
                        bytes.len() + n <= RESULT_CAP,
                        "DSH coordinator result exceeds 32 KiB"
                    );
                    bytes.extend_from_slice(&chunk[..n]);
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                    ) =>
                {
                    continue;
                }
                Err(e) => return Err(e).context("read DSH coordinator stream"),
            }
        }
        if bytes.last() == Some(&b'\n') {
            bytes.pop();
        }
        ensure!(
            !bytes.is_empty() && !bytes.contains(&b'\n') && !bytes.contains(&b'\r'),
            "expected one DSH coordinator result line followed by EOF"
        );
        Ok(bytes)
    }
}
