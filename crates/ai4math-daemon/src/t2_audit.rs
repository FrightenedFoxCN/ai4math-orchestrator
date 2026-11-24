//! Opt-in, pinned resident audit. Its observations carry no proof authority.
use crate::{
    runtime::managed_wiki::direct_main,
    t3_discovery::capture_stdout,
    t3_worker::{read_trusted_config, trusted_directory, trusted_executable, trusted_state_path},
};
use ai4math_store::StateStore;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Deserializer, Serialize, de};
use std::{
    fmt,
    marker::PhantomData,
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

const SCHEMA: &str = "ai4math.t2.resident-sample.v1";

// Struct derives can also accept positional arrays; this protocol is object-only.
// Delegate the map to the derived decoder so duplicate and unknown keys still fail.
struct ObjectOnly<T>(T);

impl<'de, T: Deserialize<'de>> Deserialize<'de> for ObjectOnly<T> {
    fn deserialize<D: Deserializer<'de>>(input: D) -> Result<Self, D::Error> {
        struct Visitor<T>(PhantomData<T>);
        impl<'de, T: Deserialize<'de>> de::Visitor<'de> for Visitor<T> {
            type Value = ObjectOnly<T>;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a resident T2 JSON object")
            }
            fn visit_map<M: de::MapAccess<'de>>(self, map: M) -> Result<Self::Value, M::Error> {
                T::deserialize(de::value::MapAccessDeserializer::new(map)).map(ObjectOnly)
            }
        }
        input.deserialize_map(Visitor(PhantomData))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Config {
    version: u8,
    cli_executable: PathBuf,
    bare_repo: PathBuf,
    anchor_module: String,
    reviewed_main: String,
    native_executable: PathBuf,
    interval_ms: u64,
    audit_timeout_ms: u64,
    max_runs: u64,
}

pub(super) fn verified_root(project: &Path) -> Result<PathBuf> {
    let result = (|| -> Result<PathBuf> {
        let root = project.canonicalize()?;
        trusted_directory(&root)?;
        Ok(root)
    })();
    result.map_err(|_| anyhow::anyhow!("invalid project root for --t2-audit"))
}

impl Config {
    pub(super) fn load(path: &Path) -> Result<Self> {
        let result = (|| -> Result<Self> {
            let ObjectOnly(config): ObjectOnly<Self> = read_trusted_config(path)?;
            config.check_fields()?;
            config.check_executables()?;
            trusted_directory(&config.bare_repo)?;
            Ok(config)
        })();
        // Do not expose parser errors, OS diagnostics, or configured paths.
        result.map_err(|_| anyhow::anyhow!("invalid --t2-audit configuration"))
    }

    fn check_fields(&self) -> Result<()> {
        ensure!(
            self.version == 1
                && valid_anchor(&self.anchor_module)
                && valid_oid(&self.reviewed_main)
                && self.cli_executable != self.native_executable
                && (1000..=3_600_000).contains(&self.interval_ms)
                && (100..=180_000).contains(&self.audit_timeout_ms)
                && (1..=64).contains(&self.max_runs),
            "invalid resident audit fields"
        );
        Ok(())
    }

    fn check_executables(&self) -> Result<()> {
        trusted_executable(&self.cli_executable)?;
        trusted_executable(&self.native_executable)
    }

    pub(super) fn start(self, root: PathBuf, handle: Arc<Mutex<AuditState>>) {
        let worker_handle = Arc::clone(&handle);
        if thread::Builder::new()
            .name("ai4math-t2-audit".into())
            .spawn(move || self.run(&root, &worker_handle))
            .is_err()
        {
            AuditState::unavailable().publish(&handle);
        }
    }

    fn run(&self, root: &Path, handle: &Mutex<AuditState>) {
        let mut attempts = 0;
        loop {
            let status = self.sample(root, handle, &mut attempts);
            let exhausted = attempts == self.max_runs;
            let stop = exhausted || matches!(status, Status::HeadChanged);
            AuditState::new(status, attempts, exhausted).publish(handle);
            if stop {
                break;
            }
            // Sleep after each result (including failure), never schedule overlapping work.
            thread::sleep(Duration::from_millis(self.interval_ms));
        }
    }

    fn sample(&self, root: &Path, handle: &Mutex<AuditState>, attempts: &mut u64) -> Status {
        let Ok(before) = self.bound_ledger_seq(root) else {
            return Status::Unavailable;
        };
        let executables_ok = before.is_some() && self.check_executables().is_ok();
        if !self.head_matches() {
            return Status::HeadChanged;
        }
        let Some(before) = before else {
            return Status::Waiting;
        };
        if !executables_ok {
            return Status::Unavailable;
        }

        *attempts += 1;
        AuditState::new(Status::Running, *attempts, false).publish(handle);
        let started = Instant::now();
        let bytes = (|| {
            let mut child = self.command(root).spawn()?;
            capture_stdout(
                &mut child,
                started,
                Duration::from_millis(self.audit_timeout_ms),
            )
        })();

        // Sample the head even if the child failed. Any observed drift is terminal.
        let after = self.bound_ledger_seq(root);
        if !self.head_matches() {
            return Status::HeadChanged;
        }
        match after {
            Ok(None) => Status::Waiting,
            Ok(Some(after)) if after == before => bytes
                .ok()
                .and_then(|bytes| Summary::parse(&bytes, &self.reviewed_main, after).ok())
                .map_or(Status::Unavailable, |observation| Status::Observation {
                    observation,
                }),
            _ => Status::Unavailable,
        }
    }

