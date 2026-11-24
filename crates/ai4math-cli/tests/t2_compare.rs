use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    ffi::OsStr,
    fs,
    path::Path,
    process::{Command, Output},
};

const INPUT_CAP: usize = 1024 * 1024;
const BASELINE: &str = "baseline.json";
const CANDIDATE: &str = "candidate.json";

// Build the exact clean-build wire shape without running a clock-sensitive compiler.
fn samples(candidate: bool, timings: &[(u64, u64, u64)]) -> Value {
    let (commit, seed, alpha_source) = if candidate {
        ('b', 45, "def alpha := 2\n")
    } else {
        ('a', 42, "def alpha := 1\n")
    };
    let sources = [
        ("Alpha.lean", alpha_source),
        ("Beta.lean", "import Alpha\ndef beta := alpha\n"),
    ];
    let mut set_hash = Sha256::new();
    set_hash.update(b"ai4math:ordered-source-dependencies:v1");
    set_hash.update((sources.len() as u64).to_be_bytes());
    for (path, source) in sources {
        for bytes in [path.as_bytes(), source.as_bytes()] {
            set_hash.update((bytes.len() as u64).to_be_bytes());
            set_hash.update(bytes);
        }
    }
    let set_hash = format!("{:x}", set_hash.finalize());
    Value::Array(
        timings
            .iter()
            .enumerate()
            .map(|(i, &(total, alpha, beta))| {
                let modules: Vec<_> = sources
                    .iter()
                    .zip([alpha, beta])
                    .map(|((path, source), millis)| {
                        json!({
                            "path": path,
                            "source_sha256": format!("{:x}", Sha256::digest(source.as_bytes())),
                            "compile_ms": millis,
                        })
                    })
                    .collect();
                json!({
                    "schema": "ai4math.t2.clean-build.v1",
                    "reviewed_commit": commit.to_string().repeat(40),
                    "ledger_event_seq": seed + i as i64,
                    "engine": "lean4",
                    "external_checker_passed": null,
                    "advisory_only": true,
                    "source_set_sha256": set_hash,
                    "compiler_sha256": "c".repeat(64),
                    "compile_ms": total,
                    "modules": modules,
                })
            })
            .collect(),
    )
}

fn write_samples(root: &Path, path: &str, samples: &Value) {
    fs::write(root.join(path), serde_json::to_vec(samples).unwrap()).unwrap();
}

fn compare_paths(root: &Path, args: [&str; 4]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ai4math"))
        .current_dir(root)
        .arg("t2-compare")
        .args(args)
        .output()
        .unwrap()
}

fn compare(root: &Path, delta: &str, percent: &str) -> Output {
    compare_paths(root, [BASELINE, CANDIDATE, delta, percent])
}

