//! Owner-signed promotion approval messages and offline Ed25519 verification.
//!
//! A signature authorizes one exact candidate preview and one request ID for at
//! most ten minutes. It does not itself execute anything; the Store consumes the
//! request ID atomically before the trusted CLI is invoked.

use ai4math_store::PromotionPreview;
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Write,
    os::unix::{
        fs::{MetadataExt, PermissionsExt},
        process::CommandExt,
    },
    path::Path,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const NAMESPACE: &str = "ai4math-owner-promotion-v1";
const SSH_NAMESPACE: &str = "ai4math-promotion";
const MAX_REMAINING_SECONDS: u64 = 600;
const MAX_PATH_BYTES: usize = 4096;
const MAX_SIGNATURE_BYTES: usize = 4096;

#[derive(Serialize)]
struct SigningMessage<'a> {
    schema: &'static str,
    action: &'static str,
    project_root: &'a str,
    repo: &'a str,
    task_id: &'a str,
    claim_id: &'a str,
    engine: &'a str,
    module_path: &'a str,
    target: &'a str,
    base_commit: &'a str,
    candidate_sha256: &'a str,
    cli_sha256: &'a str,
    lean_sha256: &'a str,
    request_id: &'a str,
    expires_unix: u64,
}

pub struct PromotionApproval {
    bytes: Vec<u8>,
    expires_unix: u64,
}

impl PromotionApproval {
    pub fn new(
        root: &Path,
        repo: &Path,
        preview: &PromotionPreview,
        cli_sha256: &str,
        lean_sha256: &str,
        request_id: &str,
        expires_unix: u64,
    ) -> Result<Self> {
        ensure!(
            !request_id.is_empty()
                && request_id.len() <= 128
                && request_id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-')),
            "invalid promotion request ID"
        );
        ensure!(expires_unix != 0, "expiry must be positive Unix seconds");
        ensure!(
            cli_sha256.len() == 64 && lean_sha256.len() == 64,
            "invalid trusted tool fingerprint"
        );
        let project_root = canonical_dir(root)?;
        let repo = canonical_dir(repo)?;
        let bytes = serde_json::to_vec(&SigningMessage {
            schema: NAMESPACE,
            action: "promote_task_result",
            project_root: &project_root,
            repo: &repo,
            task_id: &preview.task_id,
            claim_id: &preview.claim_id,
            engine: &preview.engine,
            module_path: &preview.module_path,
            target: &preview.target,
            base_commit: &preview.base_commit,
            candidate_sha256: &preview.candidate_sha256,
            cli_sha256,
            lean_sha256,
            request_id,
            expires_unix,
        })?;
        Ok(Self {
            bytes,
            expires_unix,
        })
    }

    pub fn signing_bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn message_sha256(&self) -> String {
        format!("{:x}", Sha256::digest(&self.bytes))
    }

    pub fn check_fresh(&self) -> Result<()> {
        let now = unix_seconds()?;
        ensure!(now < self.expires_unix, "promotion approval has expired");
        ensure!(
            self.expires_unix - now <= MAX_REMAINING_SECONDS,
            "promotion approval has more than 10 minutes remaining"
        );
        Ok(())
    }

    pub fn verify_fresh(&self, root: &Path, signature: &[u8]) -> Result<()> {
        ensure!(
            !signature.is_empty() && signature.len() <= MAX_SIGNATURE_BYTES,
            "invalid signature size"
        );
        self.check_fresh()?;
        let key = read_key(root)?;
        let dir = tempfile::Builder::new()
            .prefix("a4owner")
            .rand_bytes(4)
            .tempdir()?;
        let message_path = dir.path().join("message");
        let signature_path = dir.path().join("signature");
        let key_path = dir.path().join("key.pub");
        fs::write(&message_path, &self.bytes)?;
        fs::write(&signature_path, signature)?;
        let key_text = std::str::from_utf8(&key).context("Owner promotion key is not UTF-8")?;
        let token = key_text
            .split_ascii_whitespace()
            .nth(1)
            .context("missing Owner promotion key token")?;
        let allowed = format!("ai4math namespaces=\"{SSH_NAMESPACE}\" ssh-ed25519 {token}\n");
        fs::write(&key_path, allowed)?;
        for path in [&message_path, &signature_path, &key_path] {
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        }
        let mut child = Command::new("/usr/bin/ssh-keygen")
            .arg("-Y")
            .arg("verify")
            .arg("-f")
            .arg(&key_path)
            .arg("-I")
            .arg("ai4math")
            .arg("-n")
            .arg(SSH_NAMESPACE)
            .arg("-s")
            .arg(&signature_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .process_group(0)
            .spawn()
            .context("start ssh-keygen verification")?;
        {
            let mut stdin = child.stdin.take().context("verification stdin")?;
            stdin.write_all(&self.bytes)?;
        }
        let started = Instant::now();
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if started.elapsed() >= Duration::from_secs(3) {
                crate::resident_process::kill_and_reap(&mut child);
                anyhow::bail!("Owner promotion signature verification timed out");
            }
            thread::sleep(Duration::from_millis(10));
        };
        ensure!(
            status.success(),
            "Owner promotion signature verification failed"
        );
        self.check_fresh()?;
        Ok(())
    }
}

