mod breaker_probes;
mod hammer;
mod infra_rfc;
#[cfg(test)]
mod managed_tests;
mod oracle;
mod owner_approval;
mod pivot;
mod promotion;
mod reviewed_spec;
mod rocq_t2;
#[cfg(test)]
mod rocq_tests;
mod t2_batch;
mod t2_build;
mod t2_compare;
mod t2_compare_io;
mod t2_export;
mod t2_heartbeat_compare;
mod t3;
mod wiki_archive;
mod witness;
mod worker_dispatch;

use ai4math_broker::{
    AgdaMode, Broker, RegressionArtifacts, RegressionFailure, Toolchain, VerifiedCandidate,
    VerifiedLeanComparison, sandbox_self_test,
};
use ai4math_core::{Engine, FrozenTarget, ProofBodySubmission};
use ai4math_project::{ManagedProject, open_managed_state};
use ai4math_store::{
    Change, NewClaim, PromotionIntent, ReviewedLeanSpec, StateStore, VerifiedReceipt, Wiki,
};
use anyhow::{Context, Result, bail, ensure};
use promotion::{PendingRef, Promotion, PromotionRepo};
use std::{env, fs, io::Read, path::Path};
use worker_dispatch::{Suggestion, decode_result};

const HELP: &str = "ai4math init ROOT [lean4|agda|rocq]\nai4math status ROOT\nai4math claim-replay ROOT CLAIM [THROUGH_EVENT_SEQ]\nai4math plan-replay ROOT PLAN [THROUGH_EVENT_SEQ]\nai4math graph-replay ROOT [THROUGH_EVENT_SEQ]\nai4math graph-audit ROOT\nai4math graph-check ROOT\nai4math wiki-rebuild ROOT\nai4math wiki-core-rebuild ROOT BARE_REPO REVIEWED_MAIN\nai4math wiki-core-search ROOT BARE_REPO REVIEWED_MAIN TEXT\nai4math wiki-core-provenance ROOT BARE_REPO REVIEWED_MAIN ENTRY_ID\nai4math wiki-sediment-preview ROOT BARE_REPO CLAIM REVIEWED_MAIN\nai4math wiki-sediment-archive ROOT BARE_REPO CLAIM REVIEWED_MAIN APPROVAL_SHA256\nai4math wiki-sediment-wake-preview ROOT BARE_REPO ARCHIVE_ID REVIEWED_MAIN\nai4math wiki-sediment-wake-recheck ROOT BARE_REPO ARCHIVE_ID REVIEWED_MAIN NATIVE_LEAN\nai4math wiki-sediment-wake-retain ROOT BARE_REPO ARCHIVE_ID REVIEWED_MAIN NATIVE_LEAN\nai4math wiki-sediment-wake-result ROOT OBSERVATION_SHA256\nai4math wiki-sediment-adapt-preview ROOT BARE_REPO ARCHIVE_ID REVIEWED_MAIN TARGET_CLAIM\nai4math wiki-sediment-adapt-approve ROOT BARE_REPO ARCHIVE_ID REVIEWED_MAIN TARGET_CLAIM APPROVAL_SHA256\nai4math wiki-sediment-adapt-result ROOT APPROVAL_SHA256\nai4math wiki-sediment-adapt-run ROOT BARE_REPO APPROVAL_SHA256 WORKER_EXE WORKER_SHA256 WALL_SECONDS\nai4math wiki-sediment-adapt-lineage-retain ROOT BARE_REPO APPROVAL_SHA256 PROMOTED_HEAD\nai4math wiki-sediment-adapt-lineage-result ROOT APPROVAL_SHA256\nai4math self-test\nai4math hammer-reconstruct ROOT BARE_REPO CLAIM REVIEWED_COMMIT PREMISES_JSON NATIVE_LEAN\nai4math oracle-probe QUERY_JSON NATIVE_Z3 NATIVE_CVC5\nai4math oracle-preview ROOT BARE_REPO CLAIM QUERY_JSON NATIVE_Z3 NATIVE_CVC5\nai4math oracle-retain ROOT BARE_REPO CLAIM APPROVAL_SHA256 QUERY_JSON NATIVE_Z3 NATIVE_CVC5\nai4math oracle-result ROOT RECORD_SHA256\nai4math oracle-hold ROOT BARE_REPO TASK RECORD_SHA256 SECONDS\nai4math oracle-release ROOT TASK HOLD_SHA256\nai4math oracle-hold-status ROOT TASK\nai4math promote BARE_REPO MODULE TARGET NATIVE_ENGINE PROOF_BODY_FILE [ENGINE_DATA]\nai4math declare-claim ROOT CLAIM MODULE TARGET TITLE STATEMENT [--critical]\nai4math bind-lean-spec ROOT BARE_REPO CLAIM SPEC_MODULE SPEC_DECL APPROVED_COMMIT\nai4math lean-spec-comparison ROOT PROMOTED_COMMIT\nai4math lean-spec-recheck ROOT CLAIM PLAN_SHA256\nai4math bind-witness-spec ROOT BARE_REPO CLAIM MODULE PROPOSITION APPROVED_COMMIT\nai4math refute-managed ROOT BARE_REPO CLAIM MODULE PROPOSITION NATIVE_LEAN PROOF_BODY_FILE\nai4math promote-reviewed-body ROOT BARE_REPO CLAIM MODULE TARGET REVIEWED_COMMIT NATIVE_ENGINE PROOF_BODY_FILE [ENGINE_DATA]\nai4math promote-managed ROOT BARE_REPO CLAIM MODULE TARGET NATIVE_ENGINE PROOF_BODY_FILE [ENGINE_DATA]\nai4math recover-promotion ROOT BARE_REPO NATIVE_ENGINE [ENGINE_DATA]\nai4math t2-build ROOT BARE_REPO ANCHOR_MODULE REVIEWED_MAIN NATIVE_LEAN\nai4math t2-heartbeats ROOT BARE_REPO TARGET_MODULE REVIEWED_MAIN NATIVE_LEAN COMPILER_SHA256\nai4math t2-audit ROOT BARE_REPO ANCHOR_MODULE REVIEWED_MAIN NATIVE_LEAN\nai4math t2-compare BASELINE_JSON CANDIDATE_JSON MIN_DELTA_MS MIN_INCREASE_PERCENT\nai4math t2-compare-heartbeats BASELINE_JSON CANDIDATE_JSON MIN_DELTA_HEARTBEATS_RAW MIN_INCREASE_PERCENT\nai4math t2-compare-managed-heartbeats ROOT BARE_REPO TARGET_MODULE BASELINE_COMMIT REVIEWED_MAIN NATIVE_LEAN COMPILER_SHA256 MIN_DELTA_HEARTBEATS_RAW MIN_INCREASE_PERCENT\nai4math t2-compare-release-heartbeats ROOT BARE_REPO TARGET_MODULE BASELINE_LABEL BASELINE_TAG_OID REVIEWED_MAIN NATIVE_LEAN COMPILER_SHA256 MIN_DELTA_HEARTBEATS_RAW MIN_INCREASE_PERCENT\nai4math t2-trust-debt ROOT\nai4math t2-check-export EXPORT_NDJSON EXPORT_SHA256 NATIVE_NANODA CHECKER_SHA256\nai4math t2-check-managed ROOT BARE_REPO TARGET_MODULE DECLARATION REVIEWED_MAIN NATIVE_LEAN EXPORTER_SHA256 CHECKER_SHA256\nai4math t2-check-batch ROOT BARE_REPO ANCHOR_MODULE REVIEWED_MAIN NATIVE_LEAN EXPORTER_SHA256 CHECKER_SHA256 TARGETS_JSON\nai4math t2-release-check ROOT BARE_REPO ANCHOR_MODULE REVIEWED_MAIN NATIVE_LEAN POLICY_SHA256 COMPILER_SHA256 EXPORTER_SHA256 CHECKER_SHA256\nai4math t2-release-tag ROOT BARE_REPO ANCHOR_MODULE REVIEWED_MAIN NATIVE_LEAN POLICY_SHA256 COMPILER_SHA256 EXPORTER_SHA256 CHECKER_SHA256 LABEL\nai4math t2-release-inspect ROOT BARE_REPO LABEL [EXPECTED_TAG_OID]\nai4math t3-discover ROOT BARE_REPO ANCHOR_MODULE BASELINE_COMMIT\nai4math t3-discover-claim ROOT BARE_REPO CLAIM_ID\nai4math t3-preview ROOT BARE_REPO ANCHOR_MODULE OLD_COMMIT REVIEW_COMMIT\nai4math t3-apply ROOT BARE_REPO ANCHOR_MODULE OLD_COMMIT REVIEW_COMMIT PLAN_SHA256\nai4math t3-recheck ROOT BARE_REPO PLAN_SHA256 NATIVE_ENGINE [ENGINE_DATA]\nai4math t3-worklist ROOT BARE_REPO PLAN_SHA256 LIMIT [AFTER_CLAIM]\nai4math t3-recheck-one ROOT BARE_REPO PLAN_SHA256 CLAIM NATIVE_ENGINE [ENGINE_DATA]\nai4math t3-recheck-batch ROOT BARE_REPO PLAN_SHA256 LIMIT NATIVE_ENGINE [ENGINE_DATA] [--after CLAIM EVENT_SEQ]\nai4math t3-sweep-status ROOT BARE_REPO PLAN_SHA256\nai4math t3-sweep-step ROOT BARE_REPO PLAN_SHA256 LIMIT NATIVE_ENGINE [ENGINE_DATA] [--restart]\nai4math trip-breaker ROOT BARE_REPO BREAKER MODULE DEFINITION REASON [--critical]
ai4math linux-cgroup-preflight ABSOLUTE_LEAF
ai4math rocq-t2-check ROOT BARE_REPO MODULE THEOREM REVIEWED_MAIN NATIVE_ROCQ ROCQ_DATA COMPILER_SHA256 COQCHK CHECKER_SHA256
ai4math pivot-preview ROOT BARE_REPO FACT_SHA256 BREAKER REASON
ai4math pivot-apply ROOT BARE_REPO FACT_SHA256 BREAKER REASON APPROVED_ADR_ID
ai4math pivot-archive-preview ROOT BARE_REPO FACT_SHA256 BREAKER REASON
ai4math owner-archive-preview ROOT BARE_REPO FACT_SHA256 BREAKER REASON
ai4math owner-archive-signing-message ROOT BARE_REPO BASE ADR_SHA256 REQUEST_ID EXPIRES_UNIX
ai4math owner-archive-verify ROOT BARE_REPO BASE ADR_SHA256 REQUEST_ID EXPIRES_UNIX TRUSTED_PUBLIC_KEY_FILE SIGNATURE_FILE
ai4math owner-archive-apply ROOT BARE_REPO FACT_SHA256 BREAKER REASON BASE ADR_SHA256 REQUEST_ID EXPIRES_UNIX SIGNATURE_FILE
ai4math owner-archive-status ROOT REQUEST_ID
ai4math owner-archive-recover ROOT BARE_REPO REQUEST_ID
ai4math pivot-archive-apply ROOT BARE_REPO FACT_SHA256 BREAKER REASON APPROVED_ADR_ID
ai4math pivot-routes-preview ROOT BARE_REPO FACT_SHA256 BREAKER REASON PLAN...
ai4math pivot-routes-apply ROOT BARE_REPO FACT_SHA256 BREAKER REASON APPROVED_ADR_ID PLAN...
ai4math pivot-lesson-preview ROOT BARE_REPO FACT_SHA256 BREAKER REASON TITLE LESSON PLAN...
ai4math pivot-lesson-apply ROOT BARE_REPO FACT_SHA256 BREAKER REASON APPROVED_ADR_ID TITLE LESSON PLAN...
ai4math pivot-recover ROOT BARE_REPO
ai4math pivot-abandon ROOT BARE_REPO APPROVED_ADR_ID
ai4math breaker-probes-preview ROOT BARE_REPO BREAKER THEOREM [THEOREM]
ai4math breaker-probes-arm ROOT BARE_REPO BREAKER APPROVAL_ID THEOREM [THEOREM]
ai4math breaker-probes-run ROOT BARE_REPO BREAKER APPROVAL_ID NATIVE_LEAN
ai4math task-register ROOT BARE_REPO ID TRACK MODULE TARGET [DEP_ID ...]
ai4math tasks-status ROOT
ai4math infra-rfc-submit ROOT BARE_REPO TARGET_MODULE REVIEWED_MAIN DRAFT_JSON
ai4math infra-rfc-approve ROOT BARE_REPO RFC_ID REVIEWED_MAIN APPROVED_SHA256
ai4math infra-rfc-smoke-start ROOT BARE_REPO RFC_ID REVIEWED_MAIN DRAFT_SHA256
ai4math infra-rfc-freeze ROOT BARE_REPO RFC_ID REVIEWED_MAIN DRAFT_SHA256 NATIVE_LEAN SMOKE_LEMMA...
ai4math infra-rfc-complete ROOT BARE_REPO RFC_ID REVIEWED_MAIN DRAFT_SHA256 NATIVE_LEAN
ai4math infra-rfc-reject ROOT BARE_REPO RFC_ID REVIEWED_MAIN DRAFT_SHA256 REASON
ai4math infra-rfc-supersede ROOT BARE_REPO RFC_ID REVIEWED_MAIN DRAFT_SHA256 REPLACEMENT_RFC_ID REASON
ai4math infra-rfc-inspect ROOT BARE_REPO RFC_ID
ai4math task-bind ROOT BARE_REPO ANCHOR_MODULE REVIEWED_COMMIT
ai4math task-run ROOT BARE_REPO ID CLAIM ROLE WORKER_EXE SHA256 WALL_SECONDS
ai4math task-run-model ROOT BARE_REPO ID CLAIM ROLE WALL_SECONDS
ai4math model-dispatch-approve ROOT BARE_REPO ID CLAIM ROLE REVIEWED_COMMIT MODEL WALL_SECONDS MAX_MODEL_CALLS
ai4math model-dispatch-status ROOT ID
ai4math model-dispatch-cancel ROOT ID
ai4math model-dispatch-cancel-running ROOT ID
ai4math model-dispatch-reconcile ROOT ID
ai4math model-dispatch-resident-scan ROOT BARE_REPO MODEL LIMIT
ai4math task-run-approved-model ROOT BARE_REPO ID CLAIM ROLE WALL_SECONDS MODEL MAX_MODEL_CALLS
ai4math task-result ROOT ID
ai4math refute-managed ROOT BARE_REPO ID MODULE PROPOSITION NATIVE_LEAN PROOF_BODY
ai4math refute-pending ROOT
ai4math refute-reconcile ROOT [CLAIM]
ai4math formal-sketch-task ROOT BARE_REPO ID NATIVE_LEAN PROBE_OBSERVATION_ID
ai4math oracle-backed-task ROOT BARE_REPO ID NATIVE_LEAN PROBE_OBSERVATION_ID
ai4math promote-task-result ROOT BARE_REPO ID NATIVE_ENGINE [ENGINE_DATA]
ai4math cache-clone BARE_REPO COMMIT NEW_DIR";
const HOLE: &str = "{{PROOF_BODY}}";

