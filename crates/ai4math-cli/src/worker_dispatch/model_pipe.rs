//! Private one-shot operator pipe to a trusted, separately provisioned DSH Host.
//! The volatile lease fence remains in this coordinator; durable results are untrusted.
use super::*;
use ai4math_store::NewModelDispatchApproval;

const MODEL_ACTOR: Change<'static> = Change {
    actor: "operator:dsh-model",
    git_commit: None,
};

#[derive(Debug, PartialEq, Eq, Serialize)]
struct Packet<'a> {
    version: u8,
    task_id: &'a str,
    claim_id: &'a str,
    role: Role,
    engine: Engine,
    module_path: &'a str,
    target_decl: &'a str,
    base_commit: &'a str,
    max_wall_seconds: u64,
    informal_statement: &'a str,
    formal_header: &'a str,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Reply {
    task_id: String,
    claim_id: String,
    base_commit: String,
    role: Role,
    module_path: String,
    target_decl: String,
    suggestion: Suggestion,
    proof_verified: bool,
}

fn model_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 100
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
}

fn packet<'a>(
    subject: &'a Prepared,
    task: &'a str,
    claim: &'a str,
    role: Role,
    seconds: u64,
) -> Result<Packet<'a>> {
    let frozen = subject
        .frozen
        .as_ref()
        .context("missing frozen model target")?;
    ensure!(
        safe_text(&subject.informal_statement),
        "model statement exceeds its private context budget"
    );
    let target = subject.target.as_bytes();
    ensure!(
        target.len() <= 128
            && target
                .first()
                .is_some_and(|c| c.is_ascii_alphabetic() || *c == b'_')
            && target
                .iter()
                .all(|c| c.is_ascii_alphanumeric() || b"_'".contains(c)),
        "unsupported DSH model target identifier"
    );
    Ok(Packet {
        version: 1,
        task_id: task,
        claim_id: claim,
        role,
        engine: subject.engine,
        module_path: &subject.module,
        target_decl: &subject.target,
        base_commit: &subject.base,
        max_wall_seconds: seconds,
        informal_statement: &subject.informal_statement,
        formal_header: lean_frozen_header(frozen)?,
    })
}

fn validate_reply(raw: &[u8], expected: &Packet<'_>, subject: &Prepared) -> Result<String> {
    let reply: Reply = serde_json::from_slice(raw)
        .map_err(|_| anyhow::anyhow!("invalid DSH model result envelope"))?;
    ensure!(
        !reply.proof_verified
            && reply.task_id == expected.task_id
            && reply.claim_id == expected.claim_id
            && reply.base_commit == expected.base_commit
            && reply.role == expected.role
            && reply.module_path == expected.module_path
            && reply.target_decl == expected.target_decl,
        "DSH model result does not match its assigned task, Claim, role and Git snapshot"
    );
    let suggestion = serde_json::to_vec(&reply.suggestion)?;
    let checked = check_result(expected.role, subject, &suggestion)?;
    Ok(serde_json::to_string(&checked)?)
}

pub(crate) struct ModelApprovalOptions<'a> {
    pub task: &'a str,
    pub claim: &'a str,
    pub role: Role,
    pub model: &'a str,
    pub seconds: u64,
    pub max_model_calls: u64,
}

pub(crate) fn approve_model(
    root: &Path,
    repo: &Path,
    reviewed: &str,
    options: ModelApprovalOptions<'_>,
) -> Result<()> {
    let ModelApprovalOptions {
        task,
        claim,
        role,
        model,
        seconds,
        max_model_calls,
    } = options;
    let mut store = open_managed_state(root, false)?;
    let git = PromotionRepo::try_lock(repo)?;
    let subject = prepare(root, &git, &store, task, claim, role, seconds)?;
    packet(&subject, task, claim, role, seconds)?;
    ensure!(
        subject.engine == Engine::Lean4 && subject.base == reviewed && git.main_head()? == reviewed,
        "approval requires the current reviewed Lean snapshot"
    );
    print_json(store.approve_model_dispatch(
        NewModelDispatchApproval {
            task_id: task,
            claim_id: claim,
            role: role.as_str(),
            repo: repo_name(&git)?,
            base_commit: &subject.base,
            model,
            max_wall_seconds: seconds,
            max_model_calls,
        },
        MODEL_ACTOR,
    )?)
}