fn report(out: Output, regressed: bool) -> Value {
    assert!(out.status.code().is_some(), "abnormal exit: {}", out.status);
    assert_eq!(
        out.status.success(),
        !regressed,
        "stdout: {}; stderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let parsed: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(parsed["schema"], "ai4math.t2.performance-comparison.v1");
    assert_eq!(parsed["advisory_only"], true);
    assert_eq!(parsed["regressed"], regressed);
    assert!(parsed.get("external_checker_passed").is_none());
    parsed
}

fn rejected(out: Output, case: &str) -> String {
    assert!(!out.status.success(), "accepted {case}");
    assert!(
        out.stdout.is_empty(),
        "wrote partial output for {case}: {}",
        String::from_utf8_lossy(&out.stdout),
    );
    assert!(!out.stderr.is_empty(), "no diagnostic for {case}");
    String::from_utf8(out.stderr).unwrap()
}

#[test]
fn independent_medians_and_inclusive_gates_report_changed_sources_without_writes() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let baseline = samples(
        false,
        &[
            (1000, 400, 100),
            (40000, 9000, 7000),
            (980, 380, 90),
            (1010, 410, 110),
            (990, 390, 95),
        ],
    );
    let candidate = samples(
        true,
        &[
            (1210, 470, 95),
            (1200, 480, 81),
            (1190, 450, 90),
            (49000, 10000, 11000),
            (1180, 490, 80),
        ],
    );
    write_samples(root, BASELINE, &baseline);
    write_samples(root, CANDIDATE, &candidate);
    let before = [BASELINE, CANDIDATE].map(|file| fs::read(root.join(file)).unwrap());

    // Both gates meet equality for Alpha; aggregate percent also meets equality.
    let exact = report(compare(root, "80", "20"), true);
    assert_eq!(
        exact["thresholds"],
        json!({"min_delta_ms": 80, "min_increase_percent": 20})
    );
    for (field, runs, median) in [
        ("baseline", &baseline, 1000),
        ("candidate", &candidate, 1200),
    ] {
        assert_eq!(
            exact[field],
            json!({
                "sample_count": 5,
                "reviewed_commit": runs[0]["reviewed_commit"],
                "source_set_sha256": runs[0]["source_set_sha256"],
                "compiler_sha256": runs[0]["compiler_sha256"],
                "median_compile_ms": median,
            })
        );
    }
    assert_eq!(
        exact["total"],
        json!({
            "baseline_median_ms": 1000, "candidate_median_ms": 1200, "delta_ms": 200,
            "status": "compared", "regressed": true,
        })
    );
    assert_eq!(
        exact["modules"],
        json!([
            {"path":"Alpha.lean", "source_changed":true, "baseline_median_ms":400,
             "candidate_median_ms":480, "delta_ms":80, "status":"compared", "regressed":true},
            {"path":"Beta.lean", "source_changed":false, "baseline_median_ms":100,
             "candidate_median_ms":90, "delta_ms":-10, "status":"compared", "regressed":false},
        ])
    );
    let total_only = report(compare(root, "200", "20"), true);
    assert_eq!(total_only["total"]["regressed"], true);
    assert_eq!(total_only["modules"][0]["regressed"], false);
    for (delta, percent) in [("80", "21"), ("201", "20")] {
        let clean = report(compare(root, delta, percent), false);
        assert_eq!(clean["total"]["regressed"], false);
        assert_eq!(clean["modules"][0]["regressed"], false);
    }

    let after = [BASELINE, CANDIDATE].map(|file| fs::read(root.join(file)).unwrap());
    assert_eq!(after, before);
    let mut names: Vec<_> = fs::read_dir(root)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    names.sort_unstable();
    assert_eq!(names, [OsStr::new(BASELINE), OsStr::new(CANDIDATE)]);
}

#[test]
fn zero_baselines_are_uncompared_and_tiny_baselines_still_require_both_gates() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    write_samples(root, BASELINE, &samples(false, &[(100, 0, 1); 3]));
    write_samples(root, CANDIDATE, &samples(true, &[(100, 30, 2); 3]));
    let under = report(compare(root, "2", "100"), false);
    let zero = &under["modules"][0];
    assert_eq!(zero["baseline_median_ms"], 0);
    assert_eq!(zero["delta_ms"], 30);
    assert_eq!(zero["status"], "baseline_zero");
    assert_eq!(zero["regressed"], false);
    let tiny = &under["modules"][1];
    assert_eq!(tiny["baseline_median_ms"], 1);
    assert_eq!(tiny["delta_ms"], 1);
    assert_eq!(tiny["status"], "compared");
    assert_eq!(tiny["regressed"], false);

    let module_only = report(compare(root, "1", "100"), true);
    assert_eq!(module_only["total"]["regressed"], false);
    assert_eq!(module_only["modules"][0]["regressed"], false);
    assert_eq!(module_only["modules"][1]["regressed"], true);

    write_samples(root, BASELINE, &samples(false, &[(0, 0, 0); 3]));
    let all_zero = report(compare(root, "1", "1"), false);
    assert_eq!(
        all_zero["total"],
        json!({
            "baseline_median_ms": 0, "candidate_median_ms": 100, "delta_ms": 100,
            "status": "baseline_zero", "regressed": false,
        })
    );

    // 33 / 333 is just under 10%, even though it rounds to 10%.
    write_samples(root, BASELINE, &samples(false, &[(333, 0, 0); 3]));
    write_samples(root, CANDIDATE, &samples(true, &[(366, 0, 0); 3]));
    report(compare(root, "33", "10"), false);
    report(compare(root, "33", "9"), true);
}