fn print_json(value: impl serde::Serialize) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

fn replay_cursor(cursor: Option<&str>) -> Result<Option<i64>> {
    cursor
        .map(|text| -> Result<i64> {
            ensure!(
                text == "0" || (!text.starts_with('0') && text.bytes().all(|b| b.is_ascii_digit())),
                "event cursor must be a canonical nonnegative integer"
            );
            text.parse()
                .context("event cursor must be a canonical nonnegative integer")
        })
        .transpose()
}

fn replay_store(root: &Path) -> Result<StateStore> {
    open_managed_state(root, true)
        .context("replay requires an initialized, owner-controlled project")
}

fn replay_audit(command: &str, root: &Path, id: &str, cursor: Option<&str>) -> Result<()> {
    let through = replay_cursor(cursor)?;
    let state = replay_store(root)?;
    match command {
        "claim-replay" => print_json(state.replay_claim(id, through)?),
        "plan-replay" => print_json(state.replay_plan(id, through)?),
        _ => unreachable!("replay command is constrained by the CLI grammar"),
    }
}

/// Always acquire before the coordinator Git lock and any state mutation.
/// The guard is kept across the subprocess, postchecks, and associated commit.
fn managed_native(root: &Path) -> Result<ai4math_broker::ProjectNativeLease> {
    ManagedProject::new(root)?.try_native_heavy()
}

fn init(root: &Path, engine: Engine) -> Result<()> {
    use std::io::Write;
    let runtime = root.join(".runtime");
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::create_dir_all(root)?;
        match fs::DirBuilder::new().mode(0o700).create(&runtime) {
            Ok(()) => (),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => (),
            Err(error) => return Err(error.into()),
        }
    }
    #[cfg(not(unix))]
    fs::create_dir_all(&runtime)?;
    ensure!(
        fs::symlink_metadata(&runtime)?.is_dir(),
        "project .runtime must not be a symlink"
    );
    let engine_path = runtime.join("primary_engine");
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&engine_path)
    {
        Ok(mut file) => writeln!(file, "{engine}")?,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            ensure!(
                fs::read_to_string(engine_path)?.trim() == engine.as_str(),
                "primary engine is already configured differently"
            );
        }
        Err(e) => return Err(e.into()),
    }
    for stratum in ["core", "sediment", "graveyard"] {
        fs::create_dir_all(root.join(".mathwiki").join(stratum))?;
    }
    StateStore::open(runtime.join("state.sqlite"))?;
    Wiki::open(root.join(".mathwiki/.index.sqlite"))?;
    Ok(())
}

