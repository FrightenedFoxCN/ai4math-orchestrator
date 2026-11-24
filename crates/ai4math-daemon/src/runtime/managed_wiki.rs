//! Local, read-only observation of the direct managed main ref. Never accept an
//! RPC-supplied repo or OID and do not shell out for nonmatching ledger stamps.
use std::{
    io::Read,
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
fn parse_observation(bytes: &[u8], expected: &str) -> bool {
    if !matches!(expected.len(), 40 | 64)
        || !expected
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return false;
    }
    let Ok(text) = str::from_utf8(bytes) else {
        return false;
    };
    let Some(line) = text.strip_suffix('\n') else {
        return false;
    };
    let mut fields = line.split('\t');
    fields.next() == Some(MAIN)
        && fields.next() == Some(expected)
        && fields.next() == Some("commit")
        && fields.next() == Some("")
        && fields.next().is_none()
}

pub(super) fn still_at(repo: &str, expected: &str) -> bool {
    if !Path::new(repo).is_absolute() {
        return false;
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
    let Ok(mut child) = cmd.spawn() else {
        return false;
    };
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return false;
                }
                let Some(stdout) = child.stdout.take() else {
                    return false;
                };
                let mut bytes = Vec::new();
                return stdout.take(MAX_REPLY + 1).read_to_end(&mut bytes).is_ok()
                    && bytes.len() <= MAX_REPLY as usize
                    && parse_observation(&bytes, expected);
            }
            Ok(None) if start.elapsed() < DEADLINE => thread::sleep(Duration::from_millis(5)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
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
            assert!(parse_observation(line.as_bytes(), &oid));
            for wrong in [
                format!("{MAIN}-other\t{oid}\tcommit\t\n"),
                format!("{MAIN}\t{oid}\tcommit\trefs/heads/alias\n"),
                format!("{MAIN}\t{oid}\ttag\t\n"),
                format!("{line}{line}"),
                line.trim_end_matches('\n').to_owned(),
            ] {
                assert!(!parse_observation(wrong.as_bytes(), &oid));
            }
            assert!(!parse_observation(line.as_bytes(), &oid.to_uppercase()));
        }
    }
}
