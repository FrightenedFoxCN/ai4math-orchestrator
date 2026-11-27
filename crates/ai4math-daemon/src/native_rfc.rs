//! Trusted CLI adapter for native InfraRFC evidence production.
//!
//! The daemon never accepts a caller-selected executable, repository path or
//! proof body. It invokes the operator-configured CLI with fixed argv in a
//! private process group and returns only the CLI's structured RFC record.

use crate::resident_process::capture_stdout_limited;
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use sha2::Digest;
use std::{
    fs,
    os::unix::{fs::PermissionsExt, process::CommandExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

const MAX_OUTPUT: usize = 64 * 1024;

#[derive(Clone)]
pub struct Config {
    cli: PathBuf,
    lean: PathBuf,
    timeout: Duration,
}

impl Config {
    pub fn new(cli: PathBuf, lean: PathBuf, timeout_secs: u64) -> Result<Self> {
        ensure!(
            (10..=3600).contains(&timeout_secs),
            "native RFC timeout must be in 10..=3600 seconds"
        );
        for path in [&cli, &lean] {
            ensure!(path.is_absolute(), "native RFC tool paths must be absolute");
            let canonical = path
                .canonicalize()
                .context("canonicalize native RFC tool")?;
            let metadata = fs::metadata(&canonical).context("stat native RFC tool")?;
            ensure!(
                metadata.is_file() && metadata.permissions().mode() & 0o111 != 0,
                "native RFC tool must be an executable file"
            );
        }
        Ok(Self {
            cli,
            lean,
            timeout: Duration::from_secs(timeout_secs),
        })
    }

    pub fn fingerprints(&self) -> Result<(String, String)> {
        let digest = |path: &Path| -> Result<String> {
            let bytes = fs::read(path).context("read trusted tool for fingerprint")?;
            Ok(format!("{:x}", sha2::Sha256::digest(bytes)))
        };
        Ok((digest(&self.cli)?, digest(&self.lean)?))
    }

    fn command(
        &self,
        root: &Path,
        repo: &Path,
        action: &str,
        rfc_id: &str,
        reviewed: &str,
        digest: &str,
        lemmas: &[String],
    ) -> Command {
        let mut command = Command::new(&self.cli);
        command
            .arg(action)
            .arg(root)
            .arg(repo)
            .arg(rfc_id)
            .arg(reviewed)
            .arg(digest)
            .arg(&self.lean)
            .args(lemmas)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .current_dir("/")
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        command
    }

    fn run(&self, mut command: Command) -> Result<Value> {
        let started = Instant::now();
        let mut child = command.spawn().context("start native RFC CLI")?;
        let output = capture_stdout_limited(&mut child, started, self.timeout, MAX_OUTPUT)?;
        let value: Value =
            serde_json::from_slice(&output).context("decode native RFC CLI output")?;
        ensure!(value.is_object(), "native RFC CLI output must be an object");
        Ok(value)
    }

    pub fn freeze(
        &self,
        root: &Path,
        repo: &Path,
        rfc_id: &str,
        reviewed: &str,
        digest: &str,
        lemmas: &[String],
    ) -> Result<Value> {
        self.run(self.command(
            root,
            repo,
            "infra-rfc-freeze",
            rfc_id,
            reviewed,
            digest,
            lemmas,
        ))
    }

    pub fn promote_task_result(&self, root: &Path, repo: &Path, task_id: &str) -> Result<Value> {
        let mut command = Command::new(&self.cli);
        command
            .arg("promote-task-result")
            .arg(root)
            .arg(repo)
            .arg(task_id)
            .arg(&self.lean)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .current_dir("/")
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        self.run(command)
    }

    pub fn recover_promotion(&self, root: &Path, repo: &Path) -> Result<Value> {
        let mut command = Command::new(&self.cli);
        command
            .arg("recover-promotion")
            .arg(root)
            .arg(repo)
            .arg(&self.lean)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .current_dir("/")
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        self.run(command)
    }

    pub fn complete(
        &self,
        root: &Path,
        repo: &Path,
        rfc_id: &str,
        reviewed: &str,
        digest: &str,
    ) -> Result<Value> {
        self.run(self.command(
            root,
            repo,
            "infra-rfc-complete",
            rfc_id,
            reviewed,
            digest,
            &[],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_has_fixed_argv_and_no_inherited_environment() {
        let config = Config {
            cli: "/trusted/ai4math".into(),
            lean: "/trusted/lean".into(),
            timeout: Duration::from_secs(60),
        };
        let command = config.command(
            Path::new("/project"),
            Path::new("/managed/repo.git"),
            "infra-rfc-freeze",
            "RFC-1",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            &["demo_eq".into()],
        );
        let args: Vec<_> = command.get_args().collect();
        assert_eq!(
            args,
            [
                "infra-rfc-freeze",
                "/project",
                "/managed/repo.git",
                "RFC-1",
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                "/trusted/lean",
                "demo_eq",
            ]
        );
        assert_eq!(command.get_current_dir(), Some(Path::new("/")));
        assert_eq!(command.get_envs().count(), 1);
    }
}
