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
        baseline: HeartbeatBaseline::Commit(base),
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
    let (res, release) = sample_and_compare(&request(&f, &f.initial, &f.next), 2, 20, |engine| {
        assert_eq!(engine, Engine::Lean4);
        Ok(|modules: &[(&str, &str)], deadline| {
            assert_eq!(*first.get_or_insert(deadline), deadline);
            let index = calls.get();
            calls.set(index + 1);
            Ok(measured(modules, 10 + if index % 2 == 0 { 0 } else { 2 }))
        })
    })?;
    assert_eq!(calls.get(), 6);
    assert!(release.is_none());
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

#[test]
fn release_baseline_pin_is_checked_before_and_after_sampling() -> Result<()> {
    use crate::{promotion::PromotionRepo, worker_dispatch::private_state_path};
    use ai4math_store::StateStore;
    let policy = br#"[{"target_module":"Audit.lean","declaration":"N.audit"}]"#;
    let f = Fixture::with_release_policy(Some(policy))?;
    let seq = StateStore::open_readonly(private_state_path(&f.root)?)?
        .status()?
        .last_event_seq;
    let (tag_ref, oid) = {
        let repo = PromotionRepo::try_lock_existing(&f.bare)?;
        let snapshot = repo.snapshot(MODULE, Some(&f.initial))?;
        let files = snapshot.project()?;
        let sources: Vec<_> = files
            .iter()
            .map(|(path, source)| (*path, source.as_str()))
            .collect();
        // Canonical field order as written by the release publisher. This is
        // synthetic metadata for coordinator lifecycle, not verifier evidence.
        let payload = format!(
            concat!(
                "{{\"schema\":\"ai4math.t2.local-release-evidence.v1\",\"reviewed_commit\":\"{}\",",
                "\"ledger_event_seq\":{},\"policy_sha256\":\"{:x}\",\"trust_debt_findings\":0,",
                "\"source_set_sha256\":\"{}\",\"compiler_sha256\":\"{}\",\"exporter_sha256\":\"{}\",\"checker_sha256\":\"{}\",",
                "\"checks\":[{{\"target_module\":\"Audit.lean\",\"declaration\":\"N.audit\",\"export_sha256\":\"{}\"}}]}}"
            ),
            f.initial,
            seq,
            Sha256::digest(policy),
            ai4math_broker::ordered_source_set_sha256(&sources),
            PIN,
            "1".repeat(64),
            "2".repeat(64),
            "3".repeat(64)
        );
        repo.create_release_tag("milestone", &f.initial, payload.as_bytes())?
    };
    git(&f.bare, &["update-ref", MAIN, &f.next, &f.initial])?;
    let mut req = request(&f, &f.initial, &f.next);
    req.baseline = HeartbeatBaseline::LocalRelease {
        label: "milestone",
        oid: &oid,
    };
    let calls = Cell::new(0);
    let (_, pin) = sample_and_compare(&req, 2, 20, |_| {
        Ok(|sources: &[(&str, &str)], _| {
            calls.set(calls.get() + 1);
            Ok(measured(sources, 42))
        })
    })?;
    let pin = pin.context("expected pinned tag in report")?;
    assert_eq!(pin.baseline_tag_ref, tag_ref);
    assert_eq!(pin.baseline_tag_oid, oid);
    assert_eq!(pin.baseline_release_ledger_event_seq, seq);
    assert_eq!(calls.get(), 6);

    calls.set(0);
    let err = sample_and_compare(&req, 2, 20, |_| {
        Ok(|sources: &[(&str, &str)], _| {
            calls.set(calls.get() + 1);
            if calls.get() == 6 {
                git(&f.bare, &["update-ref", "-d", &tag_ref, &oid])?;
            }
            Ok(measured(sources, 42))
        })
    })
    .err()
    .context("moved tag still emitted a comparison")?;
    assert!(format!("{err:#}").contains("tag changed during heartbeat sampling"));
    assert_eq!(calls.get(), 6);
    let err = sample_and_compare(
        &req,
        2,
        20,
        |_| -> Result<fn(&[(&str, &str)], Instant) -> Result<LeanHeartbeatAudit>> {
            panic!("missing tag prepared native sampling")
        },
    )
    .err()
    .context("missing tag was accepted")?;
    assert!(format!("{err:#}").contains("release tag"));
    Ok(())
}
