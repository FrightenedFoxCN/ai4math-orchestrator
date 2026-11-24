//! Compare caller-supplied timing observations without asserting build provenance.

use ai4math_core::Engine;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Deserializer, Serialize, de};
use std::{collections::HashSet, fmt, marker::PhantomData};

const BUILD_SCHEMA: &str = "ai4math.t2.clean-build.v1";
const MAX_SAMPLES: usize = 9;
const MAX_MODULES: usize = 128;
const MAX_MODULE_COMPILE_MS: u64 = 30_000;
const MAX_TOTAL_COMPILE_MS: u64 = 120_000;
const MAX_INCREASE_PERCENT: u64 = 1_000;

#[derive(Debug, Serialize)]
pub(crate) struct T2Comparison {
    schema: &'static str,
    advisory_only: bool,
    pub regressed: bool,
    thresholds: Thresholds,
    baseline: GroupSummary,
    candidate: GroupSummary,
    total: TimingComparison,
    modules: Vec<ModuleComparison>,
}

#[derive(Debug, Serialize)]
struct Thresholds {
    min_delta_ms: u64,
    min_increase_percent: u64,
}

#[derive(Debug, Serialize)]
struct GroupSummary {
    sample_count: usize,
    reviewed_commit: String,
    source_set_sha256: String,
    compiler_sha256: String,
    median_compile_ms: u64,
}

#[derive(Debug, Serialize)]
struct ModuleComparison {
    path: String,
    source_changed: bool,
    #[serde(flatten)]
    timing: TimingComparison,
}

#[derive(Debug, Serialize)]
struct TimingComparison {
    baseline_median_ms: u64,
    candidate_median_ms: u64,
    delta_ms: i64,
    status: TimingStatus,
    regressed: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum TimingStatus {
    Compared,
    BaselineZero,
}

impl TimingComparison {
    fn new(baseline_median_ms: u64, candidate_median_ms: u64, thresholds: &Thresholds) -> Self {
        // All times and thresholds have been bounded; cross-multiply for exact
        // inclusive percentage comparisons without rounding or division by zero.
        let delta_ms = candidate_median_ms as i64 - baseline_median_ms as i64;
        let regressed = baseline_median_ms != 0
            && delta_ms > 0
            && delta_ms as u64 >= thresholds.min_delta_ms
            && delta_ms as u64 * 100 >= baseline_median_ms * thresholds.min_increase_percent;
        Self {
            baseline_median_ms,
            candidate_median_ms,
            delta_ms,
            status: if baseline_median_ms == 0 {
                TimingStatus::BaselineZero
            } else {
                TimingStatus::Compared
            },
            regressed,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BuildSample {
    schema: String,
    reviewed_commit: String,
    ledger_event_seq: i64,
    engine: String,
    // Unlike Option, this requires the field to be present and accept only null.
    #[serde(rename = "external_checker_passed")]
    _external_checker_passed: (),
    advisory_only: bool,
    source_set_sha256: String,
    compiler_sha256: String,
    compile_ms: u64,
    modules: ObjectArray<ModuleSample, MAX_MODULES>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ModuleSample {
    path: String,
    source_sha256: String,
    compile_ms: u64,
}

// A derived struct can accept positional sequences as well as objects. Only
// admit object rows, keeping derive's missing, unknown and duplicate key checks.
struct Object<T>(T);

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Object<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor<T>(PhantomData<T>);
        impl<'de, T: Deserialize<'de>> de::Visitor<'de> for Visitor<T> {
            type Value = Object<T>;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a JSON object")
            }

            fn visit_map<M: de::MapAccess<'de>>(self, map: M) -> Result<Self::Value, M::Error> {
                T::deserialize(de::value::MapAccessDeserializer::new(map)).map(Object)
            }
        }
        deserializer.deserialize_map(Visitor(PhantomData))
    }
}

// Bound the number of retained objects during deserialization, not after
// allocating an arbitrary number of reports or modules.
struct ObjectArray<T, const MAX: usize>(Vec<T>);

