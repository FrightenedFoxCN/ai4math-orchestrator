//! Opt-in observation only. This watcher publishes no authority and schedules no work.
use crate::t3_worker::{
    POLL_INTERVAL, check_state_file, kill_and_reap, read_trusted_config, trusted_directory,
    trusted_executable, trusted_metadata, trusted_state_path,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Deserializer, Serialize};
use std::{
    io::{self, Read},
    os::{fd::AsRawFd, unix::process::CommandExt},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

const INTERVAL_MS: std::ops::RangeInclusive<u64> = 100..=3_600_000;
const TIMEOUT_MS: std::ops::RangeInclusive<u64> = 100..=60_000;
const MAX_STDOUT_BYTES: usize = 4096;
// A managed snapshot has at most 128 modules (one of 129 tree slots is
// needed for a multi-module manifest). Both snapshots include the anchor.
const MAX_SNAPSHOT_MODULES: u64 = 128;
const MAX_CHANGED_MODULES: u64 = 2 * MAX_SNAPSHOT_MODULES - 1;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    version: u8,
    cli_executable: PathBuf,
    bare_repo: PathBuf,
    anchor_module: String,
    baseline_commit: String,
    interval_ms: u64,
    #[serde(default = "default_timeout_ms")]
    probe_timeout_ms: u64,
}

const fn default_timeout_ms() -> u64 {
    10_000
}

pub fn verified_root(project: &Path) -> Result<PathBuf> {
    let result = (|| {
        let root = project.canonicalize()?;
        trusted_directory(&root)?;
        Ok::<_, anyhow::Error>(root)
    })();
    result.map_err(|_| anyhow::anyhow!("invalid project root for --t3-discovery"))
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let result = (|| {
            let config: Self = read_trusted_config(path)?;
            config.validate()?;
            config.check_paths()?;
            Ok::<_, anyhow::Error>(config)
        })();
        // Configured paths and parser diagnostics never cross the startup boundary.
        result.map_err(|_| anyhow::anyhow!("invalid --t3-discovery configuration"))
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            self.version == 1
                && valid_anchor(&self.anchor_module)
                && valid_oid(&self.baseline_commit)
                && INTERVAL_MS.contains(&self.interval_ms)
                && TIMEOUT_MS.contains(&self.probe_timeout_ms),
            "invalid discovery configuration"
        );
        Ok(())
    }

    fn check_paths(&self) -> Result<()> {
        trusted_executable(&self.cli_executable)?;
        trusted_directory(&self.bare_repo)
    }

    pub fn start(self, root: PathBuf, handle: Arc<Mutex<DiscoveryState>>) {
        let worker_handle = Arc::clone(&handle);
        if thread::Builder::new()
            .name("ai4math-t3-discovery".into())
            .spawn(move || {
                loop {
                    let state = match self.probe(&root) {
                        Ok(summary) => DiscoveryState::Observation(summary),
                        Err(_) => DiscoveryState::Unavailable,
                    };
                    publish(&worker_handle, state);
                    // Wait after completion, including failure. Probes never overlap.
                    thread::sleep(Duration::from_millis(self.interval_ms));
                }
            })
            .is_err()
        {
            publish(&handle, DiscoveryState::Unavailable);
        }
    }

    fn probe(&self, root: &Path) -> Result<Summary> {
        self.check_paths()?;
        trusted_state_path(root)?;
        check_state_file(&trusted_metadata(&root.join(".runtime/primary_engine"))?)?;
        let started = Instant::now();
        let mut child = Command::new(&self.cli_executable)
            .current_dir(root)
            .arg("t3-discover")
            .arg(root)
            .arg(&self.bare_repo)
            .arg(&self.anchor_module)
            .arg(&self.baseline_commit)
            .arg("--daemon-parent")
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()?;
        let bytes = capture_stdout(
            &mut child,
            started,
            Duration::from_millis(self.probe_timeout_ms),
        )?;
        Summary::parse(&bytes, &self.baseline_commit)
    }
}

fn publish(handle: &Mutex<DiscoveryState>, state: DiscoveryState) {
    *handle
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = state;
}

#[derive(Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum DiscoveryState {
    Starting,
    Unavailable,
    #[serde(untagged)]
    Observation(Summary),
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Summary {
    status: Status,
    base_commit: String,
    observed_head: String,
    ledger_event_seq: i64,
    module_order_changed: bool,
    changed_module_count: u64,
    // Option ordinarily accepts missing fields. Here the wire must carry explicit null.
    #[serde(deserialize_with = "nullable_digest")]
    plan_sha256: Option<String>,
    advisory_only: bool,
}

