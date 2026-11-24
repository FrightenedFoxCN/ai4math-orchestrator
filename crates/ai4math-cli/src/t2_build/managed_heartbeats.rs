//! Fresh same-host baseline and candidate captures for one managed Lean generation.
use super::{HeartbeatReport, with_generation};
use crate::{
    t2_compare_io::threshold,
    t2_heartbeat_compare::{self, HeartbeatComparison},
};
use ai4math_broker::{Broker, LeanHeartbeatAudit};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use std::{
    path::Path,
    time::{Duration, Instant},
};

const SAMPLE_PAIRS: usize = 3;
const COMBINED_SOURCE_BYTES: usize = 32 * 1024 * 1024;
const SAMPLING_BUDGET: Duration = Duration::from_secs(240);

pub(crate) struct ManagedHeartbeatRequest<'a> {
    pub root: &'a Path,
    pub repo: &'a Path,
    pub target: &'a str,
    pub baseline: &'a str,
    pub reviewed: &'a str,
    pub executable: &'a Path,
    pub compiler_pin: &'a str,
    pub raw_delta: &'a str,
    pub relative_percent: &'a str,
}

#[derive(Serialize)]
struct ManagedHeartbeatComparison {
    schema: &'static str,
    advisory_only: bool,
    ledger_event_seq: i64,
    comparison: HeartbeatComparison,
}

enum Execution {
    Operator,
    #[cfg(unix)]
    Daemon,
}

pub(crate) fn compare_managed_heartbeats(request: ManagedHeartbeatRequest<'_>) -> Result<()> {
    let report = collect(&request, Execution::Operator)?;
    let regressed = report.comparison.regressed;
    crate::print_json(report)?;
    ensure!(
        !regressed,
        "measured managed T2 heartbeat increase meets both comparison thresholds"
    );
    Ok(())
}

#[cfg(unix)]
pub(crate) fn compare_for_daemon(request: ManagedHeartbeatRequest<'_>) -> Result<()> {
    use std::io::Write;

    crate::t3::arm_daemon_lifeline("T2")?;
    let report = collect(&request, Execution::Daemon)?;
    let (max_command_regressed, total_commands_regressed) = report.comparison.metric_regressions();
    // A measured regression is still an admissible observation. The only worker
    // output is this private protocol object; native measurements remain local.
    let summary = serde_json::json!({
        "schema": "ai4math.t2.resident-heartbeats.v3",
        "advisory_only": true,
        "reviewed_commit": request.reviewed,
        "ledger_event_seq": report.ledger_event_seq,
        "baseline_commit": request.baseline,
        "compiler_sha256": request.compiler_pin,
        "regressed": report.comparison.regressed,
        "max_command_regressed": max_command_regressed,
        "total_commands_regressed": total_commands_regressed,
    });
    writeln!(
        std::io::stdout().lock(),
        "{}",
        serde_json::to_string(&summary)?
    )?;
    Ok(())
}

fn collect(
    request: &ManagedHeartbeatRequest<'_>,
    execution: Execution,
) -> Result<ManagedHeartbeatComparison> {
    let raw_delta = threshold(request.raw_delta)?;
    let percent = threshold(request.relative_percent)?;
    t2_heartbeat_compare::validate_thresholds(raw_delta, percent)?;
    ensure!(
        request.compiler_pin.len() == 64
            && request
                .compiler_pin
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')),
        "expected Lean compiler SHA-256 must be 64 lowercase hex digits"
    );
    sample_and_compare(request, raw_delta, percent, |engine| {
        let toolchain = super::trusted_toolchain(engine, request.executable, None)?;
        ensure!(
            toolchain.compiler_sha256() == request.compiler_pin,
            "Lean compiler SHA-256 mismatch"
        );
        let toolchain = match execution {
            Execution::Operator => toolchain,
            #[cfg(unix)]
            Execution::Daemon => toolchain.with_supervised_process_group()?,
        };
        let broker = Broker::new();
        Ok(move |sources: &[(&str, &str)], deadline| {
            broker.audit_lean_heartbeats_until(
                &toolchain,
                sources,
                request.target,
                request.compiler_pin,
                deadline,
            )
        })
    })
}

