//! Operator-only local worker adapter. This wire protocol carries suggestions,
//! not proof receipts, capabilities, or trusted reviewer tickets.
use crate::{HOLE, bound_engine, print_json, promotion::PromotionRepo};
use ai4math_broker::run_task_process;
use ai4math_core::{
    Engine, FrozenTarget, ProofBodySubmission, Role, Trust, proof::lean_frozen_header,
    render_proof_body,
};
use ai4math_store::{Change, StateStore, TaskLease, TaskRunOutcome, TaskTrack};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{path::Path, time::Duration};

mod model_pipe;
pub(crate) use model_pipe::run_model;

const RESULT_CAP: usize = 32 * 1024;
const WORKER_ACTOR: Change<'static> = Change {
    actor: "operator:local-worker",
    git_commit: None,
};

#[derive(Debug, Serialize)]
struct Request<'a> {
    version: u8,
    task_id: &'a str,
    role: Role,
    claim_id: &'a str,
    engine: Engine,
    module_path: &'a str,
    target_decl: &'a str,
    base_commit: &'a str,
    max_wall_seconds: u64,
    #[serde(flatten)]
    input: Input<'a>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
enum Input<'a> {
    Explore {
        informal_statement: &'a str,
    },
    ReviewHeader {
        informal_statement: &'a str,
        formal_header: &'a str,
    },
    ProposeBody {
        informal_statement: &'a str,
        frozen_target: &'a FrozenTarget,
    },
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Suggestion {
    Findings {
        summary: String,
    },
    Review {
        verdict: ReviewVerdict,
        rationale: String,
    },
    ProofBody {
        proof_body: String,
    },
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum ReviewVerdict {
    LooksConsistent,
    NeedsChanges,
    InsufficientContext,
}

struct Prepared {
    payload: Vec<u8>,
    base: String,
    module: String,
    target: String,
    engine: Engine,
    frozen: Option<FrozenTarget>,
    informal_statement: String,
}

fn role_track(role: Role) -> Result<TaskTrack> {
    match role {
        Role::Explorer => Ok(TaskTrack::Exploration),
        Role::IndependentReviewer | Role::ProverWorker => Ok(TaskTrack::Infrastructure),
        _ => bail!("role is not supported by the limited local worker adapter"),
    }
}
fn repo_name(git: &PromotionRepo) -> Result<&str> {
    git.path().to_str().context("repository path is not UTF-8")
}
/// The owner-only runtime path protects private suggestions from other OS
/// accounts. The worker has a separate Seatbelt boundary, not this UID check.
#[cfg(unix)]
fn private_state_path(root: &Path) -> Result<std::path::PathBuf> {
    use std::{fs, os::unix::fs::MetadataExt};
    let runtime = root.join(".runtime");
    let owner = unsafe { libc::geteuid() };
    let info = fs::symlink_metadata(&runtime)?;
    ensure!(
        info.is_dir() && info.uid() == owner && info.mode() & 0o077 == 0,
        "task dispatch requires an owner-only .runtime directory (mode 0700), not a symlink"
    );
    for name in ["state.sqlite", "state.sqlite-wal", "state.sqlite-shm"] {
        let path = runtime.join(name);
        match fs::symlink_metadata(path) {
            Ok(info) => ensure!(
                info.is_file()
                    && info.uid() == owner
                    && info.nlink() == 1
                    && info.mode() & 0o022 == 0,
                "task dispatch requires private owner-controlled SQLite files"
            ),
            Err(e) if name != "state.sqlite" && e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
    }
    Ok(runtime.join("state.sqlite"))
}

#[cfg(not(unix))]
fn private_state_path(_: &Path) -> Result<std::path::PathBuf> {
    bail!("task dispatch requires an owner-only Unix runtime")
}

fn require_available(git: &PromotionRepo, store: &StateStore) -> Result<()> {
    ensure!(
        store.managed_repo()?.as_deref() == Some(repo_name(git)?),
        "bind this ledger to the reviewed repository before dispatch"
    );
    ensure!(
        store.promotion_intent()?.is_none() && store.pivot_intent()?.is_none(),
        "recover the pending Git transaction before dispatching"
    );
    Ok(())
}

pub(crate) fn bind(root: &Path, repo: &Path, module: &str, reviewed: &str) -> Result<()> {
    bound_engine(root, module)?;
    let git = PromotionRepo::lock(repo)?;
    let snap = git.snapshot(module, None)?;
    ensure!(
        snap.base() == reviewed && git.main_head()? == reviewed,
        "binding requires the exact reviewed main OID"
    );
    let mut store = StateStore::open(private_state_path(root)?)?;
    store.bind_task_repository(repo_name(&git)?, reviewed)?;
    print_json(json!({"bound":true,"base_commit":reviewed}))
}

fn prepare(
    root: &Path,
    git: &PromotionRepo,
    store: &StateStore,
    task_id: &str,
    claim_id: &str,
    role: Role,
    seconds: u64,
) -> Result<Prepared> {
    require_available(git, store)?;
    let claim = store
        .claim(claim_id)?
        .context("unknown Claim for dispatch")?;
    ensure!(
        !claim.stale
            && !claim.effective_stale
            && !matches!(claim.effective_trust, Trust::L2b | Trust::L3),
        "Claim is stale or invalid; reconcile it before dispatch"
    );
    let module = claim.module_path.context("Claim has no frozen module")?;
    let qualified = claim.formal_target.context("Claim has no formal target")?;
    let (registered_module, target) = qualified
        .split_once('#')
        .context("formal target is not module-qualified")?;
    ensure!(
        registered_module == module && !target.is_empty() && !target.contains('#'),
        "Claim has an invalid module-qualified target"
    );
    let target = target.to_owned();
    let engine = bound_engine(root, &module)?;
    ensure!(
        claim.engine == engine.as_str(),
        "Claim and project engine differ"
    );
    let snap = git.snapshot(&module, None)?;
    let statement = store.claim_statement_for_dispatch(claim_id)?;
    let frozen = if role != Role::Explorer {
        let project = snap.project()?;
        let (_, source) = project
            .iter()
            .find(|(path, _)| *path == module)
            .context("frozen module not in managed Git snapshot")?;
        let (prefix, suffix) = source
            .split_once(HOLE)
            .context("target module must contain a frozen proof slot")?;
        ensure!(
            !suffix.contains(HOLE),
            "target module has multiple frozen proof slots"
        );
        let target = FrozenTarget {
            engine,
            target_id: target.clone(),
            file_path: module.clone(),
            prefix: prefix.into(),
            suffix: suffix.into(),
        };
        // This adapter initially supports only the existing strict Lean slot.
        lean_frozen_header(&target)?;
        Some(target)
    } else {
        None
    };
    let input = match (role, frozen.as_ref()) {
        (Role::Explorer, None) => Input::Explore {
            informal_statement: &statement,
        },
        (Role::IndependentReviewer, Some(f)) => Input::ReviewHeader {
            informal_statement: &statement,
            formal_header: lean_frozen_header(f)?,
        },
        (Role::ProverWorker, Some(f)) => Input::ProposeBody {
            informal_statement: &statement,
            frozen_target: f,
        },
        _ => bail!("unsupported dispatch role"),
    };
    let base = snap.base().to_owned();
    let request = Request {
        version: 1,
        task_id,
        role,
        claim_id,
        engine,
        module_path: &module,
        target_decl: &target,
        base_commit: &base,
        max_wall_seconds: seconds,
        input,
    };
    let payload = serde_json::to_vec(&request)?;
    ensure!(
        payload.len() <= 1024 * 1024,
        "bounded local worker input exceeds 1 MiB"
    );
    ensure!(
        git.main_head()? == base,
        "Git main changed during task preparation"
    );
    Ok(Prepared {
        payload,
        base,
        module,
        target,
        engine,
        frozen,
        informal_statement: statement,
    })
}

fn safe_text(s: &str) -> bool {
    !s.trim().is_empty()
        && s.len() <= 8 * 1024
        && !s
            .chars()
            .any(|c| c.is_control() && !matches!(c, '\n' | '\t'))
}

fn decode_result(raw: &[u8]) -> Result<Suggestion> {
    ensure!(
        !raw.is_empty() && raw.len() <= RESULT_CAP,
        "worker result is empty or exceeds the 32 KiB protocol bound"
    );
    serde_json::from_slice(raw)
        .map_err(|_| anyhow::anyhow!("worker returned an invalid suggestion envelope"))
}

fn check_result(role: Role, subject: &Prepared, raw: &[u8]) -> Result<Suggestion> {
    let r = decode_result(raw)?;
    match (role, &r) {
        (Role::Explorer, Suggestion::Findings { summary }) if safe_text(summary) => (),
        (Role::IndependentReviewer, Suggestion::Review { rationale, .. })
            if safe_text(rationale) =>
        {
            ()
        }
        (Role::ProverWorker, Suggestion::ProofBody { proof_body }) => {
            ensure!(
                safe_text(proof_body),
                "proof-body suggestion is invalid or too large"
            );
            let f = subject
                .frozen
                .as_ref()
                .context("missing frozen prover input")?;
            let body = ProofBodySubmission {
                target_id: f.target_id.clone(),
                proof_body: proof_body.clone(),
            };
            render_proof_body(f, &body).map_err(|_| {
                anyhow::anyhow!("proof-body suggestion failed frozen-slot admission")
            })?;
        }
        _ => bail!("worker result is not an accepted suggestion for this role"),
    }
    Ok(r)
}

fn check_lease(lease: &TaskLease, subject: &Prepared) -> Result<()> {
    ensure!(
        lease.module_path == subject.module
            && lease.target == subject.target
            && lease.engine == subject.engine.as_str(),
        "the requested task does not match this Claim's module, engine and formal target"
    );
    Ok(())
}

pub(crate) fn run(
    root: &Path,
    repo: &Path,
    task_id: &str,
    claim_id: &str,
    role: Role,
    exe: &Path,
    sha: &str,
    seconds: u64,
) -> Result<()> {
    ensure!(
        (1..=60).contains(&seconds),
        "worker wall seconds must be in 1..=60"
    );
    ensure!(
        exe.is_absolute()
            && sha.len() == 64
            && sha
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "provide an absolute native worker path and lowercase SHA-256 pin"
    );
    let track = role_track(role)?;
    let _native = crate::managed_native(root)?;
    let mut store = StateStore::open(private_state_path(root)?)?;
    let (subject, lease) = {
        let git = PromotionRepo::lock(repo)?;
        let subject = prepare(root, &git, &store, task_id, claim_id, role, seconds)?;
        let lease = store
            .claim_expected_task(
                Some(track),
                task_id,
                Duration::from_secs(seconds + 30),
                WORKER_ACTOR,
            )?
            .context("requested task is not the next eligible task in this track")?;
        if let Err(error) = check_lease(&lease, &subject) {
            store.finish_task_run(&lease, TaskRunOutcome::Failed, WORKER_ACTOR)?;
            return Err(error);
        }
        (subject, lease)
    }; // Let a trip invalidate this lease while native work is running.
    let result = (|| {
        let raw = run_task_process(exe, sha, &subject.payload, Duration::from_secs(seconds))?;
        let serialized = serde_json::to_string(&check_result(role, &subject, &raw)?)?;
        ensure!(
            serialized.len() <= RESULT_CAP,
            "normalized worker result exceeds 32 KiB"
        );
        let git = PromotionRepo::lock(repo)?;
        let now = prepare(root, &git, &store, task_id, claim_id, role, seconds)?;
        ensure!(
            now.base == subject.base && now.payload == subject.payload,
            "approved dispatch inputs changed during worker execution"
        );
        store.finish_task_run(
            &lease,
            TaskRunOutcome::Candidate {
                base_commit: &subject.base,
                claim_id,
                role: role.as_str(),
                executable_sha256: sha,
                result_json: &serialized,
            },
            WORKER_ACTOR,
        )
    })();
    if let Err(err) = result {
        // Expiry or a trip can already have removed this exact lease. Never
        // clear a different generation when handling one failed attempt.
        let _ = store.finish_task_run(&lease, TaskRunOutcome::Failed, WORKER_ACTOR);
        return Err(err.context("worker suggestion was not accepted; no proof authority granted"));
    }
    print_json(
        json!({"task_id":task_id,"base_commit":subject.base,"role":role,"suggestion_recorded":true,"proof_verified":false}),
    )
}

pub(crate) fn result(root: &Path, id: &str) -> Result<()> {
    let store = StateStore::open_readonly(private_state_path(root)?)?;
    let row = store
        .task_candidate(id)?
        .context("no retained suggestion for this task")?;
    print_json(json!({"proof_verified":false,"candidate":row}))
}

pub(crate) fn promote(
    root: &Path,
    repo: &Path,
    id: &str,
    exe: &Path,
    data: Option<&str>,
) -> Result<()> {
    let store = StateStore::open_readonly(private_state_path(root)?)?;
    let row = store
        .task_candidate(id)?
        .context("no retained suggestion for this task")?;
    ensure!(
        row.role == Role::ProverWorker.as_str(),
        "only retained proof-body suggestions can be reviewed for promotion"
    );
    let Suggestion::ProofBody { proof_body } = decode_result(row.result_json.as_bytes())? else {
        bail!("retained suggestion is not a proof body");
    };
    crate::promote_managed_body(
        root,
        repo,
        &row.claim_id,
        &row.module_path,
        &row.target,
        exe,
        crate::ManagedProofInput::Candidate {
            body: proof_body,
            reviewed_base: &row.base_commit,
        },
        data,
    )
}

#[cfg(test)]
mod tests;