#[test]
fn each_group_must_have_an_allowed_size_and_one_snapshot_and_both_groups_one_layout() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let baseline = samples(false, &[(100, 40, 10); 7]);
    write_samples(root, BASELINE, &baseline);
    for count in [3, 5, 7, 9] {
        write_samples(root, CANDIDATE, &samples(true, &vec![(100, 40, 10); count]));
        let accepted = report(compare(root, "1", "1"), false);
        assert_eq!(accepted["baseline"]["sample_count"], 7);
        assert_eq!(accepted["candidate"]["sample_count"], count);
    }
    for count in [0, 1, 2, 4, 6, 8, 10] {
        write_samples(root, CANDIDATE, &samples(true, &vec![(100, 40, 10); count]));
        rejected(compare(root, "1", "1"), &format!("{count} samples"));
    }

    let candidate = samples(true, &[(130, 55, 10); 3]);
    for (pointer, value) in [
        ("/2/reviewed_commit", json!("a".repeat(40))),
        ("/2/compiler_sha256", json!("d".repeat(64))),
        ("/2/source_set_sha256", json!("d".repeat(64))),
        ("/2/modules/0/source_sha256", json!("d".repeat(64))),
        ("/2/modules/0/path", json!("Gamma.lean")),
    ] {
        let mut bad = candidate.clone();
        *bad.pointer_mut(pointer).unwrap() = value;
        write_samples(root, CANDIDATE, &bad);
        rejected(compare(root, "1", "1"), pointer);
    }

    type SampleMutation = fn(&mut Value, &Value);
    let cases: [(&str, SampleMutation); 7] = [
        ("different compiler", |run, _| {
            run["compiler_sha256"] = json!("d".repeat(64));
        }),
        ("different module order", |run, _| {
            run["modules"].as_array_mut().unwrap().swap(0, 1);
        }),
        ("different module path", |run, _| {
            run["modules"][1]["path"] = json!("Gamma.lean");
        }),
        (
            "same source-set digest with changed module source",
            |run, before| {
                run["source_set_sha256"] = before["source_set_sha256"].clone();
            },
        ),
        (
            "changed source-set digest with same module source",
            |run, before| {
                run["modules"][0]["source_sha256"] = before["modules"][0]["source_sha256"].clone();
            },
        ),
        ("same reviewed commit with changed source", |run, before| {
            run["reviewed_commit"] = before["reviewed_commit"].clone();
        }),
        ("different Git OID widths", |run, _| {
            run["reviewed_commit"] = json!("b".repeat(64));
        }),
    ];
    for (case, alter) in cases {
        let mut bad = candidate.clone();
        for run in bad.as_array_mut().unwrap() {
            alter(run, &baseline[0]);
        }
        write_samples(root, CANDIDATE, &bad);
        rejected(compare(root, "1", "1"), case);
    }
}

