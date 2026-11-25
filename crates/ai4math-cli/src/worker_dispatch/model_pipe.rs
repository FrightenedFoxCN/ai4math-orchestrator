//! Private one-shot operator pipe to a trusted, separately provisioned DSH Host.
//! The volatile lease fence remains in this coordinator; durable results are untrusted.
use super::*;

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

#[cfg(unix)]
pub(crate) fn run_model(
    root: &Path,
    repo: &Path,
    task: &str,
    claim: &str,
    role: Role,
    seconds: u64,
) -> Result<()> {
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
        let lease = store
            .claim_expected_task_with_submission_grace(
                Some(role_track(role)?),
                task,
                Duration::from_secs(seconds),
                Duration::from_secs(30),
                MODEL_ACTOR,
            )?
            .context("requested DSH model task is not next eligible in its track")?;
        if let Err(err) = check_lease(&lease, &subject) {
            let _ = store.finish_task_run(&lease, TaskRunOutcome::Failed, MODEL_ACTOR);
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
        let _ = store.finish_task_run(&lease, TaskRunOutcome::Failed, MODEL_ACTOR);
        return Err(
            error.context("DSH model suggestion was not recorded; no proof authority granted")
        );
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