fn engine_for(module: &str) -> Result<Engine> {
    if module.ends_with(".lean") {
        Ok(Engine::Lean4)
    } else if module.ends_with(".agda") {
        Ok(Engine::Agda)
    } else if module.ends_with(".v") {
        Ok(Engine::Rocq)
    } else {
        bail!("unsupported source module extension")
    }
}
fn bound_engine(root: &Path, module: &str) -> Result<Engine> {
    let engine = engine_for(module)?;
    ensure!(
        fs::read_to_string(root.join(".runtime/primary_engine"))?.trim() == engine.as_str(),
        "module does not use project primary engine"
    );
    Ok(engine)
}
fn read_proof(body: &Path) -> Result<String> {
    let body = fs::File::open(body)?;
    let mut text = String::new();
    body.take(256 * 1024 + 1).read_to_string(&mut text)?;
    ensure!(text.len() <= 256 * 1024, "proof body too large");
    Ok(text)
}
fn trusted_toolchain(
    engine: Engine,
    executable: &Path,
    engine_data: Option<&str>,
) -> Result<Toolchain> {
    let mut toolchain = match (engine, engine_data) {
        (Engine::Lean4, None) => Toolchain::lean(executable)?,
        (Engine::Agda, None) => Toolchain::agda(executable, AgdaMode::WithoutK)?,
        (Engine::Agda, Some(data)) => {
            Toolchain::agda(executable, AgdaMode::WithoutK)?.with_agda_data(data)?
        }
        (Engine::Rocq, Some(data)) => Toolchain::rocq(executable, data)?,
        (Engine::Rocq, None) => bail!("Rocq requires an explicit absolute ROCQ_DATA directory"),
        (Engine::Lean4, Some(_)) => bail!("Lean does not accept an engine data directory"),
    };
    // Rocq installations get their own narrow grants, never those intended for Agda.
    let sandbox_reads = if engine == Engine::Rocq {
        "AI4MATH_ROCQ_RUNTIME_READS"
    } else {
        "AI4MATH_RUNTIME_READS"
    };
    if let Some(runtime_reads) = env::var_os(sandbox_reads) {
        for path in env::split_paths(&runtime_reads) {
            toolchain = toolchain.with_runtime_read(path)?;
        }
    }
    Ok(toolchain)
}

fn verify_and_regress(
    tx: &Promotion<'_>,
    engine: Engine,
    module: &str,
    theorem: &str,
    executable: &Path,
    proof_body: &str,
    agda_data: Option<&str>,
) -> Result<(VerifiedCandidate, RegressionArtifacts)> {
    let (candidate, artifacts, _) = verify_promotion(
        tx, engine, module, theorem, executable, proof_body, agda_data, None,
    )?;
    Ok((candidate, artifacts))
}

fn verify_promotion(
    tx: &Promotion<'_>,
    engine: Engine,
    module: &str,
    theorem: &str,
    executable: &Path,
    proof_body: &str,
    engine_data: Option<&str>,
    spec: Option<&ReviewedLeanSpec>,
) -> Result<(
    VerifiedCandidate,
    RegressionArtifacts,
    Option<VerifiedLeanComparison>,
)> {
    let project = tx.project()?;
    let target_idx = project
        .iter()
        .position(|(path, _)| *path == module)
        .context("target module missing")?;
    let source = &project[target_idx].1;
    let (prefix, suffix) = source
        .split_once(HOLE)
        .context("base module must contain exactly one {{PROOF_BODY}} slot")?;
    ensure!(
        !suffix.contains(HOLE),
        "base module has more than one proof slot"
    );
    let target = FrozenTarget {
        engine,
        target_id: theorem.to_owned(),
        file_path: module.to_owned(),
        prefix: prefix.to_owned(),
        suffix: suffix.to_owned(),
    };
    let submission = ProofBodySubmission {
        target_id: theorem.to_owned(),
        proof_body: proof_body.to_owned(),
    };
    let deps: Vec<_> = project[..target_idx]
        .iter()
        .map(|(path, source)| (*path, source.as_str()))
        .collect();
    if let Some(spec) = spec {
        ensure!(
            engine == Engine::Lean4,
            "reviewed specification requires a Lean candidate"
        );
        reviewed_spec::check_prefix(&deps, spec)?;
    }
    let toolchain = trusted_toolchain(engine, executable, engine_data)?;
    let broker = Broker::new();
    let result =
        broker.verify_with_dependencies(&toolchain, &target, &submission, tx.base(), &deps)?;
    let comparison = spec
        .map(|spec| {
            broker.compare_lean_spec(&toolchain, &result, &spec.spec_file, &spec.spec_decl, &deps)
        })
        .transpose()
        .context("reviewed Lean specification comparator failed")?;
    // Conservative regression: compile the entire bounded project, so no affected module is skipped.
    let regression: Vec<_> = project
        .iter()
        .map(|(path, source)| {
            (
                *path,
                if *path == module {
                    result.source()
                } else {
                    source.as_str()
                },
            )
        })
        .collect();
    let artifacts = broker
        .regress(&toolchain, &result, &regression)
        .context("tracked-project regression failed")?;
    ensure!(
        artifacts.matches(&result),
        "regression result does not bind the verified candidate"
    );
    Ok((result, artifacts, comparison))
}
fn report(
    tx: &Promotion<'_>,
    commit: &str,
    result: &VerifiedCandidate,
    artifacts: RegressionArtifacts,
    event_seq: Option<i64>,
) -> Result<()> {
    let golden_published = match artifacts.publish(&tx.repo().join("ai4math-golden"), commit) {
        Ok(()) => true,
        Err(error) => {
            eprintln!("ai4math: main advanced; cache publish failed: {error:#}");
            false
        }
    };
    print_json(
        serde_json::json!({"commit":commit,"receipt":result.receipt(),"golden_published":golden_published,"ledger_event_seq":event_seq}),
    )
}
fn promote(
    repo: &Path,
    module: &str,
    theorem: &str,
    executable: &Path,
    body: &Path,
    data: Option<&str>,
) -> Result<()> {
    let locked = PromotionRepo::lock(repo)?;
    let tx = locked.snapshot(module, None)?;
    let (result, artifacts) = verify_and_regress(
        &tx,
        engine_for(module)?,
        module,
        theorem,
        executable,
        &read_proof(body)?,
        data,
    )?;
    let commit = tx.commit(result.source().as_bytes(), result.receipt())?;
    report(&tx, &commit, &result, artifacts, None)
}
fn declare_claim(
    root: &Path,
    id: &str,
    module: &str,
    target: &str,
    title: &str,
    informal_statement: &str,
) -> Result<()> {
    enroll_claim(root, id, module, target, title, informal_statement, false)
}

fn enroll_claim(
    root: &Path,
    id: &str,
    module: &str,
    target: &str,
    title: &str,
    informal_statement: &str,
    critical: bool,
) -> Result<()> {
    let engine = bound_engine(root, module)?;
    let formal_target = format!("{module}#{target}");
    let mut store = open_managed_state(root, false)?;
    store.create_claim(
        NewClaim {
            id,
            title,
            informal_statement,
            engine: engine.as_str(),
            formal_target: Some(&formal_target),
            module_path: Some(module),
            critical,
        },
        Change {
            actor: "human",
            git_commit: None,
        },
    )
}
fn promote_managed(
    root: &Path,
    repo: &Path,
    claim: &str,
    module: &str,
    theorem: &str,
    exec: &Path,
    body: &Path,
    data: Option<&str>,
) -> Result<()> {
    promote_managed_body(
        root,
        repo,
        claim,
        module,
        theorem,
        exec,
        ManagedProofInput::File(body),
        data,
    )
}

enum ManagedProofInput<'a> {
    File(&'a Path),
    ReviewedFile {
        file: &'a Path,
        reviewed_base: &'a str,
    },
    Candidate {
        body: String,
        reviewed_base: &'a str,
    },
}

