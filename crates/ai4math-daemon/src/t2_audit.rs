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

const SCHEMA_V1: &str = "ai4math.t2.resident-sample.v1";
const SCHEMA_V2: &str = "ai4math.t2.resident-sample.v2";
const SCHEMA_V3: &str = "ai4math.t2.resident-heartbeats.v3";

// Match the limits of the managed-heartbeat comparison worker.
const MAX_RAW_HEARTBEATS: u64 = (1 << 53) - 1;
const MAX_INCREASE_PERCENT: u64 = 1_000;

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

// These fields may be absent, but a present value must have the declared type.
// In particular, an explicit null cannot smuggle a v2 key into a v1 object.
fn present<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    input: D,
) -> Result<Option<T>, D::Error> {
    T::deserialize(input).map(Some)
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
    #[serde(default, deserialize_with = "present")]
    policy_sha256: Option<String>,
    #[serde(default, deserialize_with = "present")]
    compiler_sha256: Option<String>,
    #[serde(default, deserialize_with = "present")]
    exporter_sha256: Option<String>,
    #[serde(default, deserialize_with = "present")]
    checker_sha256: Option<String>,
    #[serde(default, deserialize_with = "present")]
    baseline_commit: Option<String>,
    #[serde(default, deserialize_with = "present")]
    min_delta_heartbeats_raw: Option<u64>,
    #[serde(default, deserialize_with = "present")]
    min_increase_percent: Option<u64>,
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
        let release = (
            self.policy_sha256.as_deref(),
            self.exporter_sha256.as_deref(),
            self.checker_sha256.as_deref(),
        );
        let heartbeats = (
            self.baseline_commit.as_deref(),
            self.min_delta_heartbeats_raw,
            self.min_increase_percent,
        );
        let timeout_max = match (
            self.version,
            self.compiler_sha256.as_deref(),
            release,
            heartbeats,
        ) {
            (1, None, (None, None, None), (None, None, None)) => 180_000,
            (
                2,
                Some(compiler),
                (Some(policy), Some(exporter), Some(checker)),
                (None, None, None),
            ) if [policy, compiler, exporter, checker]
                .into_iter()
                .all(valid_sha256) =>
            {
                270_000
            }
            (
                3,
                Some(compiler),
                (None, None, None),
                (Some(baseline), Some(delta), Some(percent)),
            ) if valid_sha256(compiler)
                && valid_oid(baseline)
                && baseline.len() == self.reviewed_main.len()
                && baseline != self.reviewed_main
                && (1..=MAX_RAW_HEARTBEATS).contains(&delta)
                && (1..=MAX_INCREASE_PERCENT).contains(&percent) =>
            {
                270_000
            }
            _ => anyhow::bail!("invalid resident audit version or pins"),
        };
        ensure!(
            valid_anchor(&self.anchor_module)
                && valid_oid(&self.reviewed_main)
                && self.cli_executable != self.native_executable
                && (1000..=3_600_000).contains(&self.interval_ms)
                && (100..=timeout_max).contains(&self.audit_timeout_ms)
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
                .and_then(|bytes| self.parse_summary(&bytes, after).ok())
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

    fn parse_summary(&self, bytes: &[u8], expected_seq: i64) -> Result<Summary> {
        let reviewed = &self.reviewed_main;
        match self.version {
            1 => SampleSummary::parse(bytes, reviewed, expected_seq, None).map(Summary::Sample),
            2 => SampleSummary::parse(
                bytes,
                reviewed,
                expected_seq,
                Some(
                    self.policy_sha256
                        .as_deref()
                        .context("missing policy pin")?,
                ),
            )
            .map(Summary::Sample),
            3 => HeartbeatSummary::parse(
                bytes,
                reviewed,
                expected_seq,
                self.baseline_commit
                    .as_deref()
                    .context("missing baseline pin")?,
                self.compiler_sha256
                    .as_deref()
                    .context("missing compiler pin")?,
            )
            .map(Summary::Heartbeats),
            _ => anyhow::bail!("invalid resident audit version"),
        }
    }

    fn command(&self, root: &Path) -> Command {
        let mut command = Command::new(&self.cli_executable);
        command
            .current_dir(root)
            .arg(match self.version {
                1 => "t2-audit",
                2 => "t2-release-check",
                3 => "t2-compare-managed-heartbeats",
                _ => unreachable!("validated resident audit version"),
            })
            .arg(root)
            .arg(&self.bare_repo)
            .arg(&self.anchor_module);
        if self.version == 3 {
            command.arg(
                self.baseline_commit
                    .as_deref()
                    .expect("validated baseline pin"),
            );
        }
        command
            .arg(&self.reviewed_main)
            .arg(&self.native_executable);
        match self.version {
            2 => {
                command.args(
                    [
                        &self.policy_sha256,
                        &self.compiler_sha256,
                        &self.exporter_sha256,
                        &self.checker_sha256,
                    ]
                    .into_iter()
                    .flatten(),
                );
            }
            3 => {
                command
                    .arg(
                        self.compiler_sha256
                            .as_deref()
                            .expect("validated compiler pin"),
                    )
                    .arg(
                        self.min_delta_heartbeats_raw
                            .expect("validated raw threshold")
                            .to_string(),
                    )
                    .arg(
                        self.min_increase_percent
                            .expect("validated percent threshold")
                            .to_string(),
                    );
            }
            _ => {}
        }
        command
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

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum Summary {
    Sample(SampleSummary),
    Heartbeats(HeartbeatSummary),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkerSampleSummary {
    schema: String,
    advisory_only: bool,
    reviewed_commit: String,
    ledger_event_seq: i64,
    compile_ms: u64,
    module_count: u64,
    debt_findings: u64,
    #[serde(default, deserialize_with = "present")]
    independent_checks: Option<u8>,
    #[serde(default, deserialize_with = "present")]
    policy_sha256: Option<String>,
}

// The public type cannot retain the worker policy pin.
#[derive(Debug, Serialize)]
struct SampleSummary {
    schema: String,
    advisory_only: bool,
    reviewed_commit: String,
    ledger_event_seq: i64,
    compile_ms: u64,
    module_count: u64,
    debt_findings: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    independent_checks: Option<u8>,
}

impl SampleSummary {
    fn parse(
        bytes: &[u8],
        reviewed_main: &str,
        expected_seq: i64,
        expected_policy: Option<&str>,
    ) -> Result<Self> {
        let ObjectOnly(summary): ObjectOnly<WorkerSampleSummary> = serde_json::from_slice(bytes)?;
        let audited = match (
            summary.schema.as_str(),
            expected_policy,
            summary.policy_sha256.as_deref(),
            summary.independent_checks,
        ) {
            (SCHEMA_V1, None, None, None) => summary.debt_findings <= 3072,
            (SCHEMA_V2, Some(expected), Some(policy), Some(1..=8)) => {
                policy == expected && summary.debt_findings == 0
            }
            _ => false,
        };
        ensure!(
            audited
                && summary.advisory_only
                && valid_oid(&summary.reviewed_commit)
                && summary.reviewed_commit == reviewed_main
                && summary.ledger_event_seq >= 0
                && summary.ledger_event_seq == expected_seq
                && summary.compile_ms <= 120_000
                && (1..=128).contains(&summary.module_count),
            "invalid resident audit summary"
        );
        Ok(Self {
            schema: summary.schema,
            advisory_only: summary.advisory_only,
            reviewed_commit: summary.reviewed_commit,
            ledger_event_seq: summary.ledger_event_seq,
            compile_ms: summary.compile_ms,
            module_count: summary.module_count,
            debt_findings: summary.debt_findings,
            independent_checks: summary.independent_checks,
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkerHeartbeatSummary {
    schema: String,
    advisory_only: bool,
    reviewed_commit: String,
    ledger_event_seq: i64,
    baseline_commit: String,
    compiler_sha256: String,
    regressed: bool,
    max_command_regressed: bool,
    total_commands_regressed: bool,
}

// The public observation does not retain the worker's baseline or compiler pin.
#[derive(Debug, Serialize)]
struct HeartbeatSummary {
    schema: String,
    advisory_only: bool,
    reviewed_commit: String,
    ledger_event_seq: i64,
    regressed: bool,
    max_command_regressed: bool,
    total_commands_regressed: bool,
}

impl HeartbeatSummary {
    fn parse(
        bytes: &[u8],
        reviewed_main: &str,
        expected_seq: i64,
        expected_baseline: &str,
        expected_compiler: &str,
    ) -> Result<Self> {
        let ObjectOnly(summary): ObjectOnly<WorkerHeartbeatSummary> =
            serde_json::from_slice(bytes)?;
        ensure!(
            summary.schema == SCHEMA_V3
                && summary.advisory_only
                && valid_oid(&summary.reviewed_commit)
                && summary.reviewed_commit == reviewed_main
                && summary.ledger_event_seq >= 0
                && summary.ledger_event_seq == expected_seq
                && valid_oid(&summary.baseline_commit)
                && summary.baseline_commit.len() == summary.reviewed_commit.len()
                && summary.baseline_commit != summary.reviewed_commit
                && summary.baseline_commit == expected_baseline
                && valid_sha256(&summary.compiler_sha256)
                && summary.compiler_sha256 == expected_compiler
                && summary.regressed
                    == (summary.max_command_regressed || summary.total_commands_regressed),
            "invalid resident heartbeat summary"
        );
        Ok(Self {
            schema: summary.schema,
            advisory_only: summary.advisory_only,
            reviewed_commit: summary.reviewed_commit,
            ledger_event_seq: summary.ledger_event_seq,
            regressed: summary.regressed,
            max_command_regressed: summary.max_command_regressed,
            total_commands_regressed: summary.total_commands_regressed,
        })
    }
}

fn valid_oid(oid: &str) -> bool {
    matches!(oid.len(), 40 | 64) && lowercase_hex(oid) && oid.bytes().any(|b| b != b'0')
}

fn valid_sha256(pin: &str) -> bool {
    pin.len() == 64 && lowercase_hex(pin)
}

fn lowercase_hex(value: &str) -> bool {
    value
        .bytes()
        .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
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
            "schema": SCHEMA_V1,
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
        let observation = SampleSummary::parse(&serde_json::to_vec(&sample)?, HEAD, 13, None)?;
        assert_eq!(
            serde_json::to_value(AuditState::new(
                Status::Observation {
                    observation: Summary::Sample(observation)
                },
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
                SampleSummary::parse(&serde_json::to_vec(&bad_sample)?, HEAD, 13, None).is_err(),
                "accepted {field}"
            );
        }
        let positional = serde_json::to_vec(&json!([SCHEMA_V1, true, HEAD, 13, 42, 1, 0]))?;
        assert!(SampleSummary::parse(&positional, HEAD, 13, None).is_err());
        let mut missing = summary();
        missing.as_object_mut().unwrap().remove("debt_findings");
        assert!(SampleSummary::parse(&serde_json::to_vec(&missing)?, HEAD, 13, None).is_err());
        let mut trailing = serde_json::to_vec(&summary())?;
        trailing.extend_from_slice(b"\n{}\n");
        assert!(SampleSummary::parse(&trailing, HEAD, 13, None).is_err());
        let mut duplicate = serde_json::to_vec(&summary())?;
        duplicate.pop();
        duplicate.extend_from_slice(b",\"module_count\":1}");
        assert!(SampleSummary::parse(&duplicate, HEAD, 13, None).is_err());
        Ok(())
    }

    fn release_summary() -> Value {
        let mut sample = summary();
        sample["schema"] = json!(SCHEMA_V2);
        sample["independent_checks"] = json!(8);
        sample["policy_sha256"] = json!("a".repeat(64));
        sample["debt_findings"] = json!(0);
        sample
    }

    #[test]
    fn release_protocol_is_exact_and_version_bound() -> Result<()> {
        let sample = release_summary();
        let policy = "a".repeat(64);
        let parse = |value: &Value| {
            SampleSummary::parse(&serde_json::to_vec(value)?, HEAD, 13, Some(&policy))
        };
        let observation = parse(&sample)?;
        let mut public = sample.clone();
        public.as_object_mut().unwrap().remove("policy_sha256");
        assert_eq!(serde_json::to_value(observation)?, public);

        assert!(parse(&summary()).is_err());
        assert!(SampleSummary::parse(&serde_json::to_vec(&sample)?, HEAD, 13, None).is_err());
        for field in ["policy_sha256", "independent_checks"] {
            let mut v1 = summary();
            for value in [Value::Null, sample[field].clone()] {
                v1[field] = value;
                assert!(SampleSummary::parse(&serde_json::to_vec(&v1)?, HEAD, 13, None).is_err());
            }
            let mut missing = sample.clone();
            missing.as_object_mut().unwrap().remove(field);
            assert!(parse(&missing).is_err());
        }
        for (field, bad) in [
            ("schema", json!(SCHEMA_V1)),
            ("policy_sha256", json!("b".repeat(64))),
            ("policy_sha256", Value::Null),
            ("independent_checks", Value::Null),
            ("independent_checks", json!(0)),
            ("independent_checks", json!(9)),
            ("independent_checks", json!(1.0)),
            ("independent_checks", json!("1")),
            ("debt_findings", json!(1)),
            ("advisory_only", json!(false)),
            ("compiler_sha256", json!("b".repeat(64))),
        ] {
            let mut invalid = sample.clone();
            invalid[field] = bad;
            assert!(
                parse(&invalid).is_err(),
                "accepted {field}: {}",
                invalid[field]
            );
        }
        let mut duplicate = serde_json::to_vec(&sample)?;
        duplicate.pop();
        duplicate.extend_from_slice(b",\"independent_checks\":1}");
        assert!(SampleSummary::parse(&duplicate, HEAD, 13, Some(&policy)).is_err());
        let mut lower_bound = sample;
        lower_bound["independent_checks"] = json!(1);
        assert!(parse(&lower_bound).is_ok());
        Ok(())
    }

    fn config_v1() -> Value {
        json!({
            "version": 1,
            "cli_executable": "/cli",
            "bare_repo": "/repo",
            "anchor_module": "Anchor.lean",
            "reviewed_main": HEAD,
            "native_executable": "/native",
            "interval_ms": 1000,
            "audit_timeout_ms": 180_000,
            "max_runs": 64
        })
    }

    fn validate(value: Value) -> Result<Config> {
        let ObjectOnly(config): ObjectOnly<Config> = serde_json::from_value(value)?;
        config.check_fields()?;
        Ok(config)
    }

    #[test]
    fn strict_config_fields_and_child_command() -> Result<()> {
        let document = config_v1();
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

    #[test]
    fn strict_release_config_and_child_command() -> Result<()> {
        let pins = [
            "a".repeat(64),
            "b".repeat(64),
            "c".repeat(64),
            "d".repeat(64),
        ];
        let fields = [
            "policy_sha256",
            "compiler_sha256",
            "exporter_sha256",
            "checker_sha256",
        ];
        let mut document = config_v1();
        document["version"] = json!(2);
        document["audit_timeout_ms"] = json!(270_000);
        for (field, value) in fields.into_iter().zip(&pins) {
            document[field] = json!(value);
        }
        let command = validate(document.clone())?.command(Path::new("/root"));
        assert_eq!(command.get_program(), "/cli");
        assert_eq!(command.get_current_dir(), Some(Path::new("/root")));
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            [
                "t2-release-check",
                "/root",
                "/repo",
                "Anchor.lean",
                HEAD,
                "/native",
                &pins[0],
                &pins[1],
                &pins[2],
                &pins[3],
                "--daemon-parent"
            ]
        );

        for field in fields {
            let mut missing = document.clone();
            missing.as_object_mut().unwrap().remove(field);
            assert!(validate(missing).is_err(), "accepted missing {field}");
            for bad in [
                Value::Null,
                json!(1),
                json!("a".repeat(63)),
                json!("A".repeat(64)),
                json!("g".repeat(64)),
            ] {
                let mut invalid = document.clone();
                invalid[field] = bad;
                assert!(validate(invalid).is_err(), "accepted malformed {field}");
            }
            for extra in [Value::Null, json!(&pins[0])] {
                let mut v1 = config_v1();
                v1[field] = extra;
                assert!(validate(v1).is_err(), "accepted {field} in v1");
            }
        }
        let mut duplicate = serde_json::to_vec(&document)?;
        duplicate.pop();
        duplicate.extend_from_slice(b",\"policy_sha256\":\"another pin\"}");
        assert!(serde_json::from_slice::<ObjectOnly<Config>>(&duplicate).is_err());
        let mut with_extra = document.clone();
        with_extra["release_policy"] = json!({});
        assert!(validate(with_extra).is_err());
        for timeout in [99, 270_001] {
            let mut invalid = document.clone();
            invalid["audit_timeout_ms"] = json!(timeout);
            assert!(validate(invalid).is_err());
        }
        document["audit_timeout_ms"] = json!(100);
        assert!(validate(document).is_ok());
        Ok(())
    }

    const BASELINE: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const COMPILER: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";

    fn config_v3() -> Value {
        let mut document = config_v1();
        document["version"] = json!(3);
        document["audit_timeout_ms"] = json!(270_000);
        document["baseline_commit"] = json!(BASELINE);
        document["compiler_sha256"] = json!(COMPILER);
        document["min_delta_heartbeats_raw"] = json!(MAX_RAW_HEARTBEATS);
        document["min_increase_percent"] = json!(MAX_INCREASE_PERCENT);
        document
    }

    #[test]
    fn strict_heartbeat_config() -> Result<()> {
        let document = config_v3();
        validate(document.clone())?;
        for field in [
            "baseline_commit",
            "compiler_sha256",
            "min_delta_heartbeats_raw",
            "min_increase_percent",
        ] {
            let mut missing = document.clone();
            missing.as_object_mut().unwrap().remove(field);
            assert!(validate(missing).is_err(), "accepted missing {field}");
            let mut null = document.clone();
            null[field] = Value::Null;
            assert!(validate(null).is_err(), "accepted null {field}");
            if field != "compiler_sha256" {
                let mut v1 = config_v1();
                v1[field] = document[field].clone();
                assert!(validate(v1).is_err(), "accepted {field} in v1");
            }
        }
        for (field, bad) in [
            ("version", json!(4)),
            ("policy_sha256", json!(COMPILER)),
            ("exporter_sha256", json!(COMPILER)),
            ("checker_sha256", json!(COMPILER)),
            ("baseline_commit", json!(HEAD)),
            ("baseline_commit", json!("b".repeat(64))),
            ("baseline_commit", json!("b".repeat(39))),
            ("baseline_commit", json!("B".repeat(40))),
            ("baseline_commit", json!("0".repeat(40))),
            ("compiler_sha256", json!("g".repeat(64))),
            ("compiler_sha256", json!("C".repeat(64))),
            ("compiler_sha256", json!("c".repeat(63))),
            ("audit_timeout_ms", json!(270_001)),
            ("min_delta_heartbeats_raw", json!(0)),
            ("min_delta_heartbeats_raw", json!(MAX_RAW_HEARTBEATS + 1)),
            ("min_delta_heartbeats_raw", json!(1.0)),
            ("min_delta_heartbeats_raw", json!("1")),
            ("min_increase_percent", json!(0)),
            ("min_increase_percent", json!(-1)),
            ("min_increase_percent", json!(MAX_INCREASE_PERCENT + 1)),
        ] {
            let mut invalid = document.clone();
            invalid[field] = bad;
            assert!(validate(invalid).is_err(), "accepted malformed {field}");
        }
        let mut sha256_repo = document;
        sha256_repo["baseline_commit"] = json!("b".repeat(64));
        sha256_repo["reviewed_main"] = json!("a".repeat(64));
        validate(sha256_repo)?;
        Ok(())
    }

    fn heartbeat_summary(max: bool, total: bool) -> Value {
        json!({
            "schema": SCHEMA_V3,
            "advisory_only": true,
            "reviewed_commit": HEAD,
            "ledger_event_seq": 13,
            "baseline_commit": BASELINE,
            "compiler_sha256": COMPILER,
            "regressed": max || total,
            "max_command_regressed": max,
            "total_commands_regressed": total,
        })
    }

    #[test]
    fn heartbeat_protocol_checks_every_key_and_drops_private_pins() -> Result<()> {
        let config = validate(config_v3())?;
        let parse = |value: &Value| config.parse_summary(&serde_json::to_vec(value)?, 13);
        for (max, total) in [(false, false), (true, false), (false, true), (true, true)] {
            let mut sample = heartbeat_summary(max, total);
            let observation = parse(&sample)?;
            let mut inconsistent = sample.clone();
            inconsistent["regressed"] = json!(!(max || total));
            assert!(parse(&inconsistent).is_err(), "accepted inconsistent OR");
            sample.as_object_mut().unwrap().remove("baseline_commit");
            sample.as_object_mut().unwrap().remove("compiler_sha256");
            assert_eq!(serde_json::to_value(observation)?, sample);
        }
        let sample = heartbeat_summary(true, false);
        for field in sample.as_object().unwrap().keys() {
            let mut missing = sample.clone();
            missing.as_object_mut().unwrap().remove(field);
            assert!(parse(&missing).is_err(), "accepted missing {field}");
        }
        for (field, bad) in [
            ("schema", json!(SCHEMA_V2)),
            ("advisory_only", json!(false)),
            ("reviewed_commit", json!(BASELINE)),
            ("ledger_event_seq", json!(12)),
            ("ledger_event_seq", json!(-1)),
            ("ledger_event_seq", json!(13.0)),
            ("baseline_commit", json!("a".repeat(40))),
            ("compiler_sha256", json!("a".repeat(64))),
            ("max_command_regressed", json!(1)),
            ("total_commands_regressed", Value::Null),
            ("repo_path", json!("private-path")),
            ("claim", json!("private-claim")),
            ("total_command_heartbeats_raw", json!(123)),
            ("min_increase_percent", json!(100)),
            ("compile_ms", Value::Null),
        ] {
            let mut invalid = sample.clone();
            invalid[field] = bad;
            assert!(parse(&invalid).is_err(), "accepted invalid {field}");
        }
        let positional = json!([
            SCHEMA_V3, true, HEAD, 13, BASELINE, COMPILER, true, true, false
        ]);
        assert!(parse(&positional).is_err());
        let mut duplicate = serde_json::to_vec(&sample)?;
        duplicate.pop();
        duplicate.extend_from_slice(b",\"regressed\":true}");
        assert!(config.parse_summary(&duplicate, 13).is_err());
        let mut trailing = serde_json::to_vec(&sample)?;
        trailing.extend_from_slice(b"\n{}\n");
        assert!(config.parse_summary(&trailing, 13).is_err());

        let bytes = serde_json::to_vec(&sample)?;
        assert!(validate(config_v1())?.parse_summary(&bytes, 13).is_err());
        for other in [summary(), release_summary()] {
            assert!(parse(&other).is_err());
        }
        Ok(())
    }
}
