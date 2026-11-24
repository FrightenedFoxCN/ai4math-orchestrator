#![cfg(target_os = "macos")]

use ai4math_broker::ProjectNativeGate;
use ai4math_store::{Change, NewClaim, StateStore, Trust};
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

const BASIS: &str = "ZHeartbeatBasis.lean";
const TARGET: &str = "AHeartbeatTarget.lean";
const LATER: &str = "BHeartbeatAfter.lean";
const MANIFEST: &str = "ai4math-modules.txt";
const CLAIM: &str = "native-t2-heartbeats-fixture";
const DECLARATION: &str = "heartbeatFixtureProof";
const SAMPLE_COUNT: usize = 3;
const MODULES: [(&str, &str); 3] = [
    (BASIS, "def heartbeatFixtureBasis : Nat := 2\n"),
    (
        TARGET,
        "import ZHeartbeatBasis

def heartbeatFixtureDerived : Nat := heartbeatFixtureBasis + 3
def heartbeatFixtureDouble : Nat := heartbeatFixtureDerived * 2
theorem heartbeatFixtureProof : heartbeatFixtureDerived = 5 := by decide
",
    ),
    (
        LATER,
        "import AHeartbeatTarget
theorem heartbeatFixtureAfter : heartbeatFixtureDerived = 5 := heartbeatFixtureProof
",
    ),
];
const HEAVY: &str = "
set_option maxRecDepth 10000 in
theorem heartbeatFixtureHeavy : (List.range (heartbeatFixtureBasis * 500)).length = 1000 := by decide
";
const BROKEN: &str = "import AHeartbeatTarget
theorem heartbeatFixtureAfter : False := heartbeatFixtureProof
";

fn checked(output: Output, case: &str) -> Result<Vec<u8>> {
    ensure!(
        output.status.success(),
        "{case} failed ({}):\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(output.stdout)
}

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("/usr/bin/git")
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .current_dir(dir)
        .args([
            "-c",
            "user.name=T2 Heartbeat Fixture",
            "-c",
            "user.email=t2-heartbeat@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()?;
    Ok(String::from_utf8(checked(output, args[0])?)?.trim().into())
}

fn advance_main(
    source: &Path,
    repo: &Path,
    path: &str,
    content: &str,
    message: &str,
) -> Result<String> {
    fs::write(source.join(path), content)?;
    git(source, &["add", "--", path])?;
    git(source, &["commit", "--quiet", "-m", message])?;
    git(source, &["push", "--quiet", "../managed.git", "main"])?;
    git(repo, &["rev-parse", "--verify", "refs/heads/main"])
}

fn ai(root: &Path, subcommand: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ai4math"));
    command.current_dir(root).arg(subcommand).arg(root);
    command
}

fn file_sha256(path: &Path) -> Result<String> {
    let mut digest = Sha256::new();
    std::io::copy(&mut fs::File::open(path)?, &mut digest)?;
    Ok(format!("{:x}", digest.finalize()))
}

// Include Git refs, objects and reflogs, the ledger and WAL, and any retained
// build/profile artifacts. Only the ephemeral SQLite shared-memory index is exempt.
fn snapshot(fixture: &Path) -> Result<BTreeMap<PathBuf, Option<String>>> {
    let mut files = BTreeMap::new();
    let mut pending = vec![fixture.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            let relative = path.strip_prefix(fixture)?;
            if relative == Path::new("project/.runtime/state.sqlite-shm") {
                continue;
            }
            let kind = entry.file_type()?;
            let digest = if kind.is_dir() {
                pending.push(path.clone());
                None
            } else {
                ensure!(kind.is_file(), "unexpected fixture entry: {path:?}");
                Some(file_sha256(&path)?)
            };
            files.insert(relative.to_path_buf(), digest);
        }
    }
    Ok(files)
}

fn rejects(output: Output, case: &str, diagnostic: &str) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{case}: unexpectedly accepted");
    assert!(output.stdout.is_empty(), "{case}: emitted a partial report");
    assert!(
        stderr.contains(diagnostic),
        "{case}: expected {diagnostic:?}, got {stderr:?}"
    );
}

