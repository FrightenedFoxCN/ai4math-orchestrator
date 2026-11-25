//! Opt-in operator sweep. The CLI, not this timer or its checkpoint, holds proof authority.
use crate::{
    resident_paths::{
        read_trusted_config, trusted_directory, trusted_executable, trusted_state_path,
    },
    resident_process::{POLL_INTERVAL, kill_and_reap},
    runtime::managed_wiki::direct_main,
};
use ai4math_store::{StateStore, T3SweepState, T3SweepStatus};
use anyhow::{Result, ensure};
use serde::{Deserialize, Deserializer};
use std::{
    collections::VecDeque,
    io,
    os::unix::{ffi::OsStrExt, process::CommandExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const MILLIS: std::ops::RangeInclusive<u64> = 100..=3_600_000;
const DEFAULT_TIMEOUT_MS: u64 = 120_000;
// Healthy but repeatedly rewound pages still advance revision. Limit work per daemon run.
const MAX_STEPS: usize = 256;

mod follow;

#[derive(Deserialize)]
#[serde(try_from = "ConfigDocument")]
pub struct Config {
    cli_executable: PathBuf,
    bare_repo: PathBuf,
    plans: Plans,
    native_executable: PathBuf,
    page_limit: usize,
    interval_ms: u64,
    step_timeout_ms: u64,
}

enum Plans {
    Single(String),
    RoundRobin(Vec<String>),
    AppliedCurrentMain,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigDocument {
    version: u8,
    cli_executable: PathBuf,
    bare_repo: PathBuf,
    #[serde(default, deserialize_with = "present")]
    plan_sha256: Option<String>,
    #[serde(default, deserialize_with = "present")]
    plan_sha256s: Option<Vec<String>>,
    #[serde(default, deserialize_with = "present")]
    selection: Option<String>,
    native_executable: PathBuf,
    page_limit: usize,
    interval_ms: u64,
    #[serde(default = "default_timeout_ms")]
    step_timeout_ms: u64,
}

// Missing fields default to None; a present null cannot hide an incompatible field.
fn present<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    deserializer: D,
) -> std::result::Result<Option<T>, D::Error> {
    T::deserialize(deserializer).map(Some)
}

fn default_timeout_ms() -> u64 {
    DEFAULT_TIMEOUT_MS
}

fn canonical_sha(hash: &str) -> bool {
    hash.len() == 64 && hash.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

impl TryFrom<ConfigDocument> for Config {
    type Error = anyhow::Error;

    fn try_from(doc: ConfigDocument) -> Result<Self> {
        let plans = match (
            doc.version,
            doc.plan_sha256,
            doc.plan_sha256s,
            doc.selection.as_deref(),
        ) {
            (1, Some(hash), None, None) if canonical_sha(&hash) => Plans::Single(hash),
            (2, None, Some(hashes), None)
                if (1..=8).contains(&hashes.len())
                    && hashes
                        .iter()
                        .enumerate()
                        .all(|(i, hash)| canonical_sha(hash) && !hashes[..i].contains(hash)) =>
            {
                Plans::RoundRobin(hashes)
            }
            (3, None, None, Some("applied_current_main")) => Plans::AppliedCurrentMain,
            _ => anyhow::bail!("invalid worker plan selection"),
        };
        ensure!(
            (1..=8).contains(&doc.page_limit)
                && MILLIS.contains(&doc.interval_ms)
                && MILLIS.contains(&doc.step_timeout_ms)
                && doc.cli_executable != doc.native_executable,
            "invalid worker configuration"
        );
        Ok(Self {
            cli_executable: doc.cli_executable,
            bare_repo: doc.bare_repo,
            plans,
            native_executable: doc.native_executable,
            page_limit: doc.page_limit,
            interval_ms: doc.interval_ms,
            step_timeout_ms: doc.step_timeout_ms,
        })
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        // Never include parser output, OS diagnostics or configured paths in a startup error.
        Self::read(path).map_err(|_| anyhow::anyhow!("invalid --t3-autosweep configuration"))
    }

    fn read(path: &Path) -> Result<Self> {
        let config: Self = read_trusted_config(path)?;
        config.check_paths()?;
        Ok(config)
    }

    fn check_paths(&self) -> Result<()> {
        trusted_directory(&self.bare_repo)?;
        for executable in [&self.cli_executable, &self.native_executable] {
            trusted_executable(executable)?;
        }
        Ok(())
    }

    pub fn start(self, root: PathBuf) {
        if thread::Builder::new()
            .name("ai4math-t3-autosweep".into())
            .spawn(move || self.run(&root).log())
            .is_err()
        {
            Stop::StepFailed.log();
        }
    }

    fn run(&self, root: &Path) -> Stop {
        let result = match &self.plans {
            Plans::Single(hash) => self.sweep_single(root, hash),
            Plans::RoundRobin(hashes) => self.sweep_multiple(root, hashes),
            Plans::AppliedCurrentMain => return follow::run(self, root),
        };
        match result {
            Ok(()) => Stop::Complete,
            Err(stop) => stop,
        }
    }

    fn sweep_single(&self, root: &Path, hash: &str) -> std::result::Result<(), Stop> {
        for step in 1..=MAX_STEPS {
            self.check_paths().map_err(|_| Stop::Unavailable)?;
            let before = self.progress(root, hash).map_err(|_| Stop::Unavailable)?;
            if before.status == T3SweepState::Complete {
                return Ok(());
            }
            let expected = next_revision(&before).ok_or(Stop::NoProgress)?;
            self.step(root, hash)?;
            if self.readback(root, hash, &before, expected)? == T3SweepState::Complete {
                return Ok(());
            }
            if step != MAX_STEPS {
                thread::sleep(Duration::from_millis(self.interval_ms));
            }
        }
        Err(Stop::StepLimit)
    }

    fn sweep_multiple(&self, root: &Path, hashes: &[String]) -> std::result::Result<(), Stop> {
        self.check_paths().map_err(|_| Stop::Unavailable)?;
        let state = self
            .open_selection_state(root)
            .map_err(|_| Stop::Unavailable)?;
        let repo = self.bare_repo.to_str().ok_or(Stop::Unavailable)?;
        let head = direct_main(repo).ok_or(Stop::Unavailable)?;
        // Admit only plans already applied when the supervisor starts, in config order.
        let mut startup_disabled = false;
        let candidates: Vec<_> = hashes
            .iter()
            .filter_map(|hash| match state.applied_t3_plan(hash) {
                Ok(plan) => plan
                    .filter(|plan| plan.reviewed_commit == head)
                    .map(|_| hash.as_str()),
                Err(_) => {
                    startup_disabled = true;
                    Stop::Unavailable.log_disabled();
                    None
                }
            })
            .collect();
        drop(state);
        let interval = Duration::from_millis(self.interval_ms);
        let mut next_step_at: Option<Instant> = None;
        round_robin(&candidates, |hash| {
            if let Some(at) = next_step_at.take() {
                thread::sleep(at.saturating_duration_since(Instant::now()));
            }
            self.check_paths().map_err(|_| Stop::Unavailable)?;
            let state = self
                .open_selection_state(root)
                .map_err(|_| Stop::Unavailable)?;
            let before = ready_progress(&state, hash, &head);
            drop(state);
            let before = match before {
                Ok(Some(before)) => before,
                Ok(None) => return Ok(Visit::Skip),
                Err(_) => return Ok(Visit::Disable(Stop::Unavailable)),
            };
            let Some(expected) = next_revision(&before) else {
                return Ok(Visit::Disable(Stop::NoProgress));
            };
            // Do not switch to another HEAD on drift. The child independently
            // validates the applied plan's reviewed HEAD under its own Git gate.
            if direct_main(repo).as_deref() != Some(head.as_str()) {
                return Err(Stop::HeadChanged);
            }
            let result = self
                .step(root, hash)
                .and_then(|()| self.readback(root, hash, &before, expected));
            next_step_at = Some(Instant::now() + interval);
            Ok(Visit::Attempted(result))
        })?;
        if startup_disabled {
            Err(Stop::PlanDisabled)
        } else {
            Ok(())
        }
    }

    fn open_selection_state(&self, root: &Path) -> Result<StateStore> {
        let state = self.open_bound_state(root)?;
        ensure!(
            state.promotion_intent()?.is_none() && state.pivot_intent()?.is_none(),
            "pending managed repository transaction"
        );
        Ok(state)
    }

    fn open_bound_state(&self, root: &Path) -> Result<StateStore> {
        let path = trusted_state_path(root)?;
        // Each observation uses a separate read-only connection, dropped before a child runs.
        let state = StateStore::open_readonly(path)?;
        let repo = state.managed_repo()?;
        ensure!(
            repo.as_deref().map(str::as_bytes) == Some(self.bare_repo.as_os_str().as_bytes()),
            "project is not bound to the supplied repository"
        );
        Ok(state)
    }

    fn progress(&self, root: &Path, hash: &str) -> Result<T3SweepStatus> {
        self.open_bound_state(root)?.t3_sweep_status(hash)
    }

    fn readback(
        &self,
        root: &Path,
        hash: &str,
        before: &T3SweepStatus,
        expected: i64,
    ) -> std::result::Result<T3SweepState, Stop> {
        let after = self.progress(root, hash).map_err(|_| Stop::Unavailable)?;
        // One readback, no retry. A successful exit alone says nothing about progress.
        if after.revision != Some(expected) || after.reviewed_commit != before.reviewed_commit {
            return Err(Stop::NoProgress);
        }
        Ok(after.status)
    }

    fn step(&self, root: &Path, hash: &str) -> std::result::Result<(), Stop> {
        let started = Instant::now();
        let mut child = Command::new(&self.cli_executable)
            .current_dir(root)
            .arg("t3-sweep-step")
            .arg(root)
            .arg(&self.bare_repo)
            .arg(hash)
            .arg(self.page_limit.to_string())
            .arg(&self.native_executable)
            .arg("--daemon-parent")
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .map_err(|_| Stop::StepFailed)?;
        // Keep the pipe writer independent of Child::wait, which would close child.stdin.
        // This writer's death tells the guarded CLI to kill its own process group.
        let Some(_parent_pipe) = child.stdin.take() else {
            kill_and_reap(&mut child);
            return Err(Stop::StepFailed);
        };
        let timeout = Duration::from_millis(self.step_timeout_ms);
        loop {
            match child.try_wait() {
                Ok(Some(status)) if status.success() => return Ok(()),
                Ok(Some(_)) => return Err(Stop::StepFailed),
                Ok(None) => (),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => (),
                // If an external reaper already took the child, its PID is no longer ours.
                Err(e) if e.raw_os_error() == Some(libc::ECHILD) => return Err(Stop::StepFailed),
                Err(_) => {
                    kill_and_reap(&mut child);
                    return Err(Stop::StepFailed);
                }
            }
            let remaining = timeout.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                kill_and_reap(&mut child);
                return Err(Stop::TimedOut);
            }
            thread::sleep(remaining.min(POLL_INTERVAL));
        }
    }
}

fn ready_progress(state: &StateStore, hash: &str, head: &str) -> Result<Option<T3SweepStatus>> {
    if !state
        .applied_t3_plan(hash)?
        .is_some_and(|plan| plan.reviewed_commit == head)
    {
        return Ok(None);
    }
    // The status read validates the immutable applied-plan scope as well as the checkpoint.
    let progress = state.t3_sweep_status(hash)?;
    Ok(
        (progress.reviewed_commit == head && progress.status != T3SweepState::Complete)
            .then_some(progress),
    )
}

fn next_revision(before: &T3SweepStatus) -> Option<i64> {
    before.revision.unwrap_or(0).checked_add(1)
}

enum Visit {
    Skip,
    Disable(Stop),
    Attempted(std::result::Result<T3SweepState, Stop>),
}

fn round_robin(
    hashes: &[&str],
    mut visit: impl FnMut(&str) -> std::result::Result<Visit, Stop>,
) -> std::result::Result<(), Stop> {
    let mut pending: VecDeque<_> = hashes.iter().copied().collect();
    let mut steps = 0;
    let mut disabled = false;
    while let Some(hash) = pending.pop_front() {
        match visit(hash)? {
            Visit::Skip => (),
            Visit::Disable(reason) => {
                disabled = true;
                reason.log_disabled();
            }
            Visit::Attempted(result) => {
                steps += 1;
                match result {
                    Ok(T3SweepState::Complete) => (),
                    Ok(_) => pending.push_back(hash),
                    Err(reason) => {
                        disabled = true;
                        reason.log_disabled();
                    }
                }
            }
        }
        if steps == MAX_STEPS && !pending.is_empty() {
            return Err(Stop::StepLimit);
        }
    }
    if disabled {
        Err(Stop::PlanDisabled)
    } else {
        Ok(())
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Stop {
    Complete,
    Unavailable,
    StepFailed,
    TimedOut,
    NoProgress,
    StepLimit,
    HeadChanged,
    NonForwardMain,
    HeadTransitionLimit,
    PlanDisabled,
}

impl Stop {
    fn reason(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Unavailable => "authorization or state unavailable",
            Self::StepFailed => "step failed",
            Self::TimedOut => "step timed out",
            Self::NoProgress => "checkpoint did not advance",
            Self::StepLimit => "startup step limit reached",
            Self::HeadChanged => "reviewed main changed or became unavailable",
            Self::NonForwardMain => "reviewed main did not advance by fast-forward",
            Self::HeadTransitionLimit => "startup reviewed main transition limit reached",
            Self::PlanDisabled => "one or more plans disabled",
        }
    }

    fn log(self) {
        eprintln!("ai4mathd t3 autosweep stopped: {}", self.reason());
    }

    fn log_disabled(self) {
        eprintln!("ai4mathd t3 autosweep plan disabled: {}", self.reason());
    }
}

pub fn verified_root(project: &Path) -> Result<PathBuf> {
    let result = (|| -> Result<PathBuf> {
        let root = project.canonicalize()?;
        trusted_directory(&root)?;
        Ok(root)
    })();
    result.map_err(|_| anyhow::anyhow!("invalid project root for --t3-autosweep"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn document(version: u8) -> Value {
        json!({
            "version": version,
            "cli_executable": "/cli",
            "bare_repo": "/repo",
            "native_executable": "/native",
            "page_limit": 1,
            "interval_ms": 100,
        })
    }

    #[test]
    fn selects_exact_version_shape_and_keeps_catalog_order() -> Result<()> {
        let alpha = "a".repeat(64);
        let mut single = document(1);
        single["plan_sha256"] = json!(alpha);
        let config: Config = serde_json::from_value(single.clone())?;
        assert!(matches!(config.plans, Plans::Single(hash) if hash == alpha));
        assert_eq!(config.step_timeout_ms, DEFAULT_TIMEOUT_MS);

        let mut multi = document(2);
        let ordered: Vec<_> = (0..8).rev().map(|i| i.to_string().repeat(64)).collect();
        multi["plan_sha256s"] = json!(ordered);
        multi["step_timeout_ms"] = json!(1_000);
        let config: Config = serde_json::from_value(multi.clone())?;
        assert!(matches!(config.plans, Plans::RoundRobin(hashes) if hashes == ordered));
        assert_eq!(config.step_timeout_ms, 1_000);

        for (valid, key) in [(&single, "plan_sha256s"), (&multi, "plan_sha256")] {
            for value in [Value::Null, valid["plan_sha256s"].clone(), json!(alpha)] {
                let mut incompatible = valid.clone();
                incompatible[key] = value;
                assert!(serde_json::from_value::<Config>(incompatible).is_err());
            }
        }
        for (key, value) in [
            ("version", json!(3)),
            ("version", json!(1)),
            ("restart", json!(false)),
            ("page_limit", json!(0)),
            ("interval_ms", json!(99)),
            ("step_timeout_ms", json!(3_600_001)),
        ] {
            let mut invalid = multi.clone();
            invalid[key] = value;
            assert!(serde_json::from_value::<Config>(invalid).is_err(), "{key}");
        }
        assert!(serde_json::from_value::<Config>(document(1)).is_err());
        assert!(serde_json::from_value::<Config>(document(2)).is_err());
        let duplicate = format!(
            "{},\"version\":2}}",
            single.to_string().trim_end_matches('}')
        );
        assert!(serde_json::from_str::<Config>(&duplicate).is_err());
        Ok(())
    }

    #[test]
    fn v3_requires_exact_selection_without_any_catalog_fields() -> Result<()> {
        let mut valid = document(3);
        valid["selection"] = json!("applied_current_main");
        assert!(matches!(
            serde_json::from_value::<Config>(valid.clone())?.plans,
            Plans::AppliedCurrentMain
        ));
        assert!(serde_json::from_value::<Config>(document(3)).is_err());
        for (key, value) in [
            ("selection", Value::Null),
            ("selection", json!("applied-current-main")),
            ("selection", json!(true)),
            ("plan_sha256", Value::Null),
            ("plan_sha256", json!("a".repeat(64))),
            ("plan_sha256s", Value::Null),
            ("plan_sha256s", json!(["a".repeat(64)])),
            ("page_limit", json!(9)),
            ("interval_ms", json!(99)),
            ("step_timeout_ms", json!(3_600_001)),
            ("native_executable", json!("/cli")),
        ] {
            let mut invalid = valid.clone();
            invalid[key] = value;
            assert!(serde_json::from_value::<Config>(invalid).is_err(), "{key}");
        }
        for (version, key, hash) in [
            (1, "plan_sha256", json!("a".repeat(64))),
            (2, "plan_sha256s", json!(["a".repeat(64)])),
        ] {
            for selection in [Value::Null, valid["selection"].clone()] {
                let mut invalid = document(version);
                invalid[key] = hash.clone();
                invalid["selection"] = selection;
                assert!(serde_json::from_value::<Config>(invalid).is_err());
            }
        }
        let duplicate = format!(
            "{},\"selection\":\"applied_current_main\"}}",
            valid.to_string().trim_end_matches('}')
        );
        assert!(serde_json::from_str::<Config>(&duplicate).is_err());
        Ok(())
    }

    #[test]
    fn rejects_invalid_v2_plan_arrays() -> Result<()> {
        let alpha = "a".repeat(64);
        let mut multi = document(2);
        multi["plan_sha256s"] = json!([alpha]);
        assert!(matches!(
            serde_json::from_value::<Config>(multi.clone())?.plans,
            Plans::RoundRobin(hashes) if hashes == [alpha.clone()]
        ));
        let oversized: Vec<_> = (0..9).map(|i| i.to_string().repeat(64)).collect();
        for invalid in [
            json!([]),
            json!(oversized),
            json!([alpha, alpha]),
            json!(["A".repeat(64)]),
            json!(["g".repeat(64)]),
            json!(["a".repeat(63)]),
            json!(["a".repeat(65)]),
            json!(alpha),
            json!(null),
        ] {
            multi["plan_sha256s"] = invalid;
            assert!(serde_json::from_value::<Config>(multi.clone()).is_err());
        }
        Ok(())
    }

    #[test]
    fn round_robin_isolates_failed_and_stationary_plans() {
        let hashes = ["z", "a", "skip", "b", "c", "unavailable"];
        let mut visits = Vec::new();
        let mut c_visits = 0;
        assert_eq!(
            round_robin(&hashes, |hash| {
                visits.push(hash.to_owned());
                let result = match hash {
                    "z" => Err(Stop::NoProgress),
                    "a" => Err(Stop::StepFailed),
                    "b" => Ok(T3SweepState::Complete),
                    "c" => {
                        c_visits += 1;
                        Ok(if c_visits == 2 {
                            T3SweepState::Complete
                        } else {
                            T3SweepState::Running
                        })
                    }
                    "skip" => return Ok(Visit::Skip),
                    _ => return Ok(Visit::Disable(Stop::Unavailable)),
                };
                Ok(Visit::Attempted(result))
            }),
            Err(Stop::PlanDisabled)
        );
        assert_eq!(visits, ["z", "a", "skip", "b", "c", "unavailable", "c"]);
    }

    #[test]
    fn round_robin_has_one_global_step_budget_and_propagates_drift() {
        let hashes = ["b", "a"];
        let mut steps = 0;
        assert_eq!(
            round_robin(&hashes, |hash| {
                assert_eq!(hash, hashes[steps % hashes.len()]);
                steps += 1;
                Ok(Visit::Attempted(Ok(T3SweepState::Running)))
            }),
            Err(Stop::StepLimit)
        );
        assert_eq!(steps, MAX_STEPS);
        let mut visits = 0;
        assert_eq!(
            round_robin(&hashes, |hash| {
                assert_eq!(hash, hashes[visits]);
                visits += 1;
                if visits == 1 {
                    Ok(Visit::Attempted(Ok(T3SweepState::Running)))
                } else {
                    Err(Stop::HeadChanged)
                }
            }),
            Err(Stop::HeadChanged)
        );
        assert_eq!(visits, 2);
    }
}