    fn bound_ledger_seq(&self, root: &Path) -> Result<Option<i64>> {
        // Open independently for each sample. Neither connection survives child launch.
        let state = StateStore::open_readonly(trusted_state_path(root)?)?;
        let expected = self
            .bare_repo
            .to_str()
            .context("invalid managed repository path")?;
        match state.managed_wiki_stamp()? {
            Some((repo, seq)) => {
                ensure!(repo == expected && seq >= 0, "invalid managed ledger stamp");
                Ok(Some(seq))
            }
            None => {
                ensure!(
                    state.managed_repo()?.as_deref() == Some(expected),
                    "project is not bound to the configured repository"
                );
                // The repository is bound, so a withheld stamp denotes pending Git work.
                Ok(None)
            }
        }
    }

    fn head_matches(&self) -> bool {
        trusted_directory(&self.bare_repo).is_ok()
            && self.bare_repo.to_str().and_then(direct_main).as_deref()
                == Some(self.reviewed_main.as_str())
    }

    fn command(&self, root: &Path) -> Command {
        let mut command = Command::new(&self.cli_executable);
        command
            .current_dir(root)
            .arg("t2-audit")
            .arg(root)
            .arg(&self.bare_repo)
            .arg(&self.anchor_module)
            .arg(&self.reviewed_main)
            .arg(&self.native_executable)
            .arg("--daemon-parent")
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0);
        command
    }
}

#[derive(Debug, Serialize)]
pub(super) struct AuditState {
    #[serde(flatten)]
    status: Status,
    attempts: u64,
    exhausted: bool,
}

impl AuditState {
    pub(super) fn starting() -> Self {
        Self::new(Status::Starting, 0, false)
    }

    pub(super) fn unavailable() -> Self {
        Self::new(Status::Unavailable, 0, false)
    }

    fn new(status: Status, attempts: u64, exhausted: bool) -> Self {
        Self {
            status,
            attempts,
            exhausted,
        }
    }

    fn publish(self, handle: &Mutex<Self>) {
        *handle
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = self;
    }
}

#[derive(Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum Status {
    Starting,
    Running,
    Observation { observation: Summary },
    Unavailable,
    Waiting,
    HeadChanged,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Summary {
    schema: String,
    advisory_only: bool,
    reviewed_commit: String,
    ledger_event_seq: i64,
    compile_ms: u64,
    module_count: u64,
    debt_findings: u64,
}

impl Summary {
    fn parse(bytes: &[u8], reviewed_main: &str, expected_seq: i64) -> Result<Self> {
        let ObjectOnly(summary): ObjectOnly<Self> = serde_json::from_slice(bytes)?;
        ensure!(
            summary.schema == SCHEMA
                && summary.advisory_only
                && valid_oid(&summary.reviewed_commit)
                && summary.reviewed_commit == reviewed_main
                && summary.ledger_event_seq >= 0
                && summary.ledger_event_seq == expected_seq
                && summary.compile_ms <= 120_000
                && (1..=128).contains(&summary.module_count)
                && summary.debt_findings <= 3072,
            "invalid resident audit summary"
        );
        Ok(summary)
    }
}

fn valid_oid(oid: &str) -> bool {
    matches!(oid.len(), 40 | 64)
        && oid.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        && oid.bytes().any(|b| b != b'0')
}

