//! Compare caller-supplied native Lean heartbeat observations, not proofs or provenance.

use crate::t2_compare::{
    MAX_INCREASE_PERCENT, MAX_MODULES, MAX_SAMPLES, ModuleSample, Object, ObjectArray, median,
    validate_build_metadata, validate_reviewed_metadata,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

const HEARTBEAT_SCHEMA: &str = "ai4math.t2.lean-heartbeats.v1";
// Match the broker's native Lean heartbeat summary wire limits.
const MAX_COMMAND_COUNT: u64 = 1_000_000;
const MAX_RAW_HEARTBEATS: u64 = (1 << 53) - 1;

#[derive(Debug, Serialize)]
pub(crate) struct HeartbeatComparison {
    schema: &'static str,
    advisory_only: bool,
    pub regressed: bool,
    target_module: String,
    thresholds: Thresholds,
    baseline: GroupSummary,
    candidate: GroupSummary,
    target_source_changed: bool,
    target_prefix_changed: bool,
    max_command: MetricComparison,
    total_commands: MetricComparison,
}

#[derive(Debug, Serialize)]
struct Thresholds {
    min_delta_heartbeats_raw: u64,
    min_increase_percent: u64,
}

#[derive(Debug, Serialize)]
struct GroupSummary {
    sample_count: usize,
    reviewed_commit: String,
    source_set_sha256: String,
    compiler_sha256: String,
    command_count: u64,
}

#[derive(Debug, Serialize)]
struct MetricComparison {
    baseline_median_heartbeats_raw: u64,
    candidate_median_heartbeats_raw: u64,
    delta_heartbeats_raw: i64,
    status: MetricStatus,
    regressed: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum MetricStatus {
    Compared,
    BaselineZero,
}

impl MetricComparison {
    fn new(baseline: u64, candidate: u64, thresholds: &Thresholds) -> Self {
        // Values fit in i64; the positive relative gate uses exact cross-products.
        let delta = candidate as i64 - baseline as i64;
        let regressed = baseline != 0
            && delta > 0
            && delta as u64 >= thresholds.min_delta_heartbeats_raw
            && delta as u128 * 100
                >= u128::from(baseline) * u128::from(thresholds.min_increase_percent);
        Self {
            baseline_median_heartbeats_raw: baseline,
            candidate_median_heartbeats_raw: candidate,
            delta_heartbeats_raw: delta,
            status: if baseline == 0 {
                MetricStatus::BaselineZero
            } else {
                MetricStatus::Compared
            },
            regressed,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HeartbeatSample {
    schema: String,
    advisory_only: bool,
    reviewed_commit: String,
    ledger_event_seq: i64,
    build: Object<BuildSample>,
    profile: Object<ProfileSample>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BuildSample {
    source_set_sha256: String,
    compiler_sha256: String,
    compile_ms: u64,
    modules: ObjectArray<ModuleSample, MAX_MODULES>,
}

impl BuildSample {
    fn ordered_paths(&self) -> impl Iterator<Item = &str> {
        self.modules.0.iter().map(|module| module.path.as_str())
    }

    fn ordered_module_digests(&self) -> impl Iterator<Item = &str> {
        self.modules
            .0
            .iter()
            .map(|module| module.source_sha256.as_str())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfileSample {
    target_module: String,
    command_count: u64,
    max_command_heartbeats_raw: u64,
    total_command_heartbeats_raw: u64,
}

impl HeartbeatSample {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == HEARTBEAT_SCHEMA,
            "unexpected heartbeat schema"
        );
        ensure!(self.advisory_only, "advisory_only must be true");
        validate_reviewed_metadata(&self.reviewed_commit, self.ledger_event_seq)?;
        let build = &self.build.0;
        let profile = &self.profile.0;
        validate_build_metadata(
            &build.source_set_sha256,
            &build.compiler_sha256,
            build.compile_ms,
            &build.modules.0,
        )?;
        ensure!(
            build
                .ordered_paths()
                .any(|path| path == profile.target_module),
            "target_module must exactly match an ordered build module path"
        );
        ensure!(
            (1..=MAX_COMMAND_COUNT).contains(&profile.command_count),
            "command_count must be in 1..={MAX_COMMAND_COUNT}"
        );
        ensure!(
            profile.total_command_heartbeats_raw <= MAX_RAW_HEARTBEATS,
            "total_command_heartbeats_raw exceeds {MAX_RAW_HEARTBEATS}"
        );
        ensure!(
            profile.max_command_heartbeats_raw <= profile.total_command_heartbeats_raw
                && u128::from(profile.total_command_heartbeats_raw)
                    <= u128::from(profile.command_count)
                        * u128::from(profile.max_command_heartbeats_raw),
            "inconsistent max_command_heartbeats_raw and total_command_heartbeats_raw"
        );
        Ok(())
    }
}

fn parse_group(bytes: &[u8], group: &str) -> Result<Vec<HeartbeatSample>> {
    let ObjectArray(samples) =
        serde_json::from_slice::<ObjectArray<HeartbeatSample, MAX_SAMPLES>>(bytes)
            .with_context(|| format!("invalid {group} heartbeat JSON array"))?;
    ensure!(
        matches!(samples.len(), 3 | 5 | 7 | 9),
        "{group} must contain 3, 5, 7 or 9 reports"
    );
    let first = &samples[0];
    let first_build = &first.build.0;
    let first_profile = &first.profile.0;
    for (index, sample) in samples.iter().enumerate() {
        sample
            .validate()
            .with_context(|| format!("invalid {group} heartbeat report {}", index + 1))?;
        ensure!(
            sample.reviewed_commit == first.reviewed_commit,
            "reviewed_commit differs within the {group} group"
        );
        let build = &sample.build.0;
        let profile = &sample.profile.0;
        ensure!(
            build.source_set_sha256 == first_build.source_set_sha256,
            "source_set_sha256 differs within the {group} group"
        );
        ensure!(
            build.compiler_sha256 == first_build.compiler_sha256,
            "compiler_sha256 differs within the {group} group"
        );
        ensure!(
            build.ordered_paths().eq(first_build.ordered_paths()),
            "ordered module paths differ within the {group} group"
        );
        ensure!(
            build
                .ordered_module_digests()
                .eq(first_build.ordered_module_digests()),
            "ordered module digests differ within the {group} group"
        );
        ensure!(
            profile.target_module == first_profile.target_module,
            "target_module differs within the {group} group"
        );
        ensure!(
            profile.command_count == first_profile.command_count,
            "command_count differs within the {group} group"
        );
    }
    Ok(samples)
}

fn summarize(samples: &[HeartbeatSample]) -> GroupSummary {
    let first = &samples[0];
    GroupSummary {
        sample_count: samples.len(),
        reviewed_commit: first.reviewed_commit.clone(),
        source_set_sha256: first.build.0.source_set_sha256.clone(),
        compiler_sha256: first.build.0.compiler_sha256.clone(),
        command_count: first.profile.0.command_count,
    }
}

fn compare_metric(
    baseline: &[HeartbeatSample],
    candidate: &[HeartbeatSample],
    metric: impl Fn(&ProfileSample) -> u64,
    thresholds: &Thresholds,
) -> MetricComparison {
    MetricComparison::new(
        median(baseline.iter().map(|sample| metric(&sample.profile.0))),
        median(candidate.iter().map(|sample| metric(&sample.profile.0))),
        thresholds,
    )
}

pub(crate) fn validate_thresholds(min_delta: u64, min_percent: u64) -> Result<()> {
    ensure!(
        (1..=MAX_RAW_HEARTBEATS).contains(&min_delta),
        "min_delta_heartbeats_raw must be in 1..={MAX_RAW_HEARTBEATS}"
    );
    ensure!(
        (1..=MAX_INCREASE_PERCENT).contains(&min_percent),
        "min_increase_percent must be in 1..={MAX_INCREASE_PERCENT}"
    );
    Ok(())
}

pub(crate) fn compare(
    baseline: &[u8],
    candidate: &[u8],
    min_delta: u64,
    min_percent: u64,
) -> Result<HeartbeatComparison> {
    validate_thresholds(min_delta, min_percent)?;
    let thresholds = Thresholds {
        min_delta_heartbeats_raw: min_delta,
        min_increase_percent: min_percent,
    };
    let baseline = parse_group(baseline, "baseline")?;
    let candidate = parse_group(candidate, "candidate")?;
    let before = &baseline[0];
    let after = &candidate[0];
    let before_build = &before.build.0;
    let after_build = &after.build.0;
    let target_module = &before.profile.0.target_module;
    ensure!(
        before_build.compiler_sha256 == after_build.compiler_sha256,
        "baseline and candidate compiler_sha256 must match"
    );
    ensure!(
        before.reviewed_commit.len() == after.reviewed_commit.len(),
        "baseline and candidate reviewed_commit Git object ID widths must match"
    );
    ensure!(
        before_build.ordered_paths().eq(after_build.ordered_paths()),
        "baseline and candidate ordered module paths must match"
    );
    ensure!(
        *target_module == after.profile.0.target_module,
        "baseline and candidate target_module must match"
    );
    let same_module_digests = before_build
        .ordered_module_digests()
        .eq(after_build.ordered_module_digests());
    ensure!(
        (before_build.source_set_sha256 == after_build.source_set_sha256) == same_module_digests,
        "source_set_sha256 equality disagrees with per-module source_sha256 equality"
    );
    ensure!(
        before.reviewed_commit != after.reviewed_commit || same_module_digests,
        "identical reviewed_commit cannot have different module source digests"
    );

    // The native profiler sees just the ordered build prefix ending at the target.
    let target_index = before_build
        .ordered_paths()
        .position(|path| path == target_module)
        .context("missing target_module in the build prefix")?;
    let target_source_changed = before_build.modules.0[target_index].source_sha256
        != after_build.modules.0[target_index].source_sha256;
    let target_prefix_changed = !before_build
        .ordered_module_digests()
        .take(target_index + 1)
        .eq(after_build.ordered_module_digests().take(target_index + 1));
    ensure!(
        target_prefix_changed || before.profile.0.command_count == after.profile.0.command_count,
        "command_count differs for an identical target prefix"
    );

    let max_command = compare_metric(
        &baseline,
        &candidate,
        |profile| profile.max_command_heartbeats_raw,
        &thresholds,
    );
    let total_commands = compare_metric(
        &baseline,
        &candidate,
        |profile| profile.total_command_heartbeats_raw,
        &thresholds,
    );
    Ok(HeartbeatComparison {
        schema: "ai4math.t2.heartbeat-comparison.v1",
        advisory_only: true,
        regressed: max_command.regressed || total_commands.regressed,
        target_module: target_module.clone(),
        thresholds,
        baseline: summarize(&baseline),
        candidate: summarize(&candidate),
        target_source_changed,
        target_prefix_changed,
        max_command,
        total_commands,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn module(path: &str, hash: &str) -> Value {
        json!({"path": path, "source_sha256": hash.repeat(64), "compile_ms": 5})
    }

    fn sample(max: u64, total: u64) -> Value {
        json!({
            "schema": "ai4math.t2.lean-heartbeats.v1",
            "advisory_only": true,
            "reviewed_commit": "a".repeat(40),
            "ledger_event_seq": 0,
            "build": {
                "source_set_sha256": "b".repeat(64),
                "compiler_sha256": "c".repeat(64),
                "compile_ms": 25,
                "modules": [module("Support.lean", "d"), module("Target.lean", "e"), module("After.lean", "f")]
            },
            "profile": {
                "target_module": "Target.lean",
                "command_count": 10,
                "max_command_heartbeats_raw": max,
                "total_command_heartbeats_raw": total,
            }
        })
    }

    fn group(samples: &[Value]) -> Vec<u8> {
        serde_json::to_vec(samples).unwrap()
    }

    fn repeat(sample: &Value) -> Vec<u8> {
        group(&[sample.clone(), sample.clone(), sample.clone()])
    }

    #[test]
    fn freezes_report_schema_with_independent_odd_sample_medians() -> Result<()> {
        let mut a = sample(400, 600);
        let b = sample(100, 700);
        let mut c = sample(200, 500);
        // Ledger events are independent sanity checks; timing is not compared.
        a["ledger_event_seq"] = json!(i64::MAX);
        a["build"]["compile_ms"] = json!(110);
        c["ledger_event_seq"] = json!(1);
        c["build"]["modules"][0]["compile_ms"] = json!(10);
        let baseline = group(&[a, b, c]);
        let candidate = group(
            &[(180, 900), (250, 550), (500, 500), (150, 1000), (750, 750)].map(|(max, total)| {
                let mut value = sample(max, total);
                value["reviewed_commit"] = json!("1".repeat(40));
                value["build"]["source_set_sha256"] = json!("2".repeat(64));
                value["build"]["modules"][1]["source_sha256"] = json!("3".repeat(64));
                value["profile"]["command_count"] = json!(12);
                value
            }),
        );
        let output = serde_json::to_value(compare(&baseline, &candidate, 50, 25)?)?;
        assert_eq!(
            output,
            json!({
                "schema": "ai4math.t2.heartbeat-comparison.v1",
                "advisory_only": true,
                "regressed": true,
                "target_module": "Target.lean",
                "thresholds": {"min_delta_heartbeats_raw": 50, "min_increase_percent": 25},
                "baseline": {"sample_count": 3, "reviewed_commit": "a".repeat(40), "source_set_sha256": "b".repeat(64), "compiler_sha256": "c".repeat(64), "command_count": 10},
                "candidate": {"sample_count": 5, "reviewed_commit": "1".repeat(40), "source_set_sha256": "2".repeat(64), "compiler_sha256": "c".repeat(64), "command_count": 12},
                "target_source_changed": true,
                "target_prefix_changed": true,
                "max_command": {
                    "baseline_median_heartbeats_raw": 200, "candidate_median_heartbeats_raw": 250,
                    "delta_heartbeats_raw": 50, "status": "compared", "regressed": true,
                },
                "total_commands": {
                    "baseline_median_heartbeats_raw": 600, "candidate_median_heartbeats_raw": 750,
                    "delta_heartbeats_raw": 150, "status": "compared", "regressed": true,
                },
            })
        );
        Ok(())
    }

    #[test]
    fn applies_inclusive_positive_gates_independently_without_rounding() -> Result<()> {
        let baseline = repeat(&sample(100, 100));
        let top_only = compare(
            &repeat(&sample(100, 500)),
            &repeat(&sample(120, 500)),
            20,
            20,
        )?;
        assert!(
            top_only.regressed
                && top_only.max_command.regressed
                && !top_only.total_commands.regressed
        );
        let candidate = repeat(&sample(100, 120));
        let total_only = compare(&baseline, &candidate, 20, 20)?;
        assert!(
            total_only.regressed
                && !total_only.max_command.regressed
                && total_only.total_commands.regressed
        );
        assert!(!compare(&baseline, &candidate, 21, 20)?.regressed);
        assert!(!compare(&baseline, &candidate, 20, 21)?.regressed);
        let decrease = compare(&candidate, &baseline, 1, 1)?;
        assert!(!decrease.regressed);
        assert_eq!(decrease.total_commands.delta_heartbeats_raw, -20);
        let zero = compare(&repeat(&sample(0, 0)), &baseline, 1, 1)?;
        assert!(!zero.regressed);
        assert!(matches!(
            zero.max_command.status,
            MetricStatus::BaselineZero
        ));
        assert!(matches!(
            zero.total_commands.status,
            MetricStatus::BaselineZero
        ));
        assert_eq!(zero.max_command.delta_heartbeats_raw, 100);
        assert_eq!(
            serde_json::to_value(zero)?["max_command"]["status"],
            "baseline_zero"
        );
        let n = MAX_RAW_HEARTBEATS / 11;
        let big = repeat(&sample(n, n));
        assert!(compare(&big, &repeat(&sample(n * 11, n * 11)), n * 10, 1000)?.regressed);
        assert!(!compare(&big, &repeat(&sample(n * 11 - 1, n * 11 - 1)), 1, 1000)?.regressed);
        for (delta, percent) in [(0, 1), (MAX_RAW_HEARTBEATS + 1, 1), (1, 0), (1, 1001)] {
            assert!(compare(&baseline, &candidate, delta, percent).is_err());
        }
        assert!(!compare(&baseline, &candidate, MAX_RAW_HEARTBEATS, 1000)?.regressed);
        Ok(())
    }

    #[test]
    fn enforces_strict_objects_fields_and_array_cardinality() -> Result<()> {
        let valid = sample(100, 120);
        let good = repeat(&valid);
        for count in 0..=10 {
            let data = group(&vec![valid.clone(); count]);
            assert_eq!(
                compare(&good, &data, 1, 1).is_ok(),
                matches!(count, 3 | 5 | 7 | 9)
            );
        }
        assert!(compare(&good, &serde_json::to_vec(&valid)?, 1, 1).is_err());
        assert!(compare(&good, &[good.as_slice(), b"{}"].concat(), 1, 1).is_err());
        for location in ["", "/build", "/profile", "/build/modules/0"] {
            let mut unknown = valid.clone();
            unknown.pointer_mut(location).unwrap()["extra"] = Value::Bool(true);
            assert!(
                compare(&good, &repeat(&unknown), 1, 1).is_err(),
                "unknown field at {location}"
            );
            let mut positional = valid.clone();
            let obj = positional.pointer_mut(location).unwrap();
            *obj = Value::Array(obj.as_object().unwrap().values().cloned().collect());
            assert!(
                compare(&good, &repeat(&positional), 1, 1).is_err(),
                "positional object at {location}"
            );
            let mut missing = valid.clone();
            let obj = missing
                .pointer_mut(location)
                .unwrap()
                .as_object_mut()
                .unwrap();
            let key = obj.keys().next().unwrap().clone();
            obj.remove(&key);
            assert!(
                compare(&good, &repeat(&missing), 1, 1).is_err(),
                "missing field at {location}"
            );
        }
        let text = std::str::from_utf8(&good)?;
        for (field, doubled) in [
            (
                r#""advisory_only":true"#,
                r#""advisory_only":true,"advisory_only":true"#,
            ),
            (
                r#""compiler_sha256":"#,
                r#""compiler_sha256":"","compiler_sha256":"#,
            ),
            (
                r#""command_count":10"#,
                r#""command_count":10,"command_count":10"#,
            ),
            (
                r#""path":"Support.lean""#,
                r#""path":"Support.lean","path":"Support.lean""#,
            ),
        ] {
            assert!(compare(&good, text.replacen(field, doubled, 1).as_bytes(), 1, 1).is_err());
        }
        for not_integer in ["10.0", "1e1", "null", "-1", "\"10\""] {
            let invalid = text.replacen(
                r#""command_count":10"#,
                &format!(r#""command_count":{not_integer}"#),
                1,
            );
            assert!(compare(&good, invalid.as_bytes(), 1, 1).is_err());
        }
        Ok(())
    }

    #[test]
    fn checks_native_metadata_counter_and_build_bounds() -> Result<()> {
        let valid = sample(100, 120);
        let good = repeat(&valid);
        for (pointer, value) in [
            ("/schema", json!("ai4math.t2.clean-build.v1")),
            ("/advisory_only", json!(false)),
            ("/reviewed_commit", json!("0".repeat(40))),
            ("/reviewed_commit", json!("A".repeat(40))),
            ("/ledger_event_seq", json!(-1)),
            ("/build/source_set_sha256", json!("A".repeat(64))),
            ("/build/compiler_sha256", json!("c".repeat(63))),
            ("/build/compile_ms", json!(120_001)),
            ("/build/compile_ms", json!(14)),
            ("/build/modules/0/compile_ms", json!(30_001)),
            ("/build/modules/0/source_sha256", json!("g".repeat(64))),
            ("/build/modules/0/path", json!("../Support.lean")),
            ("/build/modules/0/path", json!("S".repeat(252) + ".lean")),
            ("/build/modules/0/path", json!("TaRgEt.lean")),
            ("/build/modules", json!([])),
            ("/profile/target_module", json!("TARGET.lean")),
            ("/profile/command_count", json!(0)),
            ("/profile/command_count", json!(MAX_COMMAND_COUNT + 1)),
            ("/profile/command_count", json!(1)),
            ("/profile/max_command_heartbeats_raw", json!(121)),
            (
                "/profile/total_command_heartbeats_raw",
                json!(MAX_RAW_HEARTBEATS + 1),
            ),
        ] {
            let mut invalid = valid.clone();
            *invalid.pointer_mut(pointer).unwrap() = value;
            assert!(
                compare(&good, &repeat(&invalid), 1, 1).is_err(),
                "accepted invalid {pointer}"
            );
        }
        let mut bounded = sample(MAX_RAW_HEARTBEATS, MAX_RAW_HEARTBEATS);
        bounded["reviewed_commit"] = json!("1".repeat(64));
        bounded["build"]["compile_ms"] = json!(120_000);
        bounded["build"]["modules"][0]["compile_ms"] = json!(30_000);
        bounded["profile"]["command_count"] = json!(MAX_COMMAND_COUNT);
        let modules = bounded["build"]["modules"].as_array_mut().unwrap();
        for index in modules.len()..MAX_MODULES {
            modules.push(module(&format!("M{index}.lean"), "a"));
        }
        let upper = repeat(&bounded);
        assert!(!compare(&upper, &upper, 1, 1)?.regressed);
        bounded["build"]["modules"]
            .as_array_mut()
            .unwrap()
            .push(module("Extra.lean", "a"));
        assert!(compare(&upper, &repeat(&bounded), 1, 1).is_err());
        Ok(())
    }

    #[test]
    fn validates_group_identity_and_cross_group_comparability() {
        let valid = sample(100, 120);
        let good = repeat(&valid);
        for (pointer, value, diagnostic) in [
            ("/reviewed_commit", json!("b".repeat(40)), "reviewed_commit"),
            (
                "/build/source_set_sha256",
                json!("d".repeat(64)),
                "source_set_sha256",
            ),
            (
                "/build/compiler_sha256",
                json!("d".repeat(64)),
                "compiler_sha256",
            ),
            (
                "/build/modules/0/path",
                json!("Other.lean"),
                "ordered module paths",
            ),
            (
                "/build/modules/0/source_sha256",
                json!("b".repeat(64)),
                "ordered module digests",
            ),
            (
                "/profile/target_module",
                json!("Support.lean"),
                "target_module",
            ),
            ("/profile/command_count", json!(11), "command_count"),
        ] {
            let mut mismatch = valid.clone();
            *mismatch.pointer_mut(pointer).unwrap() = value;
            let heterogeneous = group(&[valid.clone(), mismatch, valid.clone()]);
            let error = compare(&good, &heterogeneous, 1, 1).unwrap_err();
            assert!(
                error.to_string().contains(&format!("{diagnostic} differ")),
                "{error:#}"
            );
        }
        for (pointer, value, diagnostic) in [
            (
                "/reviewed_commit",
                json!("b".repeat(64)),
                "Git object ID widths",
            ),
            (
                "/build/compiler_sha256",
                json!("b".repeat(64)),
                "compiler_sha256",
            ),
            (
                "/profile/target_module",
                json!("Support.lean"),
                "target_module",
            ),
            (
                "/build/source_set_sha256",
                json!("d".repeat(64)),
                "equality disagrees",
            ),
            (
                "/build/modules/0/source_sha256",
                json!("a".repeat(64)),
                "equality disagrees",
            ),
        ] {
            let mut mismatch = valid.clone();
            *mismatch.pointer_mut(pointer).unwrap() = value;
            let error = compare(&good, &repeat(&mismatch), 1, 1).unwrap_err();
            assert!(error.to_string().contains(diagnostic), "{error:#}");
        }
        let mut reordered = valid.clone();
        reordered["build"]["modules"]
            .as_array_mut()
            .unwrap()
            .swap(0, 1);
        assert!(
            compare(&good, &repeat(&reordered), 1, 1)
                .unwrap_err()
                .to_string()
                .contains("ordered module paths")
        );
    }

    #[test]
    fn only_changes_through_the_target_can_explain_a_command_count_change() -> Result<()> {
        let before = sample(100, 120);
        let baseline = repeat(&before);
        let mut after = before;
        after["build"]["source_set_sha256"] = json!("1".repeat(64));
        after["build"]["modules"][2]["source_sha256"] = json!("2".repeat(64));
        let same_commit = compare(&baseline, &repeat(&after), 1, 1).unwrap_err();
        assert!(
            same_commit
                .to_string()
                .contains("identical reviewed_commit")
        );
        after["reviewed_commit"] = json!("9".repeat(40));
        let downstream = compare(&baseline, &repeat(&after), 1, 1)?;
        assert!(!downstream.target_source_changed && !downstream.target_prefix_changed);
        after["profile"]["command_count"] = json!(9);
        let different_count = compare(&baseline, &repeat(&after), 1, 1).unwrap_err();
        assert!(
            different_count
                .to_string()
                .contains("identical target prefix")
        );
        after["build"]["modules"][0]["source_sha256"] = json!("3".repeat(64));
        let upstream = compare(&baseline, &repeat(&after), 1, 1)?;
        assert!(!upstream.target_source_changed && upstream.target_prefix_changed);
        Ok(())
    }
}