fn formal_sketch_task(
    root: &Path,
    repo: &Path,
    task_id: &str,
    executable: &Path,
    probe_observation_id: &str,
) -> Result<()> {
    let project = ManagedProject::new(root)?;
    let _native = project.try_native_heavy()?;
    let locked = PromotionRepo::try_lock_existing(repo)?;
    let mut store = project.open_state()?;
    let row = store
        .task_candidate(task_id)?
        .context("no retained task candidate")?;
    ensure!(
        row.role == "prover_worker",
        "formal sketch requires a prover candidate"
    );
    let Suggestion::ProofBody { proof_body } = decode_result(row.result_json.as_bytes())? else {
        bail!("formal sketch candidate is not a proof body");
    };
    ensure!(
        locked.main_head()? == row.base_commit,
        "formal sketch candidate belongs to an older Git snapshot"
    );
    ensure!(
        store.managed_repo()?.as_deref() == locked.path().to_str(),
        "formal sketch candidate is bound to a different repository"
    );
    ensure!(
        !store.module_breaker_gated("lean4", &row.module_path)?,
        "breaker still gates the formal sketch module"
    );
    ensure!(
        store.pivot_intent()?.is_none() && store.promotion_intent()?.is_none(),
        "recover pending Git transaction before formal sketch retention"
    );
    let claim = store
        .claim(&row.claim_id)?
        .context("formal sketch claim not found")?;
    ensure!(
        claim.engine == "lean4"
            && claim.formal_target.as_deref()
                == Some(format!("{}#{}", row.module_path, row.target).as_str()),
        "formal sketch claim target mismatch"
    );
    let spec = store
        .lean_spec(&row.claim_id)?
        .context("formal sketch requires a reviewed Lean spec")?;
    let tx = locked.snapshot(&row.module_path, Some(&row.base_commit))?;
    reviewed_spec::check_snapshot(&locked, &tx, &spec)?;
    let files = tx.project()?;
    let index = files
        .iter()
        .position(|(path, _)| *path == row.module_path)
        .context("formal sketch module missing")?;
    let source = &files[index].1;
    let (prefix, suffix) = source
        .split_once(HOLE)
        .context("formal sketch module must contain one proof slot")?;
    ensure!(
        !suffix.contains(HOLE),
        "formal sketch module has multiple proof slots"
    );
    let target = FrozenTarget {
        engine: Engine::Lean4,
        target_id: row.target.clone(),
        file_path: row.module_path.clone(),
        prefix: prefix.to_owned(),
        suffix: suffix.to_owned(),
    };
    let submission = ProofBodySubmission {
        target_id: row.target.clone(),
        proof_body,
    };
    let deps: Vec<(&str, &str)> = files[..index]
        .iter()
        .map(|(p, s)| (*p, s.as_str()))
        .collect();
    reviewed_spec::check_prefix(&deps, &spec)?;
    let toolchain = trusted_toolchain(Engine::Lean4, executable, None)?;
    let broker = Broker::new();
    let candidate = broker
        .assess_formal_sketch_with_dependencies(
            &toolchain,
            &target,
            &submission,
            &row.base_commit,
            &deps,
        )
        .context("formal sketch did not pass assessed T3 compilation")?;
    ensure!(
        locked.main_head()? == row.base_commit,
        "main changed during formal sketch verification"
    );
    let comparison = broker
        .compare_formal_sketch_spec(
            &toolchain,
            &candidate,
            &spec.spec_file,
            &spec.spec_decl,
            &deps,
        )
        .context("formal sketch spec comparison failed")?;
    ensure!(
        locked.main_head()? == row.base_commit,
        "main changed during formal sketch comparison"
    );
    let claim = store.apply_formal_sketch_evidence(
        &row.claim_id,
        &row.base_commit,
        &candidate,
        &comparison,
        probe_observation_id,
        Change {
            actor: "broker:formal-sketch",
            git_commit: Some(&row.base_commit),
        },
    )?;
    print_json(
        serde_json::json!({"claim":claim,"proof_verified":false,"candidate_receipt_sha256":candidate.receipt().receipt_sha256(),"comparison_receipt_sha256":comparison.receipt_sha256()}),
    )
}

fn oracle_backed_task(
    root: &Path,
    repo: &Path,
    task_id: &str,
    executable: &Path,
    probe_observation_id: &str,
) -> Result<()> {
    let project = ManagedProject::new(root)?;
    let _native = project.try_native_heavy()?;
    let locked = PromotionRepo::try_lock_existing(repo)?;
    let mut store = project.open_state()?;
    let row = store
        .task_candidate(task_id)?
        .context("no retained task candidate")?;
    ensure!(
        row.role == "prover_worker",
        "Oracle-backed trust requires a prover candidate"
    );
    let Suggestion::ProofBody { proof_body } = decode_result(row.result_json.as_bytes())? else {
        bail!("Oracle-backed candidate is not a proof body");
    };
    ensure!(
        locked.main_head()? == row.base_commit,
        "Oracle-backed candidate belongs to an older Git snapshot"
    );
    ensure!(
        store.managed_repo()?.as_deref() == locked.path().to_str(),
        "Oracle-backed candidate is bound to a different repository"
    );
    ensure!(
        !store.module_breaker_gated("lean4", &row.module_path)?,
        "breaker still gates the Oracle-backed module"
    );
    ensure!(
        store.pivot_intent()?.is_none() && store.promotion_intent()?.is_none(),
        "recover pending Git transaction before Oracle-backed retention"
    );
    let claim = store
        .claim(&row.claim_id)?
        .context("Oracle-backed claim not found")?;
    ensure!(
        claim.engine == "lean4"
            && claim.formal_target.as_deref()
                == Some(format!("{}#{}", row.module_path, row.target).as_str()),
        "Oracle-backed claim target mismatch"
    );
    let spec = store
        .lean_spec(&row.claim_id)?
        .context("Oracle-backed trust requires a reviewed Lean spec")?;
    let tx = locked.snapshot(&row.module_path, Some(&row.base_commit))?;
    reviewed_spec::check_snapshot(&locked, &tx, &spec)?;
    let files = tx.project()?;
    let index = files
        .iter()
        .position(|(path, _)| *path == row.module_path)
        .context("Oracle-backed module missing")?;
    let source = &files[index].1;
    let (prefix, suffix) = source
        .split_once(HOLE)
        .context("Oracle-backed module must contain one proof slot")?;
    ensure!(
        !suffix.contains(HOLE),
        "Oracle-backed module has multiple proof slots"
    );
    let target = FrozenTarget {
        engine: Engine::Lean4,
        target_id: row.target.clone(),
        file_path: row.module_path.clone(),
        prefix: prefix.to_owned(),
        suffix: suffix.to_owned(),
    };
    let submission = ProofBodySubmission {
        target_id: row.target.clone(),
        proof_body,
    };
    let deps: Vec<(&str, &str)> = files[..index]
        .iter()
        .map(|(p, s)| (*p, s.as_str()))
        .collect();
    reviewed_spec::check_prefix(&deps, &spec)?;
    let toolchain = trusted_toolchain(Engine::Lean4, executable, None)?;
    let broker = Broker::new();
    let candidate = broker
        .assess_oracle_backed_with_dependencies(
            &toolchain,
            &target,
            &submission,
            &row.base_commit,
            &deps,
        )
        .context("Oracle-backed candidate did not pass compiler assessment")?;
    ensure!(
        locked.main_head()? == row.base_commit,
        "main changed during Oracle-backed verification"
    );
    let comparison = broker
        .compare_oracle_backed_spec(
            &toolchain,
            &candidate,
            &spec.spec_file,
            &spec.spec_decl,
            &deps,
        )
        .context("Oracle-backed spec comparison failed")?;
    ensure!(
        locked.main_head()? == row.base_commit,
        "main changed during Oracle-backed comparison"
    );
    let claim = store.apply_oracle_backed_evidence(
        &row.claim_id,
        &row.base_commit,
        &candidate,
        &comparison,
        probe_observation_id,
        Change {
            actor: "broker:oracle-backed",
            git_commit: Some(&row.base_commit),
        },
    )?;
    print_json(serde_json::json!({
        "claim":claim,"proof_verified":false,"oracle_debt":true,
        "candidate_receipt_sha256":candidate.receipt().receipt_sha256(),
        "comparison_receipt_sha256":comparison.receipt_sha256(),
    }))
}

fn promote_managed_body(
    root: &Path,
    repo: &Path,
    claim: &str,
    module: &str,
    theorem: &str,
    exec: &Path,
    input: ManagedProofInput<'_>,
    data: Option<&str>,
) -> Result<()> {
    let project = ManagedProject::new(root)?;
    let _native = project.try_native_heavy()?;
    let reviewed_base = match &input {
        ManagedProofInput::Candidate { reviewed_base, .. }
        | ManagedProofInput::ReviewedFile { reviewed_base, .. } => Some(*reviewed_base),
        ManagedProofInput::File(_) => None,
    };
    let engine = bound_engine(root, module)?;
    let locked = PromotionRepo::lock(repo)?;
    let state_path = project.state_path().to_path_buf();
    let mut store = project.open_state()?;
    if let Some(base) = reviewed_base {
        ensure!(
            locked.main_head()? == base,
            "reviewed candidate belongs to an older Git snapshot"
        );
        ensure!(
            store.managed_repo()?.as_deref() == locked.path().to_str(),
            "candidate is bound to a different managed repository"
        );
    }
    ensure!(
        !store.module_breaker_gated(engine.as_str(), module)?,
        "breaker still gates this candidate's module"
    );
    ensure!(
        store.pivot_intent()?.is_none(),
        "recover pending Pivot before candidate promotion"
    );
    ensure!(
        store.claim(claim)?.is_some(),
        "unknown claim; declare it first"
    );
    ensure!(
        store.promotion_intent()?.is_none(),
        "project has a pending promotion; recover it first"
    );
    let pin = locked.pending_ref(&state_path)?;
    pin.clear()?;
    let tx = locked.snapshot(module, None)?;
    ensure!(
        reviewed_base.is_none_or(|base| tx.base() == base),
        "retained candidate snapshot is no longer current"
    );
    let body = match input {
        ManagedProofInput::Candidate { body, .. } => body,
        ManagedProofInput::File(file) | ManagedProofInput::ReviewedFile { file, .. } => {
            read_proof(file)?
        }
    };
    let spec = store.lean_spec(claim)?;
    if let Some(spec) = spec.as_ref() {
        reviewed_spec::check_snapshot(&locked, &tx, spec)?;
    }
    let (result, artifacts, comparison) = match verify_promotion(
        &tx,
        engine,
        module,
        theorem,
        exec,
        &body,
        data,
        spec.as_ref(),
    ) {
        Ok(verified) => verified,
        Err(error) => {
            if let Some(failure) = error.downcast_ref::<RegressionFailure>() {
                // Preserve the original failure for the operator. This side
                // effect is advisory and cannot prepare or publish a commit.
                let note = (|| {
                    if locked.main_head()? != failure.base_commit() {
                        return Ok(None);
                    }
                    let repo = locked
                        .path()
                        .to_str()
                        .context("repository path is not UTF-8")?;
                    if store.managed_repo()?.as_deref() != Some(repo) {
                        return Ok(None);
                    }
                    store.record_managed_regression_failure(repo, claim, failure)
                })();
                match note {
                    Ok(Some(seq)) => {
                        eprintln!("ai4math: recorded regression meltdown P0 event #{seq}")
                    }
                    Err(ledger_error) => {
                        eprintln!("ai4math: regression failure not retained: {ledger_error:#}")
                    }
                    Ok(None) => (),
                }
            }
            return Err(error);
        }
    };
    let commit = tx.prepare_commit(result.source().as_bytes(), result.receipt())?;
    let intent = PromotionIntent {
        repo: tx
            .repo()
            .to_str()
            .context("repository path is not UTF-8")?
            .into(),
        claim_id: claim.into(),
        base_commit: tx.base().into(),
        new_commit: commit.clone(),
        file_path: module.into(),
        target_decl: theorem.into(),
        proof_body: body,
    };
    let receipt = match comparison.as_ref() {
        Some(compared) => VerifiedReceipt::from_compared_promotion(&result, &artifacts, compared)?,
        None => VerifiedReceipt::from_promotion(&result, &artifacts)?,
    };
    journal_promotion(&mut store, &pin, &intent, &receipt)?;
    tx.advance(&commit)
        .context("promotion remains pending; recover against current main")?;
    let seq = finish_promotion(&tx, &mut store, &pin, &intent, &receipt)?;
    report(&tx, &commit, &result, artifacts, Some(seq))
}

