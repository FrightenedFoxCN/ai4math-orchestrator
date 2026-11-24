//! A daemon owns the only write end. Do not launch detached verifiers in this mode.
use anyhow::{Context, Result, ensure};
use std::{
    io::{self, Read},
    thread,
};

pub(super) fn arm() -> Result<()> {
    arm_for("T3")
}

pub(crate) fn arm_for(label: &'static str) -> Result<()> {
    // SAFETY: no pointer arguments. Refuse to signal an inherited, shared group.
    let pid = unsafe { libc::getpid() };
    ensure!(
        pid > 1 && unsafe { libc::getpgrp() } == pid,
        "daemon {label} requires a dedicated process group"
    );
    let mut meta = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: fstat fully initializes this stack buffer on success.
    if unsafe { libc::fstat(libc::STDIN_FILENO, meta.as_mut_ptr()) } < 0 {
        return Err(io::Error::last_os_error())
            .with_context(|| format!("inspect daemon {label} lifeline"));
    }
    let kind = unsafe { meta.assume_init() }.st_mode & libc::S_IFMT;
    ensure!(
        kind == libc::S_IFIFO || kind == libc::S_IFSOCK,
        "daemon {label} requires a private stdin lifeline pipe"
    );
    let mut pfd = libc::pollfd {
        fd: libc::STDIN_FILENO,
        events: libc::POLLIN,
        revents: 0,
    };
    // Poll once before any work, refusing an already-closed or non-silent pipe.
    ensure!(
        unsafe { libc::poll(&mut pfd, 1, 0) } == 0,
        "daemon {label} lifeline is already closed or has unexpected data"
    );
    thread::Builder::new()
        .name("ai4math-daemon-lifeline".into())
        .spawn(move || {
            let mut pipe = io::stdin().lock();
            let mut byte = [0];
            loop {
                match pipe.read(&mut byte) {
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    _ => {
                        // Neither daemon nor an unrelated process belongs to our group.
                        // EOF, I/O failure and any unexpected bytes all revoke it.
                        if unsafe { libc::getpgrp() == pid && libc::kill(-pid, libc::SIGKILL) == 0 }
                        {
                            // Await the uncatchable signal rather than racing it with abort().
                            loop {
                                thread::park();
                            }
                        }
                        // The group no longer exists or could not be signaled. Fail closed
                        // without running atexit code while another thread may be verifying.
                        unsafe {
                            libc::_exit(1);
                        }
                    }
                }
            }
        })
        .with_context(|| format!("start daemon {label} lifeline watcher"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{BufRead, BufReader, Write},
        os::unix::process::{CommandExt, ExitStatusExt},
        process::{Command, Stdio},
        time::{Duration, Instant},
    };

    #[test]
    fn lost_daemon_pipe_kills_dedicated_group_without_signaling_parent() -> Result<()> {
        const CHILD: &str = "AI4MATH_T3_LIFELINE_TEST_CHILD";
        if std::env::var_os(CHILD).is_some() {
            arm()?;
            // The newline goes to a real pipe even with the test harness present.
            let mut out = io::stdout().lock();
            writeln!(out, "LIFELINE_READY")?;
            out.flush()?;
            drop(out);
            loop {
                thread::park();
            }
        }
        let mut child = Command::new(std::env::current_exe()?)
            .args(["--exact", "t3::parent::tests::lost_daemon_pipe_kills_dedicated_group_without_signaling_parent", "--nocapture"])
            .env(CHILD,"1").process_group(0).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn()?;
        let out = child
            .stdout
            .take()
            .context("lifeline fixture has no output")?;
        let (send, recv) = std::sync::mpsc::channel();
        let reader = thread::spawn(move || {
            let ready = BufReader::new(out)
                .lines()
                .map_while(Result::ok)
                .any(|line| line == "LIFELINE_READY");
            let _ = send.send(ready);
        });
        let ready = recv.recv_timeout(Duration::from_secs(4));
        drop(child.stdin.take());
        let deadline = Instant::now() + Duration::from_secs(4);
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if Instant::now() >= deadline {
                child.kill()?;
                break child.wait()?;
            }
            thread::sleep(Duration::from_millis(10));
        };
        reader.join().expect("join lifeline fixture reader");
        assert_eq!(ready, Ok(true));
        assert_eq!(status.signal(), Some(libc::SIGKILL));
        Ok(())
    }
}