fn sample_and_compare<F>(
    request: &ManagedHeartbeatRequest<'_>,
    raw_delta: u64,
    percent: u64,
    prepare: impl FnOnce(ai4math_core::Engine) -> Result<F>,
) -> Result<ManagedHeartbeatComparison>
where
    F: FnMut(&[(&str, &str)], Instant) -> Result<LeanHeartbeatAudit>,
{
    with_generation(
        request.root,
        request.repo,
        request.target,
        request.reviewed,
        |generation| {
            let git = generation.git;
            ensure!(
                request.baseline != request.reviewed,
                "heartbeat baseline must be a distinct ancestor of reviewed main"
            );
            ensure!(
                git.is_ungrafted_ancestor(request.baseline, request.reviewed)?,
                "heartbeat baseline must be an ancestor of reviewed main"
            );
            let baseline = git
                .snapshot(request.target, Some(request.baseline))
                .context("read managed heartbeat baseline")?;
            let candidate = git
                .snapshot(request.target, Some(request.reviewed))
                .context("read managed heartbeat candidate")?;
            let mut source_bytes = COMBINED_SOURCE_BYTES;
            let baseline_files = baseline.project_with_byte_budget(&mut source_bytes)?;
            let candidate_files = candidate.project_with_byte_budget(&mut source_bytes)?;
            ensure!(
                baseline_files
                    .iter()
                    .map(|(path, _)| path)
                    .eq(candidate_files.iter().map(|(path, _)| path)),
                "managed heartbeat baseline and candidate ordered module paths must match"
            );
            let baseline_sources: Vec<_> = baseline_files
                .iter()
                .map(|(path, source)| (*path, source.as_str()))
                .collect();
            let candidate_sources: Vec<_> = candidate_files
                .iter()
                .map(|(path, source)| (*path, source.as_str()))
                .collect();
            // Inherit the same absolute deadline into every build and probe.
            let deadline = Instant::now() + SAMPLING_BUDGET;
            generation
                .check_current("managed main or ledger changed before heartbeat sampling; retry")?;
            let mut observe = prepare(generation.engine)?;
            let mut baseline_reports = Vec::with_capacity(SAMPLE_PAIRS);
            let mut candidate_reports = Vec::with_capacity(SAMPLE_PAIRS);
            for pair in 1..=SAMPLE_PAIRS {
                for (label, commit, sources, reports) in [
                    (
                        "baseline",
                        request.baseline,
                        &baseline_sources,
                        &mut baseline_reports,
                    ),
                    (
                        "candidate",
                        request.reviewed,
                        &candidate_sources,
                        &mut candidate_reports,
                    ),
                ] {
                    generation.check_current(
                        "managed main or ledger changed during heartbeat sampling; retry",
                    )?;
                    ensure!(
                        Instant::now() < deadline,
                        "managed heartbeat sampling wall-clock budget exhausted"
                    );
                    let observation = observe(sources, deadline).with_context(|| {
                        format!("managed Lean heartbeat {label} sample {pair} failed")
                    })?;
                    reports.push(HeartbeatReport::new(commit, generation.seq(), observation));
                }
            }
            let comparison = t2_heartbeat_compare::compare(
                &serde_json::to_vec(&baseline_reports)?,
                &serde_json::to_vec(&candidate_reports)?,
                raw_delta,
                percent,
            )?;
            generation
                .check_current("managed main or ledger changed during heartbeat sampling; retry")?;
            ensure!(
                Instant::now() < deadline,
                "managed heartbeat sampling wall-clock budget exhausted"
            );
            Ok(ManagedHeartbeatComparison {
                schema: "ai4math.t2.managed-heartbeat-comparison.v1",
                advisory_only: true,
                ledger_event_seq: generation.seq(),
                comparison,
            })
        },
    )
}

#[cfg(all(unix, test))]
mod tests;