impl<'de, T: Deserialize<'de>, const MAX: usize> Deserialize<'de> for ObjectArray<T, MAX> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor<T, const MAX: usize>(PhantomData<T>);
        impl<'de, T: Deserialize<'de>, const MAX: usize> de::Visitor<'de> for Visitor<T, MAX> {
            type Value = ObjectArray<T, MAX>;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "a JSON array with at most {MAX} objects")
            }

            fn visit_seq<S: de::SeqAccess<'de>>(self, mut seq: S) -> Result<Self::Value, S::Error> {
                let mut rows = Vec::new();
                while rows.len() < MAX {
                    let Some(Object(row)) = seq.next_element()? else {
                        return Ok(ObjectArray(rows));
                    };
                    rows.push(row);
                }
                if seq.next_element::<de::IgnoredAny>()?.is_some() {
                    return Err(de::Error::custom(format!(
                        "JSON array exceeds {MAX} objects"
                    )));
                }
                Ok(ObjectArray(rows))
            }
        }
        deserializer.deserialize_seq(Visitor(PhantomData))
    }
}

fn lower_hex(value: &str) -> bool {
    value
        .bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn canonical_sha256(hash: &str) -> bool {
    hash.len() == 64 && lower_hex(hash)
}

fn canonical_commit(oid: &str) -> bool {
    matches!(oid.len(), 40 | 64) && lower_hex(oid) && oid.bytes().any(|b| b != b'0')
}

// Match the managed producer's root Lean filename grammar and byte limit.
fn root_lean_path(path: &str) -> bool {
    path.len() <= 255
        && path.strip_suffix(".lean").is_some_and(|name| {
            name.starts_with(|ch: char| ch.is_ascii_uppercase())
                && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        })
}

impl BuildSample {
    fn ordered_paths(&self) -> impl Iterator<Item = &str> {
        self.modules.0.iter().map(|m| m.path.as_str())
    }

    fn ordered_module_digests(&self) -> impl Iterator<Item = &str> {
        self.modules.0.iter().map(|m| m.source_sha256.as_str())
    }

    fn validate(&self) -> Result<()> {
        ensure!(self.schema == BUILD_SCHEMA, "unexpected clean-build schema");
        ensure!(
            self.engine == Engine::Lean4.as_str(),
            "engine must be lean4"
        );
        ensure!(self.advisory_only, "advisory_only must be true");
        ensure!(
            canonical_commit(&self.reviewed_commit),
            "invalid reviewed_commit Git object ID"
        );
        ensure!(
            self.ledger_event_seq >= 0,
            "ledger_event_seq must be nonnegative"
        );
        ensure!(
            canonical_sha256(&self.source_set_sha256),
            "invalid source_set_sha256"
        );
        ensure!(
            canonical_sha256(&self.compiler_sha256),
            "invalid compiler_sha256"
        );
        ensure!(
            self.compile_ms <= MAX_TOTAL_COMPILE_MS,
            "total compile_ms exceeds {MAX_TOTAL_COMPILE_MS}"
        );
        ensure!(!self.modules.0.is_empty(), "empty module list");

        let mut paths = HashSet::new();
        let mut sum = 0;
        for (index, module) in self.modules.0.iter().enumerate() {
            ensure!(
                root_lean_path(&module.path),
                "invalid root Lean module path at index {index}"
            );
            ensure!(
                paths.insert(module.path.to_ascii_lowercase()),
                "case-folded module path collision: {:?}",
                module.path
            );
            ensure!(
                canonical_sha256(&module.source_sha256),
                "invalid source_sha256 for {:?}",
                module.path
            );
            ensure!(
                module.compile_ms <= MAX_MODULE_COMPILE_MS,
                "module compile_ms exceeds {MAX_MODULE_COMPILE_MS} for {:?}",
                module.path
            );
            sum += module.compile_ms;
        }
        ensure!(
            sum <= self.compile_ms,
            "sum of module compile_ms exceeds total compile_ms"
        );
        Ok(())
    }
}

fn parse_group(bytes: &[u8], group: &str) -> Result<Vec<BuildSample>> {
    let ObjectArray(samples) =
        serde_json::from_slice::<ObjectArray<BuildSample, MAX_SAMPLES>>(bytes)
            .with_context(|| format!("invalid {group} clean-build JSON array"))?;
    ensure!(
        matches!(samples.len(), 3 | 5 | 7 | 9),
        "{group} must contain 3, 5, 7 or 9 reports"
    );
    let first = &samples[0];
    for (index, sample) in samples.iter().enumerate() {
        sample
            .validate()
            .with_context(|| format!("invalid {group} clean-build report {}", index + 1))?;
        ensure!(
            sample.reviewed_commit == first.reviewed_commit,
            "reviewed_commit differs within the {group} group"
        );
        ensure!(
            sample.source_set_sha256 == first.source_set_sha256,
            "source_set_sha256 differs within the {group} group"
        );
        ensure!(
            sample.compiler_sha256 == first.compiler_sha256,
            "compiler_sha256 differs within the {group} group"
        );
        ensure!(
            sample.ordered_paths().eq(first.ordered_paths()),
            "ordered module paths differ within the {group} group"
        );
        ensure!(
            sample
                .ordered_module_digests()
                .eq(first.ordered_module_digests()),
            "ordered module digests differ within the {group} group"
        );
    }
    Ok(samples)
}

fn median(values: impl Iterator<Item = u64>) -> u64 {
    let mut values: Vec<_> = values.collect();
    values.sort_unstable();
    values[values.len() / 2]
}

fn summarize(samples: &[BuildSample]) -> GroupSummary {
    let identity = &samples[0];
    GroupSummary {
        sample_count: samples.len(),
        reviewed_commit: identity.reviewed_commit.clone(),
        source_set_sha256: identity.source_set_sha256.clone(),
        compiler_sha256: identity.compiler_sha256.clone(),
        median_compile_ms: median(samples.iter().map(|sample| sample.compile_ms)),
    }
}

pub(crate) fn compare(
    baseline: &[u8],
    candidate: &[u8],
    min_delta_ms: u64,
    min_increase_percent: u64,
) -> Result<T2Comparison> {
    ensure!(
        (1..=MAX_TOTAL_COMPILE_MS).contains(&min_delta_ms),
        "min_delta_ms must be in 1..={MAX_TOTAL_COMPILE_MS}"
    );
    ensure!(
        (1..=MAX_INCREASE_PERCENT).contains(&min_increase_percent),
        "min_increase_percent must be in 1..={MAX_INCREASE_PERCENT}"
    );
    let thresholds = Thresholds {
        min_delta_ms,
        min_increase_percent,
    };
    let baseline = parse_group(baseline, "baseline")?;
    let candidate = parse_group(candidate, "candidate")?;
    let before = &baseline[0];
    let after = &candidate[0];
    ensure!(
        before.compiler_sha256 == after.compiler_sha256,
        "baseline and candidate compiler_sha256 must match"
    );
    ensure!(
        before.ordered_paths().eq(after.ordered_paths()),
        "baseline and candidate ordered module paths must match"
    );
    ensure!(
        before.reviewed_commit.len() == after.reviewed_commit.len(),
        "baseline and candidate reviewed_commit Git object ID widths must match"
    );
    let same_module_digests = before
        .ordered_module_digests()
        .eq(after.ordered_module_digests());
    ensure!(
        (before.source_set_sha256 == after.source_set_sha256) == same_module_digests,
        "source_set_sha256 equality disagrees with per-module source_sha256 equality"
    );
    ensure!(
        before.reviewed_commit != after.reviewed_commit || same_module_digests,
        "identical reviewed_commit cannot have different module source digests"
    );

    let modules: Vec<_> = before
        .modules
        .0
        .iter()
        .zip(&after.modules.0)
        .enumerate()
        .map(|(index, (a, b))| ModuleComparison {
            path: a.path.clone(),
            source_changed: a.source_sha256 != b.source_sha256,
            timing: TimingComparison::new(
                median(
                    baseline
                        .iter()
                        .map(|sample| sample.modules.0[index].compile_ms),
                ),
                median(
                    candidate
                        .iter()
                        .map(|sample| sample.modules.0[index].compile_ms),
                ),
                &thresholds,
            ),
        })
        .collect();
    let baseline = summarize(&baseline);
    let candidate = summarize(&candidate);
    let total = TimingComparison::new(
        baseline.median_compile_ms,
        candidate.median_compile_ms,
        &thresholds,
    );
    Ok(T2Comparison {
        schema: "ai4math.t2.performance-comparison.v1",
        advisory_only: true,
        regressed: total.regressed || modules.iter().any(|m| m.timing.regressed),
        thresholds,
        baseline,
        candidate,
        total,
        modules,
    })
}