fn remove_pending_pin(pin: &PendingRef<'_>, commit: &str) {
    if let Err(error) = pin.remove(commit) {
        eprintln!("ai4math: run recover-promotion to clean the pending ref: {error:#}");
    }
}

fn journal_promotion(
    store: &mut StateStore,
    pin: &PendingRef<'_>,
    intent: &PromotionIntent,
    receipt: &VerifiedReceipt,
) -> Result<()> {
    // The ref must be durable before SQLite can record the intent. An unused ref is
    // recoverable; a journal entry pointing at a garbage-collected commit is not.
    pin.ensure_pinned(&intent.new_commit)?;
    if let Err(error) = store.prepare_promotion(intent, receipt) {
        // Only release the ref if a successful read establishes that there is no
        // matching durable journal entry (for example, a rejected claim receipt).
        if let Ok(current) = store.promotion_intent()
            && current.as_ref() != Some(intent)
        {
            remove_pending_pin(pin, &intent.new_commit);
        }
        return Err(error);
    }
    Ok(())
}

fn finish_promotion(
    tx: &Promotion<'_>,
    store: &mut StateStore,
    pin: &PendingRef<'_>,
    intent: &PromotionIntent,
    receipt: &VerifiedReceipt,
) -> Result<i64> {
    ensure!(
        tx.currently_published(&intent.new_commit)?,
        "prepared commit no longer on main; leaving journal unchanged"
    );
    let seq = store
        .finish_promotion(intent, receipt)
        .context("Git updated; run recover-promotion to reconcile the ledger")?;
    remove_pending_pin(pin, &intent.new_commit);
    Ok(seq)
}

