//! Local, offline verification of a detached signature for one Archive Breaker
//! approval. This does not track consumption, revalidate Git/ledger state,
//! grant authority to apply anything, or attest that a human used the key.
#[cfg(not(unix))]
use anyhow::bail;
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

pub(crate) const NAMESPACE: &str = "ai4math-owner-archive-v1";
const MAX_PATH_BYTES: usize = 4096;
const MAX_REQUEST_BYTES: usize = 128;
const MAX_SIGNATURE_BYTES: usize = 4 * 1024;
const MAX_KEY_BYTES: usize = 4 * 1024;
const MAX_REMAINING_SECONDS: u64 = 10 * 60;

/// All fields must describe the expected local operation, not merely echo an
/// untrusted submission. The caller checks current Git/ledger state, takes
/// `adr_sha256` from the expected canonical Pivot plan hash (`ADR-PIVOT` suffix),
/// not from human-readable Markdown, and is responsible for atomic consumption.
pub(crate) struct ArchiveApproval {
    bytes: Vec<u8>,
    project_root: String,
    expires_unix: u64,
}

#[derive(Serialize)]
struct SigningMessage<'a> {
    schema: &'static str,
    action: &'static str,
    project_root: &'a str,
    repo: &'a str,
    base_commit: &'a str,
    adr_sha256: &'a str,
    request_id: &'a str,
    expires_unix: u64,
}

impl ArchiveApproval {
    pub(crate) fn new(
        root: &Path,
        repo: &Path,
        base_commit: &str,
        adr_sha256: &str,
        request_id: &str,
        expires_unix: u64,
    ) -> Result<Self> {
        ensure!(
            matches!(base_commit.len(), 40 | 64) && lowercase_hex(base_commit),
            "base must be a complete lowercase Git object ID"
        );
        ensure!(
            adr_sha256.len() == 64 && lowercase_hex(adr_sha256),
            "ADR digest must be lowercase SHA-256"
        );
        ensure!(
            !request_id.is_empty()
                && request_id.len() <= MAX_REQUEST_BYTES
                && request_id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'),
            "request ID must be 1–128 ASCII letters, digits, underscores or hyphens"
        );
        ensure!(expires_unix != 0, "expiry must be positive Unix seconds");
        let project_root = canonical_dir(root, "project root")?;
        let repo = canonical_dir(repo, "repository")?;
        let bytes = serde_json::to_vec(&SigningMessage {
            schema: NAMESPACE,
            action: "archive_breaker",
            project_root: &project_root,
            repo: &repo,
            base_commit,
            adr_sha256,
            request_id,
            expires_unix,
        })?;
        Ok(Self {
            bytes,
            project_root,
            expires_unix,
        })
    }

    /// Compact UTF-8 JSON in struct field order, no terminal LF.
    pub(crate) fn signing_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Lowercase hexadecimal SHA-256 of the exact signed JSON bytes.
    pub(crate) fn message_sha256(&self) -> String {
        sha256_hex(self.signing_bytes())
    }

    /// Managed submission verification; a diagnostic path-loaded key cannot
    /// be used here. Check the full expiry window on both sides of verification.
    pub(crate) fn verify_fresh(&self, key: &ManagedOwnerKey, signature: &[u8]) -> Result<()> {
        self.verify_fresh_at(key, signature, unix_seconds)
    }

    fn verify_fresh_at(
        &self,
        key: &ManagedOwnerKey,
        signature: &[u8],
        mut now: impl FnMut() -> Result<u64>,
    ) -> Result<()> {
        ensure!(
            self.project_root == key.project_root,
            "trusted Owner public key belongs to a different managed root"
        );
        self.check_fresh_at(now()?)?;
        self.verify_signature(&key.key, signature)?;
        self.check_fresh_at(now()?)
    }

    /// Call as close to the durable submission as possible, even if
    /// `verify_fresh` already succeeded.
    pub(crate) fn check_fresh(&self) -> Result<()> {
        self.check_fresh_at(unix_seconds()?)
    }