fn nullable_digest<'de, D: Deserializer<'de>>(input: D) -> Result<Option<String>, D::Error> {
    Option::<String>::deserialize(input)
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum Status {
    Unchanged,
    StructuralChange,
    Diverged,
}

impl Summary {
    fn parse(bytes: &[u8], baseline_commit: &str) -> Result<Self> {
        let summary: Self = serde_json::from_slice(bytes)?;
        ensure!(
            summary.advisory_only
                && summary.base_commit == baseline_commit
                && summary.observed_head.len() == baseline_commit.len()
                && valid_oid(&summary.observed_head)
                && summary.ledger_event_seq >= 0
                && summary.changed_module_count <= MAX_CHANGED_MODULES
                && (summary.module_order_changed
                    || summary.changed_module_count <= MAX_SNAPSHOT_MODULES),
            "invalid discovery summary"
        );
        let different = summary.observed_head != baseline_commit;
        let changed = summary.changed_module_count != 0 || summary.module_order_changed;
        let consistent = match summary.status {
            Status::Unchanged => !changed && summary.plan_sha256.is_none(),
            Status::Diverged => different && !changed && summary.plan_sha256.is_none(),
            Status::StructuralChange => {
                different
                    && changed
                    && summary
                        .plan_sha256
                        .as_deref()
                        .is_some_and(|hash| hash.len() == 64 && lower_hex(hash))
            }
        };
        ensure!(consistent, "inconsistent discovery summary");
        Ok(summary)
    }
}

fn lower_hex(text: &str) -> bool {
    text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn valid_oid(oid: &str) -> bool {
    matches!(oid.len(), 40 | 64) && oid.bytes().any(|b| b != b'0') && lower_hex(oid)
}

fn valid_anchor(module: &str) -> bool {
    // Match CLI managed snapshots: only one approved root-level proof file name.
    module.len() <= 255
        && module.split_once('.').is_some_and(|(stem, ext)| {
            matches!(ext, "lean" | "agda" | "v")
                && stem.starts_with(|c: char| c.is_ascii_uppercase())
                && stem.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        })
}

fn capture_stdout(child: &mut Child, started: Instant, timeout: Duration) -> Result<Vec<u8>> {
    // Keep this writer outside Child; wait must not close it before the child exits.
    let lifeline = child.stdin.take();
    let mut may_signal = true;
    let result = (|| {
        ensure!(lifeline.is_some(), "missing discovery lifeline");
        let mut pipe = child
            .stdout
            .take()
            .context("missing discovery output pipe")?;
        set_nonblocking(&pipe)?;
        let mut bytes = Vec::with_capacity(MAX_STDOUT_BYTES);
        let mut buffer = [0u8; 1024];
        let mut eof = false;
        loop {
            ensure!(started.elapsed() < timeout, "discovery probe timed out");
            if !eof {
                let len = buffer.len().min(MAX_STDOUT_BYTES + 1 - bytes.len());
                match pipe.read(&mut buffer[..len]) {
                    Ok(0) => eof = true,
                    Ok(n) => {
                        ensure!(
                            bytes.len() + n <= MAX_STDOUT_BYTES,
                            "discovery output too large"
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
                        ensure!(status.success(), "discovery probe failed");
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
    use serde_json::{Value, json};

    const BASE: &str = "1234567890123456789012345678901234567890";
    const HEAD: &str = "abcdefabcdefabcdefabcdefabcdefabcdefabcd";
    const PLAN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn summary() -> Value {
        json!({
            "status": "structural_change",
            "base_commit": BASE,
            "observed_head": HEAD,
            "ledger_event_seq": 3,
            "module_order_changed": false,
            "changed_module_count": 1,
            "plan_sha256": PLAN,
            "advisory_only": true
        })
    }

    fn parse(value: &Value, baseline: &str) -> Result<Summary> {
        Summary::parse(&serde_json::to_vec(value)?, baseline)
    }

    #[test]
    fn exact_public_states_and_valid_summary_variants() -> Result<()> {
        assert_eq!(
            serde_json::to_value(DiscoveryState::Starting)?,
            json!({"status":"starting"})
        );
        let value = summary();
        let handle = Mutex::new(DiscoveryState::Starting);
        publish(&handle, DiscoveryState::Observation(parse(&value, BASE)?));
        assert_eq!(serde_json::to_value(&*handle.lock().unwrap())?, value);
        publish(&handle, DiscoveryState::Unavailable);
        assert_eq!(
            serde_json::to_value(&*handle.lock().unwrap())?,
            json!({"status":"unavailable"})
        );

        let mut reorder_only = summary();
        reorder_only["module_order_changed"] = json!(true);
        reorder_only["changed_module_count"] = json!(0);
        parse(&reorder_only, BASE)?;
        reorder_only["changed_module_count"] = json!(MAX_CHANGED_MODULES);
        parse(&reorder_only, BASE)?;
        reorder_only["changed_module_count"] = json!(MAX_CHANGED_MODULES + 1);
        assert!(parse(&reorder_only, BASE).is_err());

        let mut no_delta = summary();
        no_delta["changed_module_count"] = json!(0);
        no_delta["plan_sha256"] = Value::Null;
        no_delta["status"] = json!("diverged");
        parse(&no_delta, BASE)?;
        no_delta["status"] = json!("unchanged");
        parse(&no_delta, BASE)?;
        no_delta["observed_head"] = json!(BASE);
        parse(&no_delta, BASE)?;

        let sha256_base = "1".repeat(64);
        no_delta["base_commit"] = json!(sha256_base);
        no_delta["observed_head"] = json!("2".repeat(64));
        parse(&no_delta, &sha256_base)?;
        Ok(())
    }

    #[test]
    fn rejects_invalid_and_noncanonical_summaries() {
        for (key, bad) in [
            ("status", json!("unexpected")),
            ("status", json!("unchanged")),
            ("status", json!("diverged")),
            ("base_commit", json!(HEAD)),
            ("observed_head", json!(BASE)),
            ("observed_head", json!("a".repeat(64))),
            ("observed_head", json!("A".repeat(40))),
            ("observed_head", json!("0".repeat(40))),
            ("ledger_event_seq", json!(-1)),
            ("ledger_event_seq", json!(i64::MAX as u64 + 1)),
            ("changed_module_count", json!(0)),
            ("changed_module_count", json!(-1)),
            ("changed_module_count", json!(MAX_SNAPSHOT_MODULES + 1)),
            ("plan_sha256", Value::Null),
            ("plan_sha256", json!("a".repeat(63))),
            ("plan_sha256", json!("A".repeat(64))),
            ("advisory_only", json!(false)),
            ("unexpected", json!("secret path")),
        ] {
            let mut value = summary();
            value[key] = bad;
            assert!(parse(&value, BASE).is_err(), "accepted invalid {key}");
        }
        let mut value = summary();
        value["status"] = json!("unchanged");
        value["changed_module_count"] = json!(0);
        value.as_object_mut().unwrap().remove("plan_sha256");
        assert!(parse(&value, BASE).is_err());
        let mut frame = serde_json::to_vec(&summary()).unwrap();
        frame.extend_from_slice(b"\n{}\n");
        assert!(Summary::parse(&frame, BASE).is_err());
    }

    #[test]
    fn config_checks_version_anchor_commit_and_timings() -> Result<()> {
        let value = json!({
            "version": 1,
            "cli_executable": "/cli",
            "bare_repo": "/repo",
            "anchor_module": "Anchor.lean",
            "baseline_commit": BASE,
            "interval_ms": 100
        });
        let good: Config = serde_json::from_value(value.clone())?;
        good.validate()?;
        assert_eq!(good.probe_timeout_ms, 10_000);
        for (key, bad) in [
            ("version", json!(2)),
            ("anchor_module", json!("../Anchor.lean")),
            ("anchor_module", json!("anchor.lean")),
            ("anchor_module", json!("Anchor.txt")),
            ("baseline_commit", json!("HEAD")),
            ("baseline_commit", json!("0".repeat(40))),
            ("interval_ms", json!(99)),
            ("interval_ms", json!(3_600_001)),
            ("probe_timeout_ms", json!(99)),
            ("probe_timeout_ms", json!(60_001)),
            ("probe_timeout_ms", Value::Null),
            ("extra", json!(true)),
        ] {
            let mut malformed = value.clone();
            malformed[key] = bad;
            let accepted =
                serde_json::from_value::<Config>(malformed).is_ok_and(|c| c.validate().is_ok());
            assert!(!accepted, "accepted invalid {key}");
        }
        Ok(())
    }

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
        assert_eq!(error.to_string(), "discovery output too large");
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
        assert_eq!(error.to_string(), "discovery probe timed out");
        assert!(child.try_wait()?.is_some());
        Ok(())
    }
}