struct HeartbeatSample {
    report: Value,
    max_raw: u64,
    total_raw: u64,
}

fn median(mut values: [u64; SAMPLE_COUNT]) -> u64 {
    values.sort_unstable();
    values[SAMPLE_COUNT / 2]
}

fn assert_build(build: &Value, pin: &str, sources: &[(&str, &str)]) -> Result<()> {
    assert_eq!(build["compiler_sha256"], pin);
    assert!(build["compile_ms"].as_u64().is_some());
    let modules = build["modules"].as_array().context("built modules")?;
    assert_eq!(modules.len(), sources.len());
    let mut source_set = Sha256::new();
    source_set.update(b"ai4math:ordered-source-dependencies:v1");
    source_set.update((sources.len() as u64).to_be_bytes());
    for (module, &(path, source)) in modules.iter().zip(sources) {
        assert_eq!(module["path"], path);
        assert_eq!(
            module["source_sha256"],
            format!("{:x}", Sha256::digest(source.as_bytes()))
        );
        assert!(module["compile_ms"].as_u64().is_some());
        for item in [path, source] {
            source_set.update((item.len() as u64).to_be_bytes());
            source_set.update(item.as_bytes());
        }
    }
    assert_eq!(
        build["source_set_sha256"],
        format!("{:x}", source_set.finalize())
    );
    Ok(())
}