    fn check_fresh_at(&self, now: u64) -> Result<()> {
        self.check_expiry(now)?;
        ensure!(
            self.expires_unix - now <= MAX_REMAINING_SECONDS,
            "Owner archive approval has more than 10 minutes remaining"
        );
        Ok(())
    }

    /// Checks against the caller's independently provisioned key. A verified
    /// message can still be replayed within its expiry until a separate
    /// consumer persists/atomically checks its request ID.
    pub(crate) fn verify(&self, trusted_key: &TrustedOwnerKey, signature: &[u8]) -> Result<()> {
        self.verify_at(trusted_key, signature, unix_seconds)?;
        // Never report success for an approval that expired during verification.
        self.check_expiry(unix_seconds()?)
    }

    fn verify_at(
        &self,
        trusted_key: &TrustedOwnerKey,
        signature: &[u8],
        now: impl FnOnce() -> Result<u64>,
    ) -> Result<()> {
        self.check_expiry(now()?)?;
        self.verify_signature(trusted_key, signature)
    }

    fn verify_signature(&self, trusted_key: &TrustedOwnerKey, signature: &[u8]) -> Result<()> {
        ensure!(
            !signature.is_empty() && signature.len() <= MAX_SIGNATURE_BYTES,
            "signature must be nonempty and at most 4 KiB"
        );
        #[cfg(unix)]
        return unix::verify(self.signing_bytes(), trusted_key, signature);
        #[cfg(not(unix))]
        {
            let _ = trusted_key;
            bail!("Owner archive signature verification is unavailable on this platform")
        }
    }

    fn check_expiry(&self, now: u64) -> Result<()> {
        ensure!(
            now < self.expires_unix,
            "Owner archive approval has expired"
        );
        Ok(())
    }
}