fn recover_promotion(root: &Path, repo: &Path, exec: &Path, data: Option<&str>) -> Result<()> {
    let project = ManagedProject::new(root)?;
    let _native = project.try_native_heavy()?;
    let locked = PromotionRepo::lock(repo)?;
    let state_path = project.state_path().to_path_buf();
    let mut store = project.open_state()?;
    let pin = locked.pending_ref(&state_path)?;
    let Some(intent) = store.promotion_intent()? else {
        pin.clear()?;
        return print_json(serde_json::json!({"pending":false}));
    };
    ensure!(
        locked.path() == Path::new(&intent.repo),
        "pending promotion belongs to a different repository"
    );
    let tx = locked.snapshot(&intent.file_path, Some(&intent.base_commit))?;
    let engine = bound_engine(root, &intent.file_path)?;
    // Retry old journals as long as their prepared object still exists.
    pin.ensure_pinned(&intent.new_commit)?;
    let needs_advance = tx.main_at_lock() == intent.base_commit;
    ensure!(
        needs_advance || tx.published(&intent.new_commit)?,
        "main diverged from the prepared promotion; expected base {} or a descendant of {}, found {}; leaving journal unchanged",
        intent.base_commit,
        intent.new_commit,
        tx.main_at_lock(),
    );
    let spec = store.lean_spec(&intent.claim_id)?;
    if let Some(spec) = spec.as_ref() {
        reviewed_spec::check_snapshot(&locked, &tx, spec)?;
    }
    let (result, artifacts, comparison) = verify_promotion(
        &tx,
        engine,
        &intent.file_path,
        &intent.target_decl,
        exec,
        &intent.proof_body,
        data,
        spec.as_ref(),
    )?;
    tx.assert_prepared_commit(&intent.new_commit, result.source().as_bytes())?;
    let receipt = match comparison.as_ref() {
        Some(compared) => VerifiedReceipt::from_compared_promotion(&result, &artifacts, compared)?,
        None => VerifiedReceipt::from_promotion(&result, &artifacts)?,
    };
    if needs_advance {
        tx.advance(&intent.new_commit)?;
    }
    let seq = finish_promotion(&tx, &mut store, &pin, &intent, &receipt)?;
    report(&tx, &intent.new_commit, &result, artifacts, Some(seq))
}
fn run() -> Result<()> {
    let args: Vec<_> = env::args().skip(1).collect();
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        [] | ["--help" | "-h" | "help"] => {
            println!("{HELP}");
            Ok(())
        }
        ["init", root] => init(Path::new(root), Engine::Lean4),
        ["init", root, engine] => init(
            Path::new(root),
            engine.parse().map_err(|e| anyhow::anyhow!("{e}"))?,
        ),
        ["status", root] => {
            let root = Path::new(root);
            let primary_engine = fs::read_to_string(root.join(".runtime/primary_engine"))
                .context("initialize the project first")?;
            let state = open_managed_state(root, true)?.status()?;
            print_json(serde_json::json!({"primary_engine": primary_engine.trim(), "state": state}))
        }
        [command @ ("claim-replay" | "plan-replay"), root, id] => {
            replay_audit(command, Path::new(root), id, None)
        }
        [
            command @ ("claim-replay" | "plan-replay"),
            root,
            id,
            through,
        ] => replay_audit(command, Path::new(root), id, Some(through)),
        ["t2-trust-debt", root] => print_json(replay_store(Path::new(root))?.scan_trust_debt()?),
        ["graph-audit", root] => {
            let report = replay_store(Path::new(root))?.audit_graph()?;
            let consistent = report.consistent;
            print_json(report)?;
            ensure!(
                consistent,
                "materialized graph differs from its ledger projection"
            );
            Ok(())
        }
        ["graph-check", root] => {
            let report = replay_store(Path::new(root))?.check_graph()?;
            let consistent = report.consistent;
            print_json(report)?;
            ensure!(
                consistent,
                "ledger graph has inconsistent derived AND-OR state"
            );
            Ok(())
        }
        ["graph-replay", root] => print_json(replay_store(Path::new(root))?.replay_graph(None)?),
        ["graph-replay", root, through] => {
            let through = replay_cursor(Some(through))?;
            print_json(replay_store(Path::new(root))?.replay_graph(through)?)
        }
        ["wiki-rebuild", root] => {
            let root = Path::new(root);
            let wiki = wiki_archive::core_projection::wiki_root(root)
                .context("initialize the project first and verify its Wiki paths")?;
            let prepared = Wiki::prepare_rebuild(&wiki)?;
            let wiki = wiki_archive::core_projection::wiki_root(root)?;
            let count = Wiki::open(wiki.join(".index.sqlite"))?.publish_rebuild(prepared)?;
            print_json(serde_json::json!({"indexed_entries": count}))
        }
        ["wiki-core-rebuild", root, repo, head] => {
            wiki_archive::core_projection::rebuild_command(Path::new(root), Path::new(repo), head)
        }
        ["wiki-core-search", root, repo, head, text] => {
            wiki_archive::core_projection::search(Path::new(root), Path::new(repo), head, text)
        }
        ["wiki-core-provenance", root, repo, head, id] => {
            wiki_archive::core_projection::provenance(Path::new(root), Path::new(repo), head, id)
        }
        ["wiki-sediment-preview", root, repo, claim, head] => {
            wiki_archive::preview(Path::new(root), Path::new(repo), claim, head)
        }
        ["wiki-sediment-wake-preview", root, repo, id, head] => {
            wiki_archive::wake_preview(Path::new(root), Path::new(repo), id, head)
        }
        ["wiki-sediment-wake-recheck", root, repo, id, head, lean] => {
            wiki_archive::recheck::command(
                Path::new(root),
                Path::new(repo),
                id,
                head,
                Path::new(lean),
            )
        }
        ["wiki-sediment-wake-retain", root, repo, id, head, lean] => wiki_archive::recheck::retain(
            Path::new(root),
            Path::new(repo),
            id,
            head,
            Path::new(lean),
        ),
        ["wiki-sediment-wake-result", root, id] => {
            wiki_archive::recheck::result(Path::new(root), id)
        }
        ["wiki-sediment-adapt-preview", root, repo, id, head, claim] => {
            wiki_archive::adapt::preview(Path::new(root), Path::new(repo), id, head, claim)
        }
        [
            "wiki-sediment-adapt-approve",
            root,
            repo,
            id,
            head,
            claim,
            approval,
        ] => wiki_archive::adapt::approve(
            Path::new(root),
            Path::new(repo),
            id,
            head,
            claim,
            approval,
        ),
        [
            "wiki-sediment-adapt-run",
            root,
            repo,
            approval,
            worker,
            sha,
            seconds,
        ] => worker_dispatch::run_adaptation(
            Path::new(root),
            Path::new(repo),
            approval,
            Path::new(worker),
            sha,
            seconds.parse()?,
        ),
        [
            "wiki-sediment-adapt-lineage-retain",
            root,
            repo,
            approval,
            head,
        ] => {
            wiki_archive::lineage::retain_command(Path::new(root), Path::new(repo), approval, head)
        }
        ["wiki-sediment-adapt-lineage-result", root, approval] => {
            wiki_archive::lineage::result(Path::new(root), approval)
        }
        ["wiki-sediment-adapt-result", root, digest] => {
            wiki_archive::adapt::result(Path::new(root), digest)
        }
        ["wiki-sediment-archive", root, repo, claim, head, approval] => {
            wiki_archive::archive(Path::new(root), Path::new(repo), claim, head, approval)
        }
        ["cache-clone", repo, commit, destination] => {
            let n = ai4math_broker::clone_golden(
                &Path::new(repo).join("ai4math-golden"),
                commit,
                Path::new(destination),
            )?;
            print_json(serde_json::json!({"cloned_artifacts": n}))
        }
        [
            "hammer-reconstruct",
            root,
            repo,
            claim,
            commit,
            premises,
            exec,
        ] => hammer::run(
            Path::new(root),
            Path::new(repo),
            claim,
            commit,
            Path::new(premises),
            Path::new(exec),
        ),
        ["oracle-probe", query, z3, cvc5] => {
            oracle::probe(Path::new(query), Path::new(z3), Path::new(cvc5))
        }
        ["oracle-preview", root, repo, claim, query, z3, cvc5] => oracle::preview(
            Path::new(root),
            Path::new(repo),
            claim,
            Path::new(query),
            Path::new(z3),
            Path::new(cvc5),
        ),
        [
            "oracle-retain",
            root,
            repo,
            claim,
            approved,
            query,
            z3,
            cvc5,
        ] => oracle::retain(
            Path::new(root),
            Path::new(repo),
            claim,
            approved,
            Path::new(query),
            Path::new(z3),
            Path::new(cvc5),
        ),
        ["oracle-result", root, id] => oracle::read_retained(Path::new(root), id),
        ["oracle-hold", root, repo, task, id, seconds] => oracle::hold(
            Path::new(root),
            Path::new(repo),
            task,
            id,
            seconds
                .parse()
                .context("expected Oracle hold duration in seconds")?,
        ),
        ["oracle-release", root, task, hold] => oracle::release_hold(Path::new(root), task, hold),
        ["oracle-hold-status", root, task] => oracle::hold_status(Path::new(root), task),
        [
            "rocq-t2-check",
            root,
            repo,
            module,
            theorem,
            reviewed,
            compiler,
            data,
            compiler_sha256,
            checker,
            checker_sha256,
        ] => rocq_t2::check(
            Path::new(root),
            Path::new(repo),
            reviewed,
            (module, theorem),
            rocq_t2::Inputs {
                compiler: Path::new(compiler),
                data: Path::new(data),
                compiler_sha256,
                checker: Path::new(checker),
                checker_sha256,
            },
        ),
        ["linux-cgroup-preflight", path] => {
            let readiness = ai4math_broker::linux_cgroup::preflight(Path::new(path))?;
            print_json(serde_json::json!({
                "schema": "ai4math.linux-cgroup-preflight.v1",
                "mode": "READ_ONLY",
                "backend_enabled": false,
                "delegation_verified": false,
                "limits": readiness,
            }))
        }
        ["self-test"] => {
            sandbox_self_test()?;
            println!("sandbox self-test passed");
            Ok(())
        }
        ["promote", repo, module, theorem, exec, body] => promote(
            Path::new(repo),
            module,
            theorem,
            Path::new(exec),
            Path::new(body),
            None,
        ),
        ["promote", repo, module, theorem, exec, body, data] => promote(
            Path::new(repo),
            module,
            theorem,
            Path::new(exec),
            Path::new(body),
            Some(data),
        ),
        [
            "declare-claim",
            root,
            id,
            module,
            target,
            title,
            statement,
            "--critical",
        ] => enroll_claim(Path::new(root), id, module, target, title, statement, true),
        ["bind-lean-spec", root, repo, id, spec, decl, commit] => {
            reviewed_spec::bind(Path::new(root), Path::new(repo), id, spec, decl, commit)
        }
        ["lean-spec-comparison", root, commit] => {
            reviewed_spec::read_comparison(Path::new(root), commit)
        }
        ["lean-spec-recheck", root, claim, plan] => {
            reviewed_spec::read_recheck(Path::new(root), claim, plan)
        }
        ["bind-witness-spec", root, repo, id, module, prop, commit] => {
            witness::bind(Path::new(root), Path::new(repo), id, module, prop, commit)
        }
        ["refute-managed", root, repo, id, module, prop, exec, body] => witness::run(
            Path::new(root),
            Path::new(repo),
            id,
            module,
            prop,
            Path::new(exec),
            Path::new(body),
        ),
        ["refute-pending", root] => witness::pending(Path::new(root)),
        ["refute-reconcile", root] => witness::reconcile(Path::new(root), None),
        ["refute-reconcile", root, claim] => witness::reconcile(Path::new(root), Some(claim)),
        ["declare-claim", root, id, module, target, title, statement] => {
            declare_claim(Path::new(root), id, module, target, title, statement)
        }
        [
            "promote-reviewed-body",
            root,
            repo,
            claim,
            module,
            target,
            commit,
            exec,
            body,
        ] => promote_managed_body(
            Path::new(root),
            Path::new(repo),
            claim,
            module,
            target,
            Path::new(exec),
            ManagedProofInput::ReviewedFile {
                file: Path::new(body),
                reviewed_base: commit,
            },
            None,
        ),
        [
            "promote-reviewed-body",
            root,
            repo,
            claim,
            module,
            target,
            commit,
            exec,
            body,
            data,
        ] => promote_managed_body(
            Path::new(root),
            Path::new(repo),
            claim,
            module,
            target,
            Path::new(exec),
            ManagedProofInput::ReviewedFile {
                file: Path::new(body),
                reviewed_base: commit,
            },
            Some(data),
        ),
        [
            "promote-managed",
            root,
            repo,
            claim,
            module,
            target,
            exec,
            body,
        ] => promote_managed(
            Path::new(root),
            Path::new(repo),
            claim,
            module,
            target,
            Path::new(exec),
            Path::new(body),
            None,
        ),
        [
            "promote-managed",
            root,
            repo,
            claim,
            module,
            target,
            exec,
            body,
            data,
        ] => promote_managed(
            Path::new(root),
            Path::new(repo),
            claim,
            module,
            target,
            Path::new(exec),
            Path::new(body),
            Some(data),
        ),
        ["recover-promotion", root, repo, exec] => {
            recover_promotion(Path::new(root), Path::new(repo), Path::new(exec), None)
        }
        ["recover-promotion", root, repo, exec, data] => recover_promotion(
            Path::new(root),
            Path::new(repo),
            Path::new(exec),
            Some(data),
        ),
        ["trip-breaker", root, repo, id, module, target, reason] => pivot::trip(
            Path::new(root),
            Path::new(repo),
            id,
            module,
            target,
            reason,
            false,
        ),
        [
            "trip-breaker",
            root,
            repo,
            id,
            module,
            target,
            reason,
            "--critical",
        ] => pivot::trip(
            Path::new(root),
            Path::new(repo),
            id,
            module,
            target,
            reason,
            true,
        ),
        [
            "owner-archive-signing-message",
            root,
            repo,
            base,
            adr,
            request,
            expires,
        ] => {
            use std::io::Write;
            let approval = owner_approval::ArchiveApproval::new(
                Path::new(root),
                Path::new(repo),
                base,
                adr,
                request,
                expires.parse()?,
            )?;
            std::io::stdout()
                .lock()
                .write_all(approval.signing_bytes())?;
            Ok(())
        }
        [
            "owner-archive-verify",
            root,
            repo,
            base,
            adr,
            request,
            expires,
            key,
            sig,
        ] => {
            let approval = owner_approval::ArchiveApproval::new(
                Path::new(root),
                Path::new(repo),
                base,
                adr,
                request,
                expires.parse()?,
            )?;
            let key = owner_approval::TrustedOwnerKey::read(Path::new(key))?;
            let signature = owner_approval::read_signature(Path::new(sig))?;
            approval.verify(&key, &signature)?;
            print_json(
                serde_json::json!({"signature_valid":true,"diagnostic_only":true,"applied":false}),
            )
        }
        [
            "owner-archive-apply",
            root,
            repo,
            fact,
            breaker,
            reason,
            base,
            adr,
            id,
            expires,
            signature,
        ] => pivot::apply_signed_archive(
            Path::new(root),
            Path::new(repo),
            pivot::SignedArchive {
                fact,
                breaker,
                reason,
                base,
                adr_sha256: adr,
                request_id: id,
                expires_unix: expires.parse()?,
                signature: Path::new(signature),
            },
        ),
        ["owner-archive-status", root, id] => pivot::owner_archive_status(Path::new(root), id),
        ["owner-archive-recover", root, repo, id] => {
            pivot::recover_signed_archive(Path::new(root), Path::new(repo), id)
        }
        ["owner-archive-preview", root, repo, fact, breaker, reason] => {
            pivot::owner_archive_preview(Path::new(root), Path::new(repo), fact, breaker, reason)
        }
        [
            mode @ ("pivot-preview" | "pivot-archive-preview"),
            root,
            repo,
            fact,
            id,
            reason,
        ] => {
            let action = if *mode == "pivot-archive-preview" {
                ai4math_store::PivotAction::ArchiveBreaker
            } else {
                ai4math_store::PivotAction::ResumeAsIs
            };
            pivot::preview(Path::new(root), Path::new(repo), fact, id, reason, action)
        }
        [
            mode @ ("pivot-apply" | "pivot-archive-apply"),
            root,
            repo,
            fact,
            id,
            reason,
            approved,
        ] => {
            let action = if *mode == "pivot-archive-apply" {
                ai4math_store::PivotAction::ArchiveBreaker
            } else {
                ai4math_store::PivotAction::ResumeAsIs
            };
            pivot::apply(
                Path::new(root),
                Path::new(repo),
                fact,
                id,
                reason,
                approved,
                action,
            )
        }
        [
            "pivot-routes-preview",
            root,
            repo,
            fact,
            id,
            reason,
            plans @ ..,
        ] => pivot::preview_routes(Path::new(root), Path::new(repo), fact, id, reason, plans),
        [
            "pivot-routes-apply",
            root,
            repo,
            fact,
            id,
            reason,
            approved,
            plans @ ..,
        ] => pivot::apply_routes(
            Path::new(root),
            Path::new(repo),
            fact,
            id,
            reason,
            approved,
            plans,
        ),
        [
            "pivot-lesson-preview",
            root,
            repo,
            fact,
            id,
            reason,
            title,
            lesson,
            plans @ ..,
        ] => {
            let draft = ai4math_store::PivotLessonDraft::new(title, lesson)?;
            pivot::preview_lesson(
                Path::new(root),
                Path::new(repo),
                fact,
                id,
                reason,
                &draft,
                plans,
            )
        }
        [
            "pivot-lesson-apply",
            root,
            repo,
            fact,
            id,
            reason,
            approved,
            title,
            lesson,
            plans @ ..,
        ] => {
            let draft = ai4math_store::PivotLessonDraft::new(title, lesson)?;
            pivot::apply_lesson(
                Path::new(root),
                Path::new(repo),
                fact,
                id,
                reason,
                approved,
                &draft,
                plans,
            )
        }
        ["pivot-recover", root, repo] => pivot::recover(Path::new(root), Path::new(repo)),
        ["pivot-abandon", root, repo, id] => pivot::abandon(Path::new(root), Path::new(repo), id),
        ["breaker-probes-preview", root, repo, id, cases @ ..] => {
            breaker_probes::preview(Path::new(root), Path::new(repo), id, cases)
        }
        ["breaker-probes-arm", root, repo, id, approved, cases @ ..] => {
            breaker_probes::arm(Path::new(root), Path::new(repo), id, approved, cases)
        }
        ["breaker-probes-run", root, repo, id, approved, exec] => breaker_probes::run(
            Path::new(root),
            Path::new(repo),
            id,
            approved,
            Path::new(exec),
        ),
        [
            "task-register",
            root,
            repo,
            id,
            track,
            module,
            target,
            deps @ ..,
        ] => breaker_probes::enroll(
            Path::new(root),
            Path::new(repo),
            id,
            track.parse()?,
            module,
            target,
            deps,
        ),
        ["infra-rfc-submit", root, repo, module, reviewed, input] => infra_rfc::submit(
            Path::new(root),
            Path::new(repo),
            module,
            reviewed,
            Path::new(input),
        ),
        ["infra-rfc-approve", root, repo, id, reviewed, sha] => {
            infra_rfc::approve(Path::new(root), Path::new(repo), id, reviewed, sha)
        }
        ["infra-rfc-smoke-start", root, repo, id, reviewed, sha] => {
            infra_rfc::smoke_start(Path::new(root), Path::new(repo), id, reviewed, sha)
        }
        [
            "infra-rfc-freeze",
            root,
            repo,
            id,
            reviewed,
            sha,
            executable,
            lemmas @ ..,
        ] => infra_rfc::freeze(
            Path::new(root),
            Path::new(repo),
            id,
            reviewed,
            sha,
            Path::new(executable),
            lemmas,
        ),
        [
            "infra-rfc-complete",
            root,
            repo,
            id,
            reviewed,
            sha,
            executable,
        ] => infra_rfc::complete(
            Path::new(root),
            Path::new(repo),
            id,
            reviewed,
            sha,
            Path::new(executable),
        ),
        ["infra-rfc-reject", root, repo, id, reviewed, sha, reason] => {
            infra_rfc::reject(Path::new(root), Path::new(repo), id, reviewed, sha, reason)
        }
        [
            "infra-rfc-supersede",
            root,
            repo,
            id,
            reviewed,
            sha,
            replacement,
            reason,
        ] => infra_rfc::supersede(
            Path::new(root),
            Path::new(repo),
            id,
            reviewed,
            sha,
            replacement,
            reason,
        ),
        ["infra-rfc-inspect", root, repo, id] => {
            infra_rfc::inspect(Path::new(root), Path::new(repo), id)
        }
        ["task-bind", root, repo, module, reviewed] => {
            worker_dispatch::bind(Path::new(root), Path::new(repo), module, reviewed)
        }
        ["task-run", root, repo, task, claim, role, exe, sha, seconds] => worker_dispatch::run(
            Path::new(root),
            Path::new(repo),
            task,
            claim,
            role.parse()?,
            Path::new(exe),
            sha,
            seconds.parse()?,
        ),
        ["task-run-model", root, repo, task, claim, role, seconds] => worker_dispatch::run_model(
            Path::new(root),
            Path::new(repo),
            task,
            claim,
            role.parse()?,
            seconds.parse()?,
        ),
        [
            "model-dispatch-approve",
            root,
            repo,
            task,
            claim,
            role,
            reviewed,
            model,
            seconds,
            calls,
        ] => worker_dispatch::approve_model(
            Path::new(root),
            Path::new(repo),
            reviewed,
            worker_dispatch::ModelApprovalOptions {
                task,
                claim,
                role: role.parse()?,
                model,
                seconds: seconds.parse()?,
                max_model_calls: calls.parse()?,
            },
        ),
        ["model-dispatch-status", root, task] => {
            worker_dispatch::model_dispatch_status(Path::new(root), task)
        }
        ["model-dispatch-cancel", root, task] => {
            worker_dispatch::cancel_model_dispatch(Path::new(root), task)
        }
        ["model-dispatch-reconcile", root, task] => {
            worker_dispatch::reconcile_model_dispatch(Path::new(root), task)
        }
        ["model-dispatch-cancel-running", root, task] => {
            worker_dispatch::cancel_running_model_dispatch(Path::new(root), task)
        }
        ["model-dispatch-resident-scan", root, repo, model, limit] => {
            worker_dispatch::resident_model_dispatch_scan(
                Path::new(root),
                Path::new(repo),
                model,
                limit.parse()?,
            )
        }
        [
            "task-run-approved-model",
            root,
            repo,
            task,
            claim,
            role,
            seconds,
            model,
            calls,
        ] => worker_dispatch::run_approved_model(
            Path::new(root),
            Path::new(repo),
            worker_dispatch::ModelApprovalOptions {
                task,
                claim,
                role: role.parse()?,
                model,
                seconds: seconds.parse()?,
                max_model_calls: calls.parse()?,
            },
        ),
        ["task-result", root, id] => worker_dispatch::result(Path::new(root), id),
        ["formal-sketch-task", root, repo, id, exe, probe] => {
            formal_sketch_task(Path::new(root), Path::new(repo), id, Path::new(exe), probe)
        }
        ["oracle-backed-task", root, repo, id, exe, probe] => {
            oracle_backed_task(Path::new(root), Path::new(repo), id, Path::new(exe), probe)
        }
        ["promote-task-result", root, repo, id, exe] => {
            worker_dispatch::promote(Path::new(root), Path::new(repo), id, Path::new(exe), None)
        }
        ["promote-task-result", root, repo, id, exe, data] => worker_dispatch::promote(
            Path::new(root),
            Path::new(repo),
            id,
            Path::new(exe),
            Some(data),
        ),
        ["tasks-status", root] => {
            print_json(open_managed_state(Path::new(root), true)?.task_scheduler_status()?)
        }
        [
            "t2-check-export",
            export,
            export_hash,
            checker,
            checker_hash,
        ] => t2_export::run(
            Path::new(export),
            export_hash,
            Path::new(checker),
            checker_hash,
        ),
        [
            "t2-check-managed",
            root,
            repo,
            module,
            declaration,
            reviewed,
            executable,
            exporter_sha256,
            checker_sha256,
        ] => print_json(t2_build::audit_managed_theorem(
            t2_build::ManagedTheoremRequest {
                root: Path::new(root),
                repo: Path::new(repo),
                module,
                declaration,
                reviewed,
                executable: Path::new(executable),
                exporter_sha256,
                checker_sha256,
            },
        )?),
        [
            "t2-check-batch",
            root,
            repo,
            anchor,
            reviewed,
            executable,
            exporter_sha256,
            checker_sha256,
            targets,
        ] => print_json(t2_batch::run(t2_batch::BatchRequest {
            root: Path::new(root),
            repo: Path::new(repo),
            anchor,
            reviewed,
            executable: Path::new(executable),
            exporter_sha256,
            checker_sha256,
            targets: Path::new(targets),
        })?),
        #[cfg(unix)]
        [
            "t2-release-check",
            root,
            repo,
            anchor,
            reviewed,
            executable,
            policy_sha256,
            compiler_sha256,
            exporter_sha256,
            checker_sha256,
            "--daemon-parent",
        ] => t2_batch::release_for_daemon(t2_batch::ReleaseRequest {
            root: Path::new(root),
            repo: Path::new(repo),
            anchor,
            reviewed,
            executable: Path::new(executable),
            policy_sha256,
            compiler_sha256,
            exporter_sha256,
            checker_sha256,
        }),
        [
            "t2-release-check",
            root,
            repo,
            anchor,
            reviewed,
            executable,
            policy_sha256,
            compiler_sha256,
            exporter_sha256,
            checker_sha256,
        ] => print_json(t2_batch::release(t2_batch::ReleaseRequest {
            root: Path::new(root),
            repo: Path::new(repo),
            anchor,
            reviewed,
            executable: Path::new(executable),
            policy_sha256,
            compiler_sha256,
            exporter_sha256,
            checker_sha256,
        })?),
        [
            "t2-release-tag",
            root,
            repo,
            anchor,
            reviewed,
            executable,
            policy_sha256,
            compiler_sha256,
            exporter_sha256,
            checker_sha256,
            label,
        ] => print_json(t2_batch::release_tag(
            t2_batch::ReleaseRequest {
                root: Path::new(root),
                repo: Path::new(repo),
                anchor,
                reviewed,
                executable: Path::new(executable),
                policy_sha256,
                compiler_sha256,
                exporter_sha256,
                checker_sha256,
            },
            label,
        )?),
        ["t2-release-inspect", root, repo, label] => print_json(t2_batch::inspect_release(
            Path::new(root),
            Path::new(repo),
            label,
            None,
        )?),
        ["t2-release-inspect", root, repo, label, expected_oid] => print_json(
            t2_batch::inspect_release(Path::new(root), Path::new(repo), label, Some(expected_oid))?,
        ),
        #[cfg(unix)]
        [
            "t2-compare-managed-heartbeats",
            root,
            repo,
            target,
            baseline,
            reviewed,
            executable,
            compiler_pin,
            raw_delta,
            relative_percent,
            "--daemon-parent",
        ] => {
            let result =
                t2_build::compare_heartbeats_for_daemon(t2_build::ManagedHeartbeatRequest {
                    root: Path::new(root),
                    repo: Path::new(repo),
                    target,
                    baseline: t2_build::HeartbeatBaseline::Commit(baseline),
                    reviewed,
                    executable: Path::new(executable),
                    compiler_pin,
                    raw_delta,
                    relative_percent,
                });
            // Faults are not observations, and diagnostics may contain project paths.
            if result.is_err() {
                std::process::exit(1);
            }
            Ok(())
        }
        [
            "t2-compare-managed-heartbeats",
            root,
            repo,
            target,
            baseline,
            reviewed,
            executable,
            compiler_pin,
            raw_delta,
            relative_percent,
        ] => t2_build::compare_managed_heartbeats(t2_build::ManagedHeartbeatRequest {
            root: Path::new(root),
            repo: Path::new(repo),
            target,
            baseline: t2_build::HeartbeatBaseline::Commit(baseline),
            reviewed,
            executable: Path::new(executable),
            compiler_pin,
            raw_delta,
            relative_percent,
        }),
        [
            "t2-compare-release-heartbeats",
            root,
            repo,
            target,
            label,
            oid,
            reviewed,
            executable,
            compiler_pin,
            raw_delta,
            relative_percent,
        ] => t2_build::compare_managed_heartbeats(t2_build::ManagedHeartbeatRequest {
            root: Path::new(root),
            repo: Path::new(repo),
            target,
            baseline: t2_build::HeartbeatBaseline::LocalRelease { label, oid },
            reviewed,
            executable: Path::new(executable),
            compiler_pin,
            raw_delta,
            relative_percent,
        }),
        ["t2-compare-heartbeats", baseline, candidate, delta, percent] => {
            t2_compare_io::run_heartbeats(Path::new(baseline), Path::new(candidate), delta, percent)
        }
        ["t2-compare", baseline, candidate, delta, percent] => {
            t2_compare_io::run(Path::new(baseline), Path::new(candidate), delta, percent)
        }
        #[cfg(unix)]
        [
            "t2-audit",
            root,
            repo,
            anchor,
            reviewed,
            executable,
            "--daemon-parent",
        ] => t2_build::audit_for_daemon(
            Path::new(root),
            Path::new(repo),
            anchor,
            reviewed,
            Path::new(executable),
        ),
        ["t2-audit", root, repo, anchor, reviewed, executable] => {
            print_json(t2_build::audit_with_debt(
                Path::new(root),
                Path::new(repo),
                anchor,
                reviewed,
                Path::new(executable),
            )?)
        }
        [
            "t2-heartbeats",
            root,
            repo,
            module,
            reviewed,
            executable,
            compiler_sha256,
        ] => print_json(t2_build::audit_heartbeats(
            Path::new(root),
            Path::new(repo),
            module,
            reviewed,
            Path::new(executable),
            compiler_sha256,
        )?),
        ["t2-build", root, repo, anchor, reviewed, executable] => print_json(t2_build::audit(
            Path::new(root),
            Path::new(repo),
            anchor,
            reviewed,
            Path::new(executable),
        )?),
        #[cfg(unix)]
        [
            "t3-discover",
            root,
            repo,
            anchor,
            baseline,
            "--daemon-parent",
        ] => t3::discover_for_daemon(Path::new(root), Path::new(repo), anchor, baseline),
        ["t3-discover", root, repo, anchor, baseline] => {
            t3::discover(Path::new(root), Path::new(repo), anchor, baseline)
        }
        #[cfg(unix)]
        ["t3-discover-claim", root, repo, claim, "--daemon-parent"] => {
            t3::discover_claim_for_daemon(Path::new(root), Path::new(repo), claim)
        }
        ["t3-discover-claim", root, repo, claim] => {
            t3::discover_claim(Path::new(root), Path::new(repo), claim)
        }
        ["t3-preview", root, repo, anchor, old, reviewed] => {
            t3::dry_run(Path::new(root), Path::new(repo), anchor, old, reviewed)
        }
        ["t3-apply", root, repo, anchor, old, reviewed, hash] => t3::apply(
            Path::new(root),
            Path::new(repo),
            anchor,
            old,
            reviewed,
            hash,
        ),
        ["t3-worklist", root, repo, hash, limit] => {
            t3::worklist(Path::new(root), Path::new(repo), hash, limit.parse()?, None)
        }
        ["t3-worklist", root, repo, hash, limit, after] => t3::worklist(
            Path::new(root),
            Path::new(repo),
            hash,
            limit.parse()?,
            Some(after),
        ),
        ["t3-recheck-one", root, repo, hash, claim, exec] => t3::recheck_one(
            Path::new(root),
            Path::new(repo),
            hash,
            claim,
            Path::new(exec),
            None,
        ),
        ["t3-recheck-one", root, repo, hash, claim, exec, data] => t3::recheck_one(
            Path::new(root),
            Path::new(repo),
            hash,
            claim,
            Path::new(exec),
            Some(data),
        ),
        ["t3-sweep-status", root, repo, hash] => {
            t3::sweep_status(Path::new(root), Path::new(repo), hash)
        }
        ["t3-sweep-step", root, repo, hash, limit, exec, args @ ..] => t3::sweep_step(
            Path::new(root),
            Path::new(repo),
            hash,
            limit.parse()?,
            Path::new(exec),
            args,
        ),
        ["t3-recheck-batch", root, repo, hash, limit, exec, args @ ..] => t3::recheck_batch(
            Path::new(root),
            Path::new(repo),
            hash,
            limit.parse()?,
            Path::new(exec),
            args,
        ),
        ["t3-recheck", root, repo, hash, exec] => t3::recheck(
            Path::new(root),
            Path::new(repo),
            hash,
            Path::new(exec),
            None,
        ),
        ["t3-recheck", root, repo, hash, exec, data] => t3::recheck(
            Path::new(root),
            Path::new(repo),
            hash,
            Path::new(exec),
            Some(data),
        ),
        _ => bail!("invalid arguments\n{HELP}"),
    }
}

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ai4math: {e:#}");
            std::process::ExitCode::FAILURE
        }
    }
}