pub(crate) fn model_dispatch_status(root: &Path, task: &str) -> Result<()> {
    let store = open_managed_state(root, true)?;
    print_json(
        store
            .get_model_dispatch(task)?
            .context("unknown model dispatch approval")?,
    )
}

pub(crate) fn cancel_model_dispatch(root: &Path, task: &str) -> Result<()> {
    let mut store = open_managed_state(root, false)?;
    print_json(store.cancel_model_dispatch(task, MODEL_ACTOR)?)
}

pub(crate) fn reconcile_model_dispatch(root: &Path, task: &str) -> Result<()> {
    let mut store = open_managed_state(root, false)?;
    print_json(store.reconcile_model_dispatch(task, MODEL_ACTOR)?)
}

#[cfg(unix)]
pub(crate) fn run_approved_model(
    root: &Path,
    repo: &Path,
    options: ModelApprovalOptions<'_>,
) -> Result<()> {
    use ai4math_store::ModelDispatchState;
    let ModelApprovalOptions {
        task,
        claim,
        role,
        model,
        seconds,
        max_model_calls,
    } = options;
    let approval = open_managed_state(root, true)?
        .get_model_dispatch(task)?
        .context("unknown model dispatch approval")?;
    ensure!(
        approval.state == ModelDispatchState::Approved,
        "the model approval cannot be dispatched again"
    );
    ensure!(
        approval.model == model
            && approval.max_model_calls == max_model_calls
            && approval.claim_id == claim
            && approval.role == role.as_str()
            && approval.max_wall_seconds == seconds,
        "assignment identity, route or budget differs from the dispatch approval"
    );
    run_model_inner(
        root,
        repo,
        Run {
            task,
            claim,
            role,
            seconds,
            approved_base: Some(&approval.base_commit),
        },
    )
}

#[cfg(not(unix))]
pub(crate) fn run_approved_model(_: &Path, _: &Path, _: ModelApprovalOptions<'_>) -> Result<()> {
    bail!("DSH model coordinator requires Unix stdin/stdout pipes")
}

#[cfg(unix)]
struct Run<'a> {
    task: &'a str,
    claim: &'a str,
    role: Role,
    seconds: u64,
    approved_base: Option<&'a str>,
}

#[cfg(unix)]
pub(crate) fn run_model(
    root: &Path,
    repo: &Path,
    task: &str,
    claim: &str,
    role: Role,
    seconds: u64,
) -> Result<()> {
    run_model_inner(
        root,
        repo,
        Run {
            task,
            claim,
            role,
            seconds,
            approved_base: None,
        },
    )
}

