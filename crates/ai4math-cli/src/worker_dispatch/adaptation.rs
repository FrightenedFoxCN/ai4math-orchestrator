//! An explicitly selected sediment approval can run one local ProverWorker.
//! No scheduler discovery, promotion, or model-host route is provided here.
use super::{
    Prepared, RESULT_CAP, WORKER_ACTOR, check_lease, check_result, prepare, validate_native_worker,
};
use crate::{
    managed_native, print_json, promotion::PromotionRepo, wiki_archive::adapt::revalidate,
};
use ai4math_broker::run_task_process;
use ai4math_core::Role;
use ai4math_project::open_managed_state;
use ai4math_store::{SedimentAdaptationPlan, StateStore, TaskRunOutcome};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{path::Path, time::Duration};

const ROLE: Role = Role::ProverWorker;

fn prepare_adaptation(
    root: &Path,
    git: &PromotionRepo,
    state: &StateStore,
    approval: &str,
    task_id: &str,
    seconds: u64,
) -> Result<(Prepared, SedimentAdaptationPlan)> {
    let (plan, historical_proof_body) = revalidate(root, git, state, approval)?;
    let mut prepared = prepare(root, git, state, task_id, &plan.target_claim, ROLE, seconds)?;
    ensure!(
        prepared.base == plan.reviewed_commit
            && prepared.module == plan.target_module
            && prepared.target == plan.target_decl
            && state.status()?.last_event_seq == plan.expected_event_seq,
        "frozen adaptation dispatch bindings changed while preparing worker context"
    );
    let mut request: Value = serde_json::from_slice(&prepared.payload)?;
    request
        .as_object_mut()
        .context("worker request is not an object")?
        .insert(
            "adaptation_reference".into(),
            json!({
                "archive_id": plan.archive_id,
                "origin_claim": plan.origin_claim,
                "verified_at_commit": plan.promoted_commit,
                "historical_proof_body": historical_proof_body,
                "current_proof_authority": false,
            }),
        );
    prepared.payload = serde_json::to_vec(&request)?;
    ensure!(
        prepared.payload.len() <= 1024 * 1024,
        "bounded adaptation worker request exceeds 1 MiB"
    );
    Ok((prepared, plan))
}

pub(crate) fn run_adaptation(
    root: &Path,
    repo: &Path,
    approval: &str,
    exe: &Path,
    sha: &str,
    seconds: u64,
) -> Result<()> {
    validate_native_worker(exe, sha, seconds)?;
    let task_id = StateStore::sediment_adaptation_task_id(approval)?;
    let _native = managed_native(root)?;
    let mut state = open_managed_state(root, false)?;
    let (subject, plan, lease) = {
        let git = PromotionRepo::try_lock(repo)?;
        let (subject, plan) = prepare_adaptation(root, &git, &state, approval, &task_id, seconds)?;
        ensure!(
            git.main_head()? == plan.reviewed_commit,
            "main changed before the adaptation lease"
        );
        let lease = state
            .lease_sediment_adaptation(
                approval,
                &plan.repo,
                &plan.reviewed_commit,
                plan.expected_event_seq,
                Duration::from_secs(seconds + 30),
            )?
            .context("approved adaptation is completed, blocked, busy or without capacity")?;
        if let Err(err) = check_lease(&lease, &subject) {
            let _ = state.finish_task_run(&lease, TaskRunOutcome::Failed, WORKER_ACTOR);
            return Err(err);
        }
        (subject, plan, lease)
    };
    let result = (|| -> Result<()> {
        let raw = run_task_process(exe, sha, &subject.payload, Duration::from_secs(seconds))?;
        let result_json = serde_json::to_string(&check_result(ROLE, &subject, &raw)?)?;
        ensure!(
            result_json.len() <= RESULT_CAP,
            "normalized adaptation suggestion exceeds 32 KiB"
        );
        let git = PromotionRepo::try_lock(repo)?;
        let (current, current_plan) =
            prepare_adaptation(root, &git, &state, approval, &task_id, seconds)?;
        ensure!(
            current.base == subject.base
                && current.payload == subject.payload
                && git.main_head()? == plan.reviewed_commit,
            "approved adaptation inputs changed during worker execution"
        );
        // Store checks the scoped base, Claim, native role, and live fence again
        // in the result transaction. No candidate is interpreted as proof here.
        state.finish_task_run(
            &lease,
            TaskRunOutcome::Candidate {
                base_commit: &current_plan.reviewed_commit,
                claim_id: &current_plan.target_claim,
                role: ROLE.as_str(),
                executable_sha256: sha,
                result_json: &result_json,
            },
            WORKER_ACTOR,
        )
    })();
    if let Err(err) = result {
        let _ = state.finish_task_run(&lease, TaskRunOutcome::Failed, WORKER_ACTOR);
        return Err(
            err.context("adaptation suggestion was not accepted; no proof authority granted")
        );
    }
    print_json(
        json!({"approval_sha256":approval,"task_id":task_id,"base_commit":subject.base,
        "role":ROLE,"suggestion_recorded":true,"proof_verified":false}),
    )
}
