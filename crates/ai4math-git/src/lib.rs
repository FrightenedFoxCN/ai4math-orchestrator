//! Shared managed-Git coordination for CLI and daemon control planes.
//!
//! The lock is deliberately narrower than a Git transaction API: it serializes
//! managed operations on one bare repository and provides a direct, verified
//! main HEAD. Higher-level CAS, publication and recovery remain in their domain
//! services.

use anyhow::{Context, Result, bail, ensure};
use std::{
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

const MAIN: &str = "refs/heads/main";
const LOCK_FILE: &str = "ai4math-promotion.lock";

pub struct ManagedGit {
    path: PathBuf,
    _lock: File,
}

impl ManagedGit {
    pub fn lock(repo: &Path) -> Result<Self> {
        Self::acquire(repo, false, true)
    }

    pub fn try_lock(repo: &Path) -> Result<Self> {
        Self::acquire(repo, true, true)
    }

    /// Observe only an already initialized managed repository.
    pub fn try_lock_existing(repo: &Path) -> Result<Self> {
        Self::acquire(repo, true, false)
    }

    fn acquire(repo: &Path, no_wait: bool, create: bool) -> Result<Self> {
        let path = repo.canonicalize()?;
        ensure!(
            text(git(&path, &["rev-parse", "--is-bare-repository"])?)? == "true",
            "managed operations require an operator-owned bare Git repository"
        );
        let mut options = OpenOptions::new();
        options.write(true).create(create).truncate(false);
        #[cfg(unix)]
        if !create {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        let lock = options.open(path.join(LOCK_FILE))?;
        if !create {
            let metadata = lock.metadata()?;
            ensure!(
                metadata.is_file(),
                "expected an existing regular Git coordination file"
            );
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                ensure!(
                    metadata.nlink() == 1
                        && metadata.uid() == unsafe { libc::geteuid() }
                        && metadata.mode() & 0o022 == 0,
                    "unsafe managed Git coordination file"
                );
            }
        }
        if no_wait {
            fs2::FileExt::try_lock_exclusive(&lock).context("managed Git coordination is busy")?;
        } else {
            fs2::FileExt::lock_exclusive(&lock)?;
        }
        Ok(Self { path, _lock: lock })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Physical ancestry with replace refs and commit-graph shortcuts disabled.
    /// A false result is a proven non-ancestor relation; an error stays unknown.
    pub fn is_ancestor(&self, ancestor: &str, descendant: &str) -> Result<bool> {
        ensure!(
            valid_commit(ancestor) && valid_commit(descendant),
            "invalid Git commit ID"
        );
        for commit in [ancestor, descendant] {
            let spec = format!("{commit}^{{commit}}");
            let found = text(git(self.path(), &["rev-parse", "--verify", &spec])?)?;
            ensure!(found == commit, "Git object is not the requested commit");
        }
        let mut command = command(self.path(), &[]);
        command.env("GIT_ALLOW_PROTOCOL", "");
        let output =
            ai4math_broker::git_policy::physical_ancestry(&mut command, ancestor, descendant)
                .output()
                .context("run physical Git ancestry")?;
        if output.status.code() == Some(1) {
            return Ok(false);
        }
        ensure!(output.status.success(), "physical Git ancestry failed");
        Ok(true)
    }

    /// Whether an exact workspace-relative path is tracked at a full commit.
    pub fn contains_tracked(&self, commit: &str, path: &str) -> Result<bool> {
        ensure!(valid_commit(commit), "invalid Git commit ID");
        let spec = format!("{commit}^{{commit}}");
        let found = text(git(self.path(), &["rev-parse", "--verify", &spec])?)?;
        ensure!(found == commit, "Git object is not the requested commit");
        let names = git(
            self.path(),
            &["ls-tree", "-r", "-z", "--name-only", &spec, "--", path],
        )?;
        ensure!(
            names.len() <= 1024 * 1024,
            "tracked-path query exceeded its bound"
        );
        Ok(names
            .split(|byte| *byte == 0)
            .any(|name| name == path.as_bytes()))
    }

    /// Return the direct main ref only after proving it names a full commit.
    pub fn main_head(&self) -> Result<String> {
        let args = ["symbolic-ref", "--no-recurse", "-q", MAIN];
        let out = run_git(self.path(), &args)?;
        if out.status.code() != Some(1) {
            ensure!(out.status.success(), "git symbolic-ref failed");
            bail!("expected a direct Git ref: {MAIN}");
        }
        let head = text(git(self.path(), &["rev-parse", "--verify", MAIN])?)?;
        ensure!(
            valid_commit(&head),
            "main does not name a full Git object ID"
        );
        let spec = format!("{head}^{{commit}}");
        let found = text(git(self.path(), &["rev-parse", "--verify", &spec])?)?;
        ensure!(found == head, "main does not name the requested commit");
        Ok(head)
    }
}

fn command(repo: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new("/usr/bin/git");
    cmd.env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_NO_LAZY_FETCH", "1")
        .arg("--git-dir")
        .arg(repo)
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.fsync=all",
        ])
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd
}

fn run_git(repo: &Path, args: &[&str]) -> Result<Output> {
    command(repo, args).output().context("start Git")
}

fn git(repo: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let out = run_git(repo, args)?;
    ensure!(
        out.status.success(),
        "git {}: {}",
        args[0],
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(out.stdout)
}

fn text(bytes: Vec<u8>) -> Result<String> {
    Ok(String::from_utf8(bytes)?.trim_end().to_owned())
}

fn valid_commit(commit: &str) -> bool {
    matches!(commit.len(), 40 | 64)
        && commit.bytes().any(|b| b != b'0')
        && commit
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git_ok(repo: &Path, args: &[&str]) -> Result<()> {
        git(repo, args)?;
        Ok(())
    }

    #[test]
    fn lock_and_main_head_share_the_cli_protocol() -> Result<()> {
        let dir = tempfile::tempdir()?;
        git_ok(dir.path(), &["init", "--bare", "--initial-branch=main"])?;
        let empty = git(dir.path(), &["hash-object", "-w", "-t", "tree", "--stdin"])?;
        let tree = String::from_utf8(empty)?.trim().to_owned();
        let commit = git(
            dir.path(),
            &[
                "-c",
                "user.name=AI4Math Test",
                "-c",
                "user.email=ai4math@localhost",
                "commit-tree",
                &tree,
                "-m",
                "initial",
            ],
        )?;
        let commit = String::from_utf8(commit)?.trim().to_owned();
        git_ok(dir.path(), &["update-ref", MAIN, &commit])?;
        let next = git(
            dir.path(),
            &[
                "-c",
                "user.name=AI4Math Test",
                "-c",
                "user.email=ai4math@localhost",
                "commit-tree",
                &tree,
                "-p",
                &commit,
                "-m",
                "next",
            ],
        )?;
        let next = String::from_utf8(next)?.trim().to_owned();
        git_ok(dir.path(), &["update-ref", MAIN, &next])?;
        let repo = ManagedGit::lock(dir.path())?;
        assert_eq!(repo.main_head()?, next);
        assert!(repo.is_ancestor(&commit, &next)?);
        assert!(!repo.is_ancestor(&next, &commit)?);
        assert!(ManagedGit::try_lock(dir.path()).is_err());
        Ok(())
    }
}