#[cfg(unix)]
fn run_model_inner(root: &Path, repo: &Path, request: Run<'_>) -> Result<()> {
    let Run {
        task,
        claim,
        role,
        seconds,
        approved_base,
    } = request;
    use std::{
        io::{self, IsTerminal},
        time::Instant,
    };
    ensure!(
        (1..=60).contains(&seconds),
        "DSH model wall seconds must be in 1..=60"
    );
    ensure!(
        matches!(role, Role::ProverWorker | Role::IndependentReviewer),
        "unsupported DSH model role"
    );
    ensure!(
        model_id(task) && model_id(claim),
        "invalid DSH model task or Claim ID"
    );
    ensure!(
        !io::stdin().is_terminal() && !io::stdout().is_terminal(),
        "DSH model coordinator requires private stdin/stdout pipes"
    );
    // Validate descriptors before acquiring a lease. No native execution here:
    // T1 promotion is a separate operator action under the native project gate.
    let mut wire = timed_io::Wire::open()?;
    let mut store = open_managed_state(root, false)?;
    let (subject, lease, deadline, submission_deadline) = {
        let git = PromotionRepo::try_lock(repo)?;
        let subject = prepare(root, &git, &store, task, claim, role, seconds)?;
        packet(&subject, task, claim, role, seconds)?;
        let deadline = Instant::now() + Duration::from_secs(seconds);
        let submission_deadline = deadline + Duration::from_secs(30);
        let lease = if let Some(base) = approved_base {
            ensure!(
                subject.base == base && git.main_head()? == base,
                "approved Git snapshot is no longer current; no model task dispatched"
            );
            // Approval consumption and the lease are committed together, before
            // the host can read a context packet or make a model call.
            let (_, lease) = store.lease_and_dispatch_model_once(
                task,
                repo_name(&git)?,
                base,
                Duration::from_secs(30),
                MODEL_ACTOR,
            )?;
            lease
        } else {
            store
                .claim_expected_task_with_submission_grace(
                    Some(role_track(role)?),
                    task,
                    Duration::from_secs(seconds),
                    Duration::from_secs(30),
                    MODEL_ACTOR,
                )?
                .context("requested DSH model task is not next eligible in its track")?
        };
        if let Err(err) = check_lease(&lease, &subject) {
            if approved_base.is_some() {
                let _ = store.release_model_dispatch_lease(&lease);
            } else {
                let _ = store.finish_task_run(&lease, TaskRunOutcome::Failed, MODEL_ACTOR);
            }
            return Err(err);
        }
        (subject, lease, deadline, submission_deadline)
    }; // A breaker or trusted Git update can proceed while the Host is working.
    let outcome = (|| {
        let expected = packet(&subject, task, claim, role, seconds)?;
        wire.send(json!({"packet":expected}), deadline)?;
        let raw = wire.receive(deadline)?;
        let result_json = validate_reply(&raw, &expected, &subject)?;
        let git = PromotionRepo::try_lock(repo)?;
        let current = prepare(root, &git, &store, task, claim, role, seconds)?;
        ensure!(
            current.payload == subject.payload && git.main_head()? == subject.base,
            "approved DSH dispatch inputs changed while model task was running"
        );
        ensure!(
            Instant::now() < submission_deadline,
            "DSH model submission grace elapsed before persistence"
        );
        store.finish_task_run(
            &lease,
            TaskRunOutcome::ModelCandidate {
                base_commit: &subject.base,
                claim_id: claim,
                role: role.as_str(),
                result_json: &result_json,
            },
            MODEL_ACTOR,
        )
    })();
    if let Err(error) = outcome {
        if approved_base.is_some() {
            let _ = store.release_model_dispatch_lease(&lease);
        } else {
            let _ = store.finish_task_run(&lease, TaskRunOutcome::Failed, MODEL_ACTOR);
        }
        return Err(
            error.context("DSH model suggestion was not recorded; no proof authority granted")
        );
    }
    if approved_base.is_some() {
        // Candidate is already committed. A projection failure must not make the
        // caller retry; it can reconcile by ID without starting a model.
        let _ = store.reconcile_model_dispatch(task, MODEL_ACTOR);
    }
    // A lost acknowledgement is not a new task attempt. The operator can read
    // task-result by ID; the candidate is already durable and cannot be replayed.
    wire.send(
        json!({"task_id":task,"base_commit":subject.base,"role":role,
        "source":"dsh_model","suggestion_recorded":true,"proof_verified":false}),
        Instant::now() + Duration::from_secs(2),
    )
}

#[cfg(not(unix))]
pub(crate) fn run_model(_: &Path, _: &Path, _: &str, _: &str, _: Role, _: u64) -> Result<()> {
    bail!("DSH model coordinator requires Unix stdin/stdout pipes")
}

#[cfg(unix)]
mod timed_io;