fn valid_anchor(module: &str) -> bool {
    // The T2 audit supports only one managed root-level Lean anchor.
    module.len() <= 255
        && module.strip_suffix(".lean").is_some_and(|stem| {
            stem.starts_with(|c: char| c.is_ascii_uppercase())
                && stem.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    const HEAD: &str = "abcdefabcdefabcdefabcdefabcdefabcdefabcd";

    fn summary() -> Value {
        json!({
            "schema": SCHEMA,
            "advisory_only": true,
            "reviewed_commit": HEAD,
            "ledger_event_seq": 13,
            "compile_ms": 120_000,
            "module_count": 128,
            "debt_findings": 3072
        })
    }

    #[test]
    fn exact_status_and_observation_shapes() -> Result<()> {
        let sample = summary();
        let observation = Summary::parse(&serde_json::to_vec(&sample)?, HEAD, 13)?;
        assert_eq!(
            serde_json::to_value(AuditState::new(
                Status::Observation { observation },
                2,
                true
            ))?,
            json!({"status":"observation", "attempts":2, "exhausted":true, "observation":sample})
        );
        assert_eq!(
            serde_json::to_value(AuditState::starting())?,
            json!({"status":"starting", "attempts":0, "exhausted":false})
        );
        assert_eq!(
            serde_json::to_value(AuditState::unavailable())?,
            json!({"status":"unavailable", "attempts":0, "exhausted":false})
        );
        assert_eq!(
            serde_json::to_value(AuditState::new(Status::HeadChanged, 1, false))?,
            json!({"status":"head_changed", "attempts":1, "exhausted":false})
        );
        Ok(())
    }

    #[test]
    fn rejects_untrusted_wire_values() -> Result<()> {
        for (field, bad) in [
            ("schema", json!("wrong")),
            ("advisory_only", json!(false)),
            ("reviewed_commit", json!("a".repeat(40))),
            ("reviewed_commit", json!("A".repeat(40))),
            ("ledger_event_seq", json!(12)),
            ("ledger_event_seq", json!(-1)),
            ("compile_ms", json!(120_001)),
            ("compile_ms", json!(-1)),
            ("module_count", json!(0)),
            ("module_count", json!(129)),
            ("debt_findings", json!(3073)),
            ("debt_findings", json!(-1)),
            ("details", json!("private/path")),
        ] {
            let mut bad_sample = summary();
            bad_sample[field] = bad;
            assert!(
                Summary::parse(&serde_json::to_vec(&bad_sample)?, HEAD, 13).is_err(),
                "accepted {field}"
            );
        }
        let positional = serde_json::to_vec(&json!([SCHEMA, true, HEAD, 13, 42, 1, 0]))?;
        assert!(Summary::parse(&positional, HEAD, 13).is_err());
        let mut missing = summary();
        missing.as_object_mut().unwrap().remove("debt_findings");
        assert!(Summary::parse(&serde_json::to_vec(&missing)?, HEAD, 13).is_err());
        let mut trailing = serde_json::to_vec(&summary())?;
        trailing.extend_from_slice(b"\n{}\n");
        assert!(Summary::parse(&trailing, HEAD, 13).is_err());
        let mut duplicate = serde_json::to_vec(&summary())?;
        duplicate.pop();
        duplicate.extend_from_slice(b",\"module_count\":1}");
        assert!(Summary::parse(&duplicate, HEAD, 13).is_err());
        Ok(())
    }

    #[test]
    fn strict_config_fields_and_child_command() -> Result<()> {
        let document = json!({
            "version": 1,
            "cli_executable": "/cli",
            "bare_repo": "/repo",
            "anchor_module": "Anchor.lean",
            "reviewed_main": HEAD,
            "native_executable": "/native",
            "interval_ms": 1000,
            "audit_timeout_ms": 180_000,
            "max_runs": 64
        });
        let validate = |value: Value| -> Result<Config> {
            let ObjectOnly(config): ObjectOnly<Config> = serde_json::from_value(value)?;
            config.check_fields()?;
            Ok(config)
        };
        assert!(
            validate(json!([
                1,
                "/cli",
                "/repo",
                "Anchor.lean",
                HEAD,
                "/native",
                1000,
                180_000,
                64
            ]))
            .is_err()
        );
        let command = validate(document.clone())?.command(Path::new("/root"));
        assert_eq!(command.get_program(), "/cli");
        assert_eq!(command.get_current_dir(), Some(Path::new("/root")));
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            [
                "t2-audit",
                "/root",
                "/repo",
                "Anchor.lean",
                HEAD,
                "/native",
                "--daemon-parent"
            ]
        );
        let env: Vec<_> = command.get_envs().collect();
        assert_eq!(env.len(), 1);
        assert_eq!(env[0], ("PATH".as_ref(), Some("/usr/bin:/bin".as_ref())));

        let mut missing = document.clone();
        missing.as_object_mut().unwrap().remove("audit_timeout_ms");
        assert!(validate(missing).is_err());
        for (field, bad) in [
            ("version", json!(2)),
            ("anchor_module", json!("../Anchor.lean")),
            ("anchor_module", json!("anchor.lean")),
            ("anchor_module", json!("Anchor.agda")),
            ("reviewed_main", json!("0".repeat(64))),
            ("reviewed_main", json!("HEAD")),
            ("reviewed_main", json!("A".repeat(40))),
            ("native_executable", json!("/cli")),
            ("interval_ms", json!(999)),
            ("interval_ms", json!(3_600_001)),
            ("audit_timeout_ms", json!(99)),
            ("audit_timeout_ms", json!(180_001)),
            ("max_runs", json!(0)),
            ("max_runs", json!(65)),
            ("extra", json!(true)),
        ] {
            let mut invalid = document.clone();
            invalid[field] = bad;
            assert!(validate(invalid).is_err(), "accepted {field}");
        }
        Ok(())
    }
}