#[test]
fn rejects_malformed_and_unbounded_input_without_partial_output() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let baseline = samples(false, &[(80_000, 30_000, 30_000); 3]);
    let candidate = samples(true, &[(120_000, 30_000, 30_000); 3]);
    write_samples(root, BASELINE, &baseline);
    write_samples(root, CANDIDATE, &candidate);
    report(compare(root, "120000", "1000"), false);
    for threshold in [
        "",
        "0",
        "01",
        "+1",
        "-1",
        " 1",
        "1.0",
        "1e1",
        "18446744073709551616",
    ] {
        rejected(compare(root, threshold, "1"), threshold);
        rejected(compare(root, "1", threshold), threshold);
    }
    rejected(compare(root, "120001", "1"), "absolute threshold too large");
    rejected(compare(root, "1", "1001"), "relative threshold too large");

    for (pointer, value) in [
        ("/2/schema", json!("ai4math.t2.clean-build.v2")),
        ("/2/engine", json!("rocq")),
        ("/2/advisory_only", json!(false)),
        ("/2/external_checker_passed", json!(true)),
        ("/2/ledger_event_seq", json!(-1)),
        ("/2/reviewed_commit", json!("0".repeat(40))),
        ("/2/source_set_sha256", json!("C".repeat(64))),
        ("/2/compile_ms", json!(120_001)),
        ("/2/compile_ms", json!(59_999)),
        ("/2/compile_ms", json!(80_000.0)),
        ("/2/modules/0/path", json!("../Alpha.lean")),
        (
            "/2/modules/0/path",
            json!(format!("{}.lean", "A".repeat(251))),
        ),
        ("/2/modules/1/path", json!("ALPHA.lean")),
        ("/2/modules/0/compile_ms", json!(30_001)),
        ("/2/modules", json!([])),
        (
            "/2/modules/0",
            json!([
                "Alpha.lean",
                candidate[2]["modules"][0]["source_sha256"],
                30_000
            ]),
        ),
        (
            "/2",
            json!([
                candidate[2]["schema"],
                candidate[2]["reviewed_commit"],
                candidate[2]["ledger_event_seq"],
                candidate[2]["engine"],
                null,
                true,
                candidate[2]["source_set_sha256"],
                candidate[2]["compiler_sha256"],
                candidate[2]["compile_ms"],
                candidate[2]["modules"],
            ]),
        ),
    ] {
        let mut bad = candidate.clone();
        *bad.pointer_mut(pointer).unwrap() = value;
        write_samples(root, CANDIDATE, &bad);
        rejected(compare(root, "1", "1"), pointer);
    }
    let mut over_modules = candidate.clone();
    over_modules[2]["modules"] = Value::Array(
        (0..129)
            .map(|i| {
                json!({
                    "path": format!("Item{i}.lean"),
                    "source_sha256": "a".repeat(64),
                    "compile_ms": 0,
                })
            })
            .collect(),
    );
    write_samples(root, CANDIDATE, &over_modules);
    rejected(compare(root, "1", "1"), "over 128 modules");

    let mut missing = candidate.clone();
    missing[2]
        .as_object_mut()
        .unwrap()
        .remove("external_checker_passed")
        .unwrap();
    let mut unknown = candidate.clone();
    unknown[2]["modules"][0]["ignored"] = json!(null);
    let raw = serde_json::to_string(&candidate).unwrap();
    for (case, input) in [
        ("missing explicit checker null", missing.to_string()),
        ("unknown module field", unknown.to_string()),
        (
            "duplicate report key",
            raw.replacen(
                "\"advisory_only\":true",
                "\"advisory_only\":true,\"advisory_only\":true",
                1,
            ),
        ),
        ("report instead of array", candidate[0].to_string()),
        ("trailing document", format!("{raw} null")),
        ("truncated JSON", raw[..raw.len() - 1].to_owned()),
    ] {
        fs::write(root.join(CANDIDATE), input).unwrap();
        rejected(compare(root, "1", "1"), case);
    }
    let mut padded = raw.into_bytes();
    padded.resize(INPUT_CAP, b' ');
    fs::write(root.join(CANDIDATE), &padded).unwrap();
    report(compare(root, "120000", "1000"), false);
    padded.push(b' ');
    fs::write(root.join(CANDIDATE), &padded).unwrap();
    let error = rejected(compare(root, "1", "1"), "file over byte limit");
    assert!(error.contains("1 MiB limit"), "{error}");
}

#[test]
fn only_regular_sample_files_are_accepted() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let good = samples(false, &[(100, 40, 10); 3]);
    write_samples(root, BASELINE, &good);
    let error = rejected(compare_paths(root, [BASELINE, ".", "1", "1"]), "directory");
    assert!(error.contains("must be a regular file"), "{error}");
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(BASELINE, root.join(CANDIDATE)).unwrap();
        let error = rejected(compare(root, "1", "1"), "symlink to a valid sample file");
        assert!(error.contains("must be a regular file"), "{error}");
    }
}