fn canonical_dir(path: &Path) -> Result<String> {
    ensure!(path.as_os_str().len() <= MAX_PATH_BYTES, "path is too long");
    let canonical = path.canonicalize().context("canonicalize promotion path")?;
    let text = canonical.to_str().context("promotion path is not UTF-8")?;
    ensure!(
        text.len() <= MAX_PATH_BYTES && canonical.is_dir(),
        "promotion path is not a directory"
    );
    Ok(text.to_owned())
}

fn read_key(root: &Path) -> Result<Vec<u8>> {
    let path = root.join(".runtime/owner-promotion.pub");
    let metadata = fs::symlink_metadata(&path).context("stat fixed Owner promotion key")?;
    ensure!(
        metadata.is_file() && metadata.nlink() == 1,
        "Owner promotion key must be a regular single-link file"
    );
    ensure!(
        metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o022 == 0,
        "unsafe Owner promotion key permissions"
    );
    let bytes = fs::read(&path)?;
    ensure!(
        bytes.len() <= MAX_SIGNATURE_BYTES,
        "Owner promotion key is too large"
    );
    let line = bytes.strip_suffix(b"\n").unwrap_or(&bytes);
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    let text = std::str::from_utf8(line).context("Owner promotion key is not UTF-8")?;
    let mut words = text.split_ascii_whitespace();
    ensure!(
        words.next() == Some("ssh-ed25519"),
        "Owner promotion key must be ssh-ed25519"
    );
    let key = words.next().context("missing Owner promotion key bytes")?;
    ensure!(
        key.len() == 68 && key.starts_with("AAAAC3NzaC1lZDI1NTE5AAAAI"),
        "invalid Owner promotion key encoding"
    );
    let _comment = words.next();
    ensure!(
        words.next().is_none(),
        "Owner promotion key must not carry options or extra fields"
    );
    Ok(bytes)
}

fn unix_seconds() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("clock before Unix epoch")?
        .as_secs())
}

pub fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write;
        write!(&mut out, "{byte:02x}").expect("writing to String cannot fail");
    }
    out
}

pub fn hex_decode(text: &str) -> Result<Vec<u8>> {
    ensure!(
        text.len().is_multiple_of(2) && !text.is_empty(),
        "invalid hex length"
    );
    let mut out = Vec::with_capacity(text.len() / 2);
    for pair in text.as_bytes().as_chunks::<2>().0 {
        let digit = |b: u8| match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            _ => None,
        };
        let hi = digit(pair[0]).context("invalid hex digit")?;
        let lo = digit(pair[1]).context("invalid hex digit")?;
        out.push((hi << 4) | lo);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signing_message_is_deterministic_and_hex_round_trips() -> Result<()> {
        let preview = PromotionPreview {
            schema: "ai4math-promotion-preview-v1",
            advisory_only: true,
            proof_verified: false,
            authorization_granted: false,
            task_id: "T1".into(),
            claim_id: "C1".into(),
            engine: "lean4".into(),
            module_path: "Demo.lean".into(),
            target: "demo".into(),
            base_commit: "a".repeat(40),
            current_head: "a".repeat(40),
            ledger_event_seq: 1,
            candidate_sha256: "b".repeat(64),
            preview_sha256: "c".repeat(64),
            proof_body_bytes: 6,
            head_matches_base: true,
            claim: ai4math_store::PromotionClaimSummary {
                stale: false,
                effective_stale: false,
                local_trust: "L0".into(),
                effective_trust: "L0".into(),
            },
            pending_promotion: false,
        };
        let root = tempfile::tempdir()?;
        let expiry = unix_seconds()? + 60;
        let approval = PromotionApproval::new(
            root.path(),
            root.path(),
            &preview,
            &"d".repeat(64),
            &"e".repeat(64),
            "REQ-1",
            expiry,
        )?;
        let replay = PromotionApproval::new(
            root.path(),
            root.path(),
            &preview,
            &"d".repeat(64),
            &"e".repeat(64),
            "REQ-1",
            expiry,
        )?;
        assert_eq!(approval.signing_bytes(), replay.signing_bytes());
        assert_eq!(
            hex_decode(&hex_encode(approval.signing_bytes()))?,
            approval.signing_bytes()
        );
        Ok(())
    }
}
