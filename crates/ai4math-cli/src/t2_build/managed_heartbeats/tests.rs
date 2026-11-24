use super::*;
use crate::t2_build::tests::{Fixture, git};
use ai4math_broker::{LeanBuildAudit, LeanBuildModuleAudit, LeanHeartbeatProfile};
use ai4math_core::Engine;
use sha2::{Digest, Sha256};
use std::{cell::Cell, fs};

const MODULE: &str = "Audit.lean";
const MAIN: &str = "refs/heads/main";
const PIN: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn request<'a>(f: &'a Fixture, base: &'a str, reviewed: &'a str) -> ManagedHeartbeatRequest<'a> {
    ManagedHeartbeatRequest {
        root: &f.root,
        repo: &f.bare,
        target: MODULE,
        baseline: base,
        reviewed,
        executable: Path::new("/never-launched"),
        compiler_pin: PIN,
        raw_delta: "2",
        relative_percent: "20",
    }
}

fn measured(inputs: &[(&str, &str)], raw: u64) -> LeanHeartbeatAudit {
    LeanHeartbeatAudit {
        build: LeanBuildAudit {
            source_set_sha256: ai4math_broker::ordered_source_set_sha256(inputs),
            compiler_sha256: PIN.into(),
            compile_ms: 1,
            modules: inputs
                .iter()
                .map(|&(path, source)| LeanBuildModuleAudit {
                    path: path.into(),
                    source_sha256: format!("{:x}", Sha256::digest(source.as_bytes())),
                    compile_ms: 1,
                })
                .collect(),
        },
        profile: LeanHeartbeatProfile {
            target_module: MODULE.into(),
            command_count: 1,
            max_command_heartbeats_raw: raw,
            total_command_heartbeats_raw: raw,
        },
    }
}

#[test]
fn same_generation_runs_three_interleaved_pairs_with_one_absolute_deadline() -> Result<()> {
    let f = Fixture::new()?;
    git(&f.bare, &["update-ref", MAIN, &f.next, &f.initial])?;
    let calls = Cell::new(0);
    let mut first = None;
    let res = sample_and_compare(&request(&f, &f.initial, &f.next), 2, 20, |engine| {
        assert_eq!(engine, Engine::Lean4);
        Ok(|modules: &[(&str, &str)], deadline| {
            assert_eq!(*first.get_or_insert(deadline), deadline);
            let index = calls.get();
            calls.set(index + 1);
            Ok(measured(modules, 10 + if index % 2 == 0 { 0 } else { 2 }))
        })
    })?;
    assert_eq!(calls.get(), 6);
    let j = serde_json::to_value(res)?;
    assert_eq!(j["comparison"]["baseline"]["reviewed_commit"], f.initial);
    assert_eq!(j["comparison"]["candidate"]["reviewed_commit"], f.next);
    assert_eq!(
        j["comparison"]["max_command"]["baseline_median_heartbeats_raw"],
        10
    );
    assert_eq!(
        j["comparison"]["max_command"]["candidate_median_heartbeats_raw"],
        12
    );
    assert_eq!(j["comparison"]["regressed"], true);
    Ok(())
}

#[test]
fn drift_from_either_store_never_starts_the_next_native_sample() -> Result<()> {
    for move_git in [false, true] {
        let f = Fixture::new()?;
        git(&f.bare, &["update-ref", MAIN, &f.next, &f.initial])?;
        let calls = Cell::new(0);
        let err = sample_and_compare(&request(&f, &f.initial, &f.next), 2, 20, |_| {
            Ok(|modules: &[(&str, &str)], _| {
                calls.set(calls.get() + 1);
                assert_eq!(calls.get(), 1);
                if move_git {
                    git(&f.bare, &["update-ref", MAIN, &f.initial, &f.next])?;
                } else {
                    f.append_from_other_connection()?;
                }
                Ok(measured(modules, 10))
            })
        })
        .err()
        .context("expected drift rejection")?;
        assert_eq!(calls.get(), 1);
        assert!(format!("{err:#}").contains("changed during heartbeat sampling"));
    }
    Ok(())
}

#[test]
fn rejects_equal_future_and_grafted_baselines_before_native_preparation() -> Result<()> {
    let f = Fixture::new()?;
    for base in [&f.initial, &f.next] {
        let reject =
            |_: Engine| -> Result<fn(&[(&str, &str)], Instant) -> Result<LeanHeartbeatAudit>> {
                panic!("invalid ancestry prepared native")
            };
        let err = sample_and_compare(&request(&f, base, &f.initial), 2, 20, reject)
            .err()
            .context("expected ancestor refusal")?;
        assert!(format!("{err:#}").contains("ancestor"));
    }
    fs::write(
        f.bare.join("info/grafts"),
        format!("{} {}\n", f.initial, f.next),
    )?;
    let err = sample_and_compare(
        &request(&f, &f.next, &f.initial),
        2,
        20,
        |_| -> Result<fn(&[(&str, &str)], Instant) -> Result<LeanHeartbeatAudit>> {
            panic!("Git graft was accepted as actual ancestry")
        },
    )
    .err()
    .context("expected grafted ancestry refusal")?;
    assert!(format!("{err:#}").contains("ancestor"));
    Ok(())
}
