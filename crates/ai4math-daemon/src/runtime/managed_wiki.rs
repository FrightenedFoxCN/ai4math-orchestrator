//! Bounded, read-only observation of the direct managed main ref. Callers
//! provide an operator-bound repository, never an RPC-supplied path.
use std::{
    io::{self, Read},
    os::fd::AsRawFd,
    path::Path,
    process::{Child, Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

const MAIN: &str = "refs/heads/main";
const MAX_REPLY: u64 = 256;
const DEADLINE: Duration = Duration::from_millis(450);
fn valid_oid(oid: &str) -> bool {
    matches!(oid.len(), 40 | 64)
        && oid.bytes().all(|b| matches!(b,b'0'..=b'9'|b'a'..=b'f'))
        && oid.bytes().any(|b| b != b'0')
}

// A single ref iteration samples the ref name, direct/symbolic status and
// object type together and handles both loose and packed refs. A pattern can
// also match ref descendants; require an exact full-name and field match.
fn parse_observation(bytes: &[u8]) -> Option<&str> {
    let mut fields = str::from_utf8(bytes).ok()?.strip_suffix('\n')?.split('\t');
    if fields.next()? != MAIN {
        return None;
    }
    let oid = fields.next()?;
    (valid_oid(oid)
        && fields.next() == Some("commit")
        && fields.next() == Some("")
        && fields.next().is_none())
    .then_some(oid)
}

pub(super) fn still_at(repo: &str, expected: &str) -> bool {
    direct_main(repo).as_deref() == Some(expected)
}

fn managed_git(repo: &str) -> Option<Command> {
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
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    Some(cmd)
}

pub(crate) fn direct_main(repo: &str) -> Option<String> {
    let mut child = managed_git(repo)?
        .args([
            "for-each-ref",
            "--count=1",
            "--format=%(refname)%09%(objectname)%09%(objecttype)%09%(symref)",
            MAIN,
        ])
        .stdout(Stdio::piped())
        .spawn()
        .ok()?;
    if !wait_bounded(&mut child)?.success() {
        return None;
    }
    let stdout = child.stdout.take()?;
    let fd = stdout.as_raw_fd();
    // A retained writer in a descendant must not make the observer wait.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return None;
    }
    let mut bytes = Vec::new();
    stdout.take(MAX_REPLY + 1).read_to_end(&mut bytes).ok()?;
    (bytes.len() <= MAX_REPLY as usize)
        .then(|| parse_observation(&bytes))
        .flatten()
        .map(str::to_owned)
}

/// Verify a strict ancestry step between two already verified direct-main commit IDs.
/// Read actual commit parents rather than local replace, graft or commit-graph metadata.
pub(crate) fn is_forward_main(repo: &str, previous: &str, current: &str) -> bool {
    if previous == current
        || !valid_oid(previous)
        || !valid_oid(current)
        || previous.len() != current.len()
    {
        return false;
    }
    let Some(mut command) = managed_git(repo) else {
        return false;
    };
    let Ok(mut child) = command
        .env("GIT_GRAFT_FILE", "/dev/null")
        .args([
            "-c",
            "core.commitGraph=false",
            "merge-base",
            "--is-ancestor",
            previous,
            current,
        ])
        .spawn()
    else {
        return false;
    };
    wait_bounded(&mut child).is_some_and(|status| status.success())
}

fn wait_bounded(child: &mut Child) -> Option<ExitStatus> {
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
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

    #[test]
    fn forward_main_checks_real_parents_and_ignores_local_grafts() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let repo = temp.path().join("managed.git");
        let init = Command::new("/usr/bin/git")
            .arg("init")
            .arg("--bare")
            .arg("--quiet")
            .arg(&repo)
            .status()?;
        assert!(init.success());
        let name = repo.to_str().unwrap();
        let git = |args: &[&str]| -> anyhow::Result<String> {
            let out = managed_git(name)
                .unwrap()
                .args(args)
                .env("GIT_AUTHOR_NAME", "Fixture")
                .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
                .env("GIT_COMMITTER_NAME", "Fixture")
                .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .output()?;
            anyhow::ensure!(
                out.status.success(),
                "fixture Git {:?}: {}",
                args,
                String::from_utf8_lossy(&out.stderr)
            );
            Ok(String::from_utf8(out.stdout)?.trim().into())
        };
        let tree = git(&["mktree"])?;
        let first = git(&["commit-tree", &tree, "-m", "first"])?;
        let child = git(&["commit-tree", &tree, "-p", &first, "-m", "child"])?;
        let other = git(&["commit-tree", &tree, "-m", "independent"])?;
        assert!(is_forward_main(name, &first, &child));
        assert!(!is_forward_main(name, &child, &first));
        assert!(!is_forward_main(name, &first, &first));
        assert!(!is_forward_main(name, &first, &other));
        std::fs::write(repo.join("info/grafts"), format!("{other} {first}\n"))?;
        // Confirm the repository-local graft would fool an ordinary ancestry probe.
        assert!(git(&["merge-base", "--is-ancestor", &first, &other]).is_ok());
        assert!(!is_forward_main(name, &first, &other));
        assert!(!is_forward_main(name, "--all", &other));
        Ok(())
    }
}