fn unix_seconds() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_secs())
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn lowercase_hex(s: &str) -> bool {
    s.bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn canonical_dir(path: &Path, name: &str) -> Result<String> {
    ensure!(
        path.as_os_str().len() <= MAX_PATH_BYTES,
        "{name} path is too long"
    );
    let canonical = path
        .canonicalize()
        .with_context(|| format!("resolve {name}"))?;
    let text = canonical.to_str().context("canonical path is not UTF-8")?;
    ensure!(
        text.len() <= MAX_PATH_BYTES && canonical.is_dir(),
        "canonical {name} is not a directory or is too long"
    );
    Ok(text.to_owned())
}

/// One manually provisioned plain OpenSSH Ed25519 public key. No trust path,
/// allowed-signers syntax, principals or options are read from an approval.
pub(crate) struct TrustedOwnerKey(String);

/// A key admitted exclusively from the fixed trust anchor of one managed root.
pub(crate) struct ManagedOwnerKey {
    key: TrustedOwnerKey,
    project_root: String,
}

impl ManagedOwnerKey {
    /// Lowercase hexadecimal SHA-256 of the canonical ASCII SSH base64 token,
    /// not the comment, algorithm prefix, or decoded wire bytes.
    pub(crate) fn fingerprint(&self) -> String {
        sha256_hex(self.key.0.as_bytes())
    }
}

impl TrustedOwnerKey {
    /// Diagnostic-only key loading: not trusted for managed submissions.
    pub(crate) fn read(path: &Path) -> Result<Self> {
        Self::parse(&read_bounded(path, MAX_KEY_BYTES).context("read trusted Owner public key")?)
    }

    /// Load a deployer-provisioned trust anchor at `.runtime/owner-archive.pub`.
    /// The managed root must already be canonical; no override or fallback exists.
    pub(crate) fn read_managed(root: &Path) -> Result<ManagedOwnerKey> {
        #[cfg(unix)]
        {
            let project_root = root.to_str().context("managed root is not UTF-8")?;
            ensure!(
                project_root.len() <= MAX_PATH_BYTES,
                "managed root path is too long"
            );
            let bytes = managed_anchor::read(root)
                .context("read fixed Owner public key at .runtime/owner-archive.pub")?;
            Ok(ManagedOwnerKey {
                key: Self::parse(&bytes)?,
                project_root: project_root.to_owned(),
            })
        }
        #[cfg(not(unix))]
        {
            let _ = root;
            bail!("Managed Owner trust anchors are unavailable on this platform")
        }
    }

    fn parse(bytes: &[u8]) -> Result<Self> {
        ensure!(
            bytes.len() <= MAX_KEY_BYTES,
            "trusted public key exceeds 4 KiB"
        );
        let line = bytes.strip_suffix(b"\n").unwrap_or(bytes);
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let line = std::str::from_utf8(line).context("public key is not UTF-8")?;
        ensure!(
            !line.chars().any(|c| c.is_control() && c != '\t'),
            "trusted public key must be a single line"
        );
        let mut words = line.split_ascii_whitespace();
        ensure!(
            words.next() == Some("ssh-ed25519"),
            "trusted public key must be a plain ssh-ed25519 key"
        );
        let key = words.next().context("missing trusted public key bytes")?;
        // The plain Ed25519 wire format is exactly 51 bytes: the key type and
        // 32 key bytes, both prefixed by a four-byte big-endian length.
        ensure!(
            key.len() == 68
                && key.starts_with("AAAAC3NzaC1lZDI1NTE5AAAAI")
                && matches!(key.as_bytes()[25], b'A'..=b'P')
                && key
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/'),
            "invalid OpenSSH Ed25519 public key encoding"
        );
        // Only the strict key token is used; an optional comment is discarded.
        Ok(Self(key.to_owned()))
    }
}

pub(crate) fn read_signature(path: &Path) -> Result<Vec<u8>> {
    read_bounded(path, MAX_SIGNATURE_BYTES).context("read detached Owner archive signature")
}

fn read_bounded(path: &Path, cap: usize) -> Result<Vec<u8>> {
    ensure!(
        path.as_os_str().len() <= MAX_PATH_BYTES,
        "approval input path is too long"
    );
    #[cfg(unix)]
    return unix::read_bounded(path, cap);
    #[cfg(not(unix))]
    {
        let _ = cap;
        bail!("Owner archive signature verification is unavailable on this platform")
    }
}

#[cfg(unix)]
mod managed_anchor;

#[cfg(unix)]
mod unix {
    use super::{NAMESPACE, TrustedOwnerKey};
    use anyhow::{Context, Result, bail, ensure};
    use std::{
        fs::{self, OpenOptions, Permissions},
        io::{Read, Seek, Write},
        os::unix::{
            fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
            process::CommandExt,
        },
        path::Path,
        process::{Child, Command, ExitStatus, Stdio},
        thread,
        time::{Duration, Instant},
    };

    pub(super) const SSH_KEYGEN: &str = "/usr/bin/ssh-keygen";
    const PRINCIPAL: &str = "ai4math-owner";
    const VERIFY_TIMEOUT: Duration = Duration::from_secs(3);

    pub(super) fn read_bounded(path: &Path, cap: usize) -> Result<Vec<u8>> {
        let before = fs::symlink_metadata(path).context("inspect approval input")?;
        ensure!(
            before.is_file() && before.len() <= cap as u64,
            "approval input must be a non-link regular file of at most {cap} bytes"
        );
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY)
            .open(path)
            .context("open approval input")?;
        let after = file.metadata()?;
        ensure!(
            after.is_file()
                && after.len() <= cap as u64
                && (before.dev(), before.ino()) == (after.dev(), after.ino()),
            "approval input changed during open, is not regular, or exceeds {cap} bytes"
        );
        let mut bytes = Vec::new();
        file.take(cap as u64 + 1).read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= cap, "approval input exceeds {cap} bytes");
        Ok(bytes)
    }

    fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?
            .write_all(bytes)?;
        Ok(())
    }

    pub(super) fn verify(message: &[u8], key: &TrustedOwnerKey, signature: &[u8]) -> Result<()> {
        // Use system /tmp, not TMPDIR or the repo; set mode at creation.
        let scratch = tempfile::Builder::new()
            .prefix("ai4math-owner-")
            .permissions(Permissions::from_mode(0o700))
            .tempdir_in("/tmp")
            .context("create private Owner verification directory")?;
        let allowed = format!(
            "{PRINCIPAL} namespaces=\"{NAMESPACE}\" ssh-ed25519 {}\n",
            key.0
        );
        write_private(&scratch.path().join("allowed_signers"), allowed.as_bytes())?;
        write_private(&scratch.path().join("signature"), signature)?;
        let mut stdin = tempfile::tempfile_in(scratch.path())?;
        stdin.write_all(message)?;
        stdin.rewind()?;

        let mut command = Command::new(SSH_KEYGEN);
        command
            .args([
                "-Y",
                "verify",
                "-f",
                "allowed_signers",
                "-I",
                PRINCIPAL,
                "-n",
                NAMESPACE,
                "-s",
                "signature",
            ])
            .current_dir(scratch.path())
            .env_clear()
            .stdin(Stdio::from(stdin))
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // SAFETY: only async-signal-safe POSIX calls and stack-only error capture
        // in the subprocess between fork and exec. Limits are child-only.
        unsafe { command.pre_exec(child_limits) };
        let mut child = command
            .spawn()
            .context("start system SSH signature verifier")?;
        ensure!(
            wait_bounded(&mut child, VERIFY_TIMEOUT)?.success(),
            "Owner archive signature did not verify with the trusted key"
        );
        Ok(())
    }

    fn child_limits() -> std::io::Result<()> {
        let set = |resource, value| {
            let limit = libc::rlimit {
                rlim_cur: value,
                rlim_max: value,
            };
            // Lowering a limit already below our maximum is unnecessary and
            // must not attempt to raise an inherited hard limit.
            let mut inherited = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            if unsafe { libc::getrlimit(resource, &mut inherited) } != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if inherited.rlim_max <= value {
                return Ok(());
            }
            let limit = libc::rlimit {
                rlim_cur: inherited.rlim_cur.min(value),
                ..limit
            };
            if unsafe { libc::setrlimit(resource, &limit) } != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        };
        // No controlling terminal, crash dump, unbounded regular-file output,
        // CPU time, or newly opened descriptors.
        if unsafe { libc::setsid() } < 0 {
            return Err(std::io::Error::last_os_error());
        }
        set(libc::RLIMIT_CORE, 0)?;
        set(libc::RLIMIT_FSIZE, 64 * 1024)?;
        set(libc::RLIMIT_CPU, 2)?;
        set(libc::RLIMIT_NOFILE, 32)?;
        Ok(())
    }

    pub(super) fn wait_bounded(child: &mut Child, timeout: Duration) -> Result<ExitStatus> {
        let start = Instant::now();
        loop {
            let elapsed = start.elapsed();
            if elapsed >= timeout {
                kill_and_reap(child)?;
                bail!("Owner archive signature verifier timed out");
            }
            match child.try_wait() {
                Ok(Some(status)) => return Ok(status),
                Ok(None) => thread::sleep(Duration::from_millis(10).min(timeout - elapsed)),
                Err(err) => {
                    kill_and_reap(child)?;
                    return Err(err).context("wait for Owner archive signature verifier");
                }
            }
        }
    }

    fn kill_and_reap(child: &mut Child) -> Result<()> {
        if let Err(err) = child.kill()
            && child.try_wait()?.is_none()
        {
            return Err(err).context("stop Owner archive signature verifier");
        }
        child
            .wait()
            .context("reap Owner archive signature verifier")?;
        Ok(())
    }
}

#[cfg(all(test, unix))]
mod tests;