#[test]
#[ignore = "requires native macOS Seatbelt and absolute AI4MATH_TEST_LEAN from the bundled toolchain"]
fn native_t2_heartbeats_profiles_a_reviewed_snapshot_without_authority() -> Result<()> {
    let lean = PathBuf::from(
        std::env::var_os("AI4MATH_TEST_LEAN").context("set AI4MATH_TEST_LEAN to bundled Lean")?,
    );
    ensure!(lean.is_absolute(), "AI4MATH_TEST_LEAN must be absolute");
    let lean = lean
        .canonicalize()
        .context("resolve native Lean executable")?;
    let pin = file_sha256(&lean)?;
    let wrong_pin = format!(
        "{}{}",
        if pin.starts_with('0') { '1' } else { '0' },
        &pin[1..]
    );

    let temp = tempfile::tempdir()?;
    let fixture = temp.path().canonicalize()?;
    let root = fixture.join("project");
    let source = fixture.join("source");
    let repo = fixture.join("managed.git");
    fs::create_dir(&root)?;
    fs::create_dir(&source)?;
    git(&source, &["init", "--quiet", "--initial-branch=main"])?;
    for (path, content) in MODULES {
        fs::write(source.join(path), content)?;
    }
    // Nonlexical manifest order: the target needs the predecessor's compiled
    // object, and the module following the target needs the target's object.
    fs::write(
        source.join(MANIFEST),
        format!("{BASIS}\n{TARGET}\n{LATER}\n"),
    )?;
    git(&source, &["add", "--", BASIS, TARGET, LATER, MANIFEST])?;
    git(&source, &["commit", "--quiet", "-m", "review Lean fixture"])?;
    git(
        &fixture,
        &[
            "clone",
            "--bare",
            "--no-hardlinks",
            "--quiet",
            "source",
            "managed.git",
        ],
    )?;
    let head = git(&repo, &["rev-parse", "--verify", "refs/heads/main"])?;
    checked(ai(&root, "init").arg("lean4").output()?, "init project")?;
    checked(
        ai(&root, "task-bind")
            .arg(&repo)
            .args([TARGET, &head])
            .output()?,
        "bind reviewed repository",
    )?;
    // Initialize the coordination inode before baselining durable state.
    drop(ProjectNativeGate::new(&root).try_heavy()?);

    // Keep the store open so the fixture can compare the WAL bytes as well.
    let mut store = StateStore::open(root.join(".runtime/state.sqlite"))?;
    store.create_claim(
        NewClaim {
            id: CLAIM,
            title: "Native Lean heartbeat fixture",
            informal_statement: "A heartbeat profile is not a proof receipt.",
            engine: "lean4",
            formal_target: Some(DECLARATION),
            module_path: Some(TARGET),
            critical: true,
        },
        Change {
            actor: "t2-heartbeats-fixture",
            git_commit: Some(&head),
        },
    )?;
    let seq = store.status()?.last_event_seq;
    let assert_unchanged = |before: &BTreeMap<PathBuf, Option<String>>, case: &str| -> Result<()> {
        assert_eq!(&snapshot(&fixture)?, before, "{case}: wrote fixture files");
        assert_eq!(
            store.status()?.last_event_seq,
            seq,
            "{case}: changed ledger"
        );
        let claim = store.claim(CLAIM)?.context("missing fixture claim")?;
        assert_eq!(claim.local_trust, Trust::L0);
        assert_eq!(claim.effective_trust, Trust::L0);
        assert!(claim.receipt_id.is_none());
        assert!(store.managed_proof_recipe(CLAIM)?.is_none());
        Ok(())
    };
    let profile = |reviewed: &str, compiler_pin: &str| {
        ai(&root, "t2-heartbeats")
            .arg(&repo)
            .args([TARGET, reviewed])
            .arg(&lean)
            .arg(compiler_pin)
            .output()
    };

    let read_sample =
        |commit: &str, modules: &[(&str, &str)], count: u64| -> Result<HeartbeatSample> {
            let report: Value =
                serde_json::from_slice(&checked(profile(commit, &pin)?, "profile")?)?;
            assert_eq!(report["schema"], "ai4math.t2.lean-heartbeats.v1");
            assert_eq!(report["advisory_only"], true);
            assert_eq!(report["reviewed_commit"], commit);
            assert_eq!(report["ledger_event_seq"], seq);
            assert_build(&report["build"], &pin, modules)?;
            let profile = &report["profile"];
            assert_eq!(profile["target_module"], TARGET);
            assert_eq!(profile["command_count"], count);
            let max = profile["max_command_heartbeats_raw"]
                .as_u64()
                .context("raw maximum heartbeat count")?;
            let total = profile["total_command_heartbeats_raw"]
                .as_u64()
                .context("raw total heartbeat count")?;
            // These fixtures consume thousands of raw allocation heartbeats,
            // not normalized heartbeats. Do not pin exact absolute values.
            assert!(
                (1_000..10_000_000).contains(&max),
                "unexpected maximum: {max}"
            );
            assert!(
                (max + 1..=count * max).contains(&total),
                "unexpected total: {total}"
            );
            Ok(HeartbeatSample {
                report,
                max_raw: max,
                total_raw: total,
            })
        };
    let collect_samples = |commit: &str,
                           modules: &[(&str, &str)],
                           count: u64|
     -> Result<[HeartbeatSample; SAMPLE_COUNT]> {
        let before = snapshot(&fixture)?;
        let sample = || -> Result<HeartbeatSample> {
            let observation = read_sample(commit, modules, count)?;
            assert_unchanged(&before, "successful profile")?;
            Ok(observation)
        };
        // Launch a separate native CLI process for every observation.
        Ok([sample()?, sample()?, sample()?])
    };

    // Count the target's two definitions and theorem, not its header or EOF.
    let baseline = collect_samples(&head, &MODULES, 3)?;
    let baseline_max = median(baseline.each_ref().map(|sample| sample.max_raw));
    let baseline_total = median(baseline.each_ref().map(|sample| sample.total_raw));
    assert!(
        baseline_max < 1_000_000,
        "unexpected baseline max: {baseline_max}"
    );
    // Compare raw counts for ordinary synchronous elaboration, not wall time.
    assert_eq!(baseline[0].max_raw, baseline[1].max_raw);
    assert_eq!(baseline[0].total_raw, baseline[1].total_raw);

    let before = snapshot(&fixture)?;
    rejects(
        profile(&head, &wrong_pin)?,
        "wrong compiler pin",
        "Lean compiler SHA-256 mismatch",
    );
    assert_unchanged(&before, "wrong compiler pin")?;

    // Add one real, substantially costlier elaboration to the same target.
    let heavier_source = format!("{}{HEAVY}", MODULES[1].1);
    let mut heavier_modules = MODULES;
    heavier_modules[1].1 = &heavier_source;
    let heavier_head = advance_main(&source, &repo, TARGET, &heavier_source, "add heavier proof")?;
    assert_ne!(heavier_head, head);
    let candidate = collect_samples(&heavier_head, &heavier_modules, 4)?;
    let candidate_max = median(candidate.each_ref().map(|sample| sample.max_raw));
    let candidate_total = median(candidate.each_ref().map(|sample| sample.total_raw));
    assert!(
        candidate_max > 10 * baseline_max && candidate_total > 10 * baseline_total,
        "expected larger raw medians: max {baseline_max} -> {candidate_max}, \
         total {baseline_total} -> {candidate_total}"
    );

    let baseline_json = fixture.join("baseline-heartbeats.json");
    let candidate_json = fixture.join("candidate-heartbeats.json");
    for (path, samples) in [(&baseline_json, &baseline), (&candidate_json, &candidate)] {
        serde_json::to_writer(
            fs::File::create(path)?,
            &samples.each_ref().map(|sample| &sample.report),
        )?;
    }
    // Baseline after writing the inputs, so both comparison invocations must
    // leave the JSON arrays, repository and ledger untouched.
    let before = snapshot(&fixture)?;
    for (min_delta, regressed) in [(1_000, true), (9_000_000_000_000_000u64, false)] {
        let output = Command::new(env!("CARGO_BIN_EXE_ai4math"))
            .current_dir(&root)
            .arg("t2-compare-heartbeats")
            .arg(&baseline_json)
            .arg(&candidate_json)
            .arg(min_delta.to_string())
            .arg("100")
            .output()?;
        assert_eq!(
            output.status.success(),
            !regressed,
            "comparison at raw delta {min_delta} exited {}:\nstdout: {}\nstderr: {}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        let report: Value = serde_json::from_slice(&output.stdout).with_context(|| {
            format!(
                "comparison at raw delta {min_delta} did not emit JSON:\nstdout: {}\nstderr: {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            )
        })?;
        assert_eq!(report["schema"], "ai4math.t2.heartbeat-comparison.v1");
        assert_eq!(report["advisory_only"], true);
        assert_eq!(report["regressed"], regressed);
        assert_eq!(report["target_module"], TARGET);
        assert_eq!(report["thresholds"]["min_delta_heartbeats_raw"], min_delta);
        assert_eq!(report["thresholds"]["min_increase_percent"], 100);
        assert_eq!(report["baseline"]["reviewed_commit"], head);
        assert_eq!(report["candidate"]["reviewed_commit"], heavier_head);
        assert_eq!(report["baseline"]["command_count"], 3);
        assert_eq!(report["candidate"]["command_count"], 4);
        assert_eq!(report["baseline"]["sample_count"], SAMPLE_COUNT);
        assert_eq!(report["candidate"]["sample_count"], SAMPLE_COUNT);
        assert_eq!(report["target_source_changed"], true);
        assert_eq!(report["target_prefix_changed"], true);
        for (metric, baseline_median, candidate_median) in [
            ("max_command", baseline_max, candidate_max),
            ("total_commands", baseline_total, candidate_total),
        ] {
            assert_eq!(
                report[metric]["baseline_median_heartbeats_raw"],
                baseline_median
            );
            assert_eq!(
                report[metric]["candidate_median_heartbeats_raw"],
                candidate_median
            );
            assert_eq!(report[metric]["regressed"], regressed);
        }
        assert_unchanged(&before, &format!("comparison at raw delta {min_delta}"))?;
    }

    // Break only the later module. The unchanged target must not yield a
    // partial report even though its predecessors and its own proof are valid.
    let broken_head = advance_main(&source, &repo, LATER, BROKEN, "break later Lean module")?;
    assert_ne!(broken_head, heavier_head);
    let before = snapshot(&fixture)?;
    rejects(
        profile(&broken_head, &pin)?,
        "broken later module",
        &format!("build isolated Lean module {LATER:?}"),
    );
    assert_unchanged(&before, "broken later module")?;
    rejects(
        profile(&head, &pin)?,
        "stale review",
        "reviewed T2 commit is not current main",
    );
    assert_unchanged(&before, "stale review")?;
    Ok(())
}
