//! Bounded, read-only observation of the direct managed main ref. Callers
//! provide an operator-bound repository, never an RPC-supplied path.
use std::{
    io::{self, Read},
    os::fd::AsRawFd,
    path::Path,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const MAIN: &str = "refs/heads/main";
const MAX_REPLY: u64 = 256;
const DEADLINE: Duration = Duration::from_millis(450);

// A single ref iteration samples the ref name, direct/symbolic status and
// object type together and handles both loose and packed refs. A pattern can
// also match ref descendants; require an exact full-name and field match.
fn parse_observation(bytes: &[u8]) -> Option<&str> {
    let mut fields = str::from_utf8(bytes).ok()?.strip_suffix('\n')?.split('\t');
    if fields.next()? != MAIN {
        return None;
    }
    let oid = fields.next()?;
    (matches!(oid.len(), 40 | 64)
        && oid.bytes().all(|b| matches!(b,b'0'..=b'9'|b'a'..=b'f'))
        && oid.bytes().any(|b| b != b'0')
        && fields.next() == Some("commit")
        && fields.next() == Some("")
        && fields.next().is_none())
    .then_some(oid)
}

pub(super) fn still_at(repo: &str, expected: &str) -> bool {
    direct_main(repo).as_deref() == Some(expected)
}

pub(crate) fn direct_main(repo: &str) -> Option<String> {
    if !Path::new(repo).is_absolute() {
        return None;
    }
    let mut cmd = Command::new("/usr/bin/git");
    cmd.env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .arg("--git-dir")
        .arg(repo)
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "protocol.allow=never",
            "for-each-ref",
            "--count=1",
            "--format=%(refname)%09%(objectname)%09%(objecttype)%09%(symref)",
            MAIN,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = cmd.spawn().ok()?;
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return None;
                }
                let stdout = child.stdout.take()?;
                let fd = stdout.as_raw_fd();
                // A retained writer in a descendant must not make the observer wait.
                let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
                if flags < 0
                    || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
                {
                    return None;
                }
                let mut bytes = Vec::new();
                stdout.take(MAX_REPLY + 1).read_to_end(&mut bytes).ok()?;
                return (bytes.len() <= MAX_REPLY as usize)
                    .then(|| parse_observation(&bytes))
                    .flatten()
                    .map(str::to_owned);
            }
            Ok(None) if start.elapsed() < DEADLINE => thread::sleep(Duration::from_millis(5)),
            Err(error) if error.raw_os_error() == Some(libc::ECHILD) => return None,
            Err(error)
                if error.kind() == io::ErrorKind::Interrupted && start.elapsed() < DEADLINE =>
            {
                continue;
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_direct_main_commit_only() {
        for oid in ["e".repeat(40), "a".repeat(64)] {
            let line = format!("{MAIN}\t{oid}\tcommit\t\n");
            assert_eq!(parse_observation(line.as_bytes()), Some(oid.as_str()));
            for wrong in [
                format!("{MAIN}-other\t{oid}\tcommit\t\n"),
                format!("{MAIN}\t{oid}\tcommit\trefs/heads/alias\n"),
                format!("{MAIN}\t{oid}\ttag\t\n"),
                format!("{line}{line}"),
                line.trim_end_matches('\n').to_owned(),
            ] {
                assert!(parse_observation(wrong.as_bytes()).is_none());
            }
            assert!(
                parse_observation(line.replace(&oid, &oid.to_uppercase()).as_bytes()).is_none()
            );
            assert!(
                parse_observation(line.replace(&oid, &"0".repeat(oid.len())).as_bytes()).is_none()
            );
        }
    }
}
