mod breaker_probes;
mod hammer;
#[cfg(test)]
mod managed_tests;
mod oracle;
mod pivot;
mod promotion;
mod reviewed_spec;
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
    AgdaMode, Broker, ProjectNativeGate, ProjectNativeLease, RegressionArtifacts,
    RegressionFailure, Toolchain, VerifiedCandidate, VerifiedLeanComparison, sandbox_self_test,
};
use ai4math_core::{Engine, FrozenTarget, ProofBodySubmission};
use ai4math_store::{
    Change, NewClaim, PromotionIntent, ReviewedLeanSpec, StateStore, VerifiedReceipt, Wiki,
};
use anyhow::{Context, Result, bail, ensure};
use promotion::{PendingRef, Promotion, PromotionRepo};
use std::{env, fs, io::Read, path::Path};

const HELP: &str = "ai4math init ROOT [lean4|agda|rocq]\nai4math status ROOT\nai4math claim-replay ROOT CLAIM [THROUGH_EVENT_SEQ]\nai4math plan-replay ROOT PLAN [THROUGH_EVENT_SEQ]\nai4math graph-replay ROOT [THROUGH_EVENT_SEQ]\nai4math graph-audit ROOT\nai4math graph-check ROOT\nai4math wiki-rebuild ROOT\nai4math wiki-core-rebuild ROOT BARE_REPO REVIEWED_MAIN\nai4math wiki-core-search ROOT BARE_REPO REVIEWED_MAIN TEXT\nai4math wiki-core-provenance ROOT BARE_REPO REVIEWED_MAIN ENTRY_ID\nai4math wiki-sediment-preview ROOT BARE_REPO CLAIM REVIEWED_MAIN\nai4math wiki-sediment-archive ROOT BARE_REPO CLAIM REVIEWED_MAIN APPROVAL_SHA256\nai4math wiki-sediment-wake-preview ROOT BARE_REPO ARCHIVE_ID REVIEWED_MAIN\nai4math wiki-sediment-wake-recheck ROOT BARE_REPO ARCHIVE_ID REVIEWED_MAIN NATIVE_LEAN\nai4math wiki-sediment-wake-retain ROOT BARE_REPO ARCHIVE_ID REVIEWED_MAIN NATIVE_LEAN\nai4math wiki-sediment-wake-result ROOT OBSERVATION_SHA256\nai4math wiki-sediment-adapt-preview ROOT BARE_REPO ARCHIVE_ID REVIEWED_MAIN TARGET_CLAIM\nai4math wiki-sediment-adapt-approve ROOT BARE_REPO ARCHIVE_ID REVIEWED_MAIN TARGET_CLAIM APPROVAL_SHA256\nai4math wiki-sediment-adapt-result ROOT APPROVAL_SHA256\nai4math wiki-sediment-adapt-run ROOT BARE_REPO APPROVAL_SHA256 WORKER_EXE WORKER_SHA256 WALL_SECONDS\nai4math wiki-sediment-adapt-lineage-retain ROOT BARE_REPO APPROVAL_SHA256 PROMOTED_HEAD\nai4math wiki-sediment-adapt-lineage-result ROOT APPROVAL_SHA256\nai4math self-test\nai4math hammer-reconstruct ROOT BARE_REPO CLAIM REVIEWED_COMMIT PREMISES_JSON NATIVE_LEAN\nai4math oracle-probe QUERY_JSON NATIVE_Z3 NATIVE_CVC5\nai4math oracle-preview ROOT BARE_REPO CLAIM QUERY_JSON NATIVE_Z3 NATIVE_CVC5\nai4math oracle-retain ROOT BARE_REPO CLAIM APPROVAL_SHA256 QUERY_JSON NATIVE_Z3 NATIVE_CVC5\nai4math oracle-result ROOT RECORD_SHA256\nai4math oracle-hold ROOT BARE_REPO TASK RECORD_SHA256 SECONDS\nai4math oracle-release ROOT TASK HOLD_SHA256\nai4math oracle-hold-status ROOT TASK\nai4math promote BARE_REPO MODULE TARGET NATIVE_ENGINE PROOF_BODY_FILE [ENGINE_DATA]\nai4math declare-claim ROOT CLAIM MODULE TARGET TITLE STATEMENT [--critical]\nai4math bind-lean-spec ROOT BARE_REPO CLAIM SPEC_MODULE SPEC_DECL APPROVED_COMMIT\nai4math lean-spec-comparison ROOT PROMOTED_COMMIT\nai4math lean-spec-recheck ROOT CLAIM PLAN_SHA256\nai4math bind-witness-spec ROOT BARE_REPO CLAIM MODULE PROPOSITION APPROVED_COMMIT\nai4math refute-managed ROOT BARE_REPO CLAIM MODULE PROPOSITION NATIVE_LEAN PROOF_BODY_FILE\nai4math promote-reviewed-body ROOT BARE_REPO CLAIM MODULE TARGET REVIEWED_COMMIT NATIVE_ENGINE PROOF_BODY_FILE [ENGINE_DATA]\nai4math promote-managed ROOT BARE_REPO CLAIM MODULE TARGET NATIVE_ENGINE PROOF_BODY_FILE [ENGINE_DATA]\nai4math recover-promotion ROOT BARE_REPO NATIVE_ENGINE [ENGINE_DATA]\nai4math t2-build ROOT BARE_REPO ANCHOR_MODULE REVIEWED_MAIN NATIVE_LEAN\nai4math t2-heartbeats ROOT BARE_REPO TARGET_MODULE REVIEWED_MAIN NATIVE_LEAN COMPILER_SHA256\nai4math t2-audit ROOT BARE_REPO ANCHOR_MODULE REVIEWED_MAIN NATIVE_LEAN\nai4math t2-compare BASELINE_JSON CANDIDATE_JSON MIN_DELTA_MS MIN_INCREASE_PERCENT\nai4math t2-compare-heartbeats BASELINE_JSON CANDIDATE_JSON MIN_DELTA_HEARTBEATS_RAW MIN_INCREASE_PERCENT\nai4math t2-trust-debt ROOT\nai4math t2-check-export EXPORT_NDJSON EXPORT_SHA256 NATIVE_NANODA CHECKER_SHA256\nai4math t2-check-managed ROOT BARE_REPO TARGET_MODULE DECLARATION REVIEWED_MAIN NATIVE_LEAN EXPORTER_SHA256 CHECKER_SHA256\nai4math t2-check-batch ROOT BARE_REPO ANCHOR_MODULE REVIEWED_MAIN NATIVE_LEAN EXPORTER_SHA256 CHECKER_SHA256 TARGETS_JSON\nai4math t2-release-check ROOT BARE_REPO ANCHOR_MODULE REVIEWED_MAIN NATIVE_LEAN POLICY_SHA256 COMPILER_SHA256 EXPORTER_SHA256 CHECKER_SHA256\nai4math t2-release-tag ROOT BARE_REPO ANCHOR_MODULE REVIEWED_MAIN NATIVE_LEAN POLICY_SHA256 COMPILER_SHA256 EXPORTER_SHA256 CHECKER_SHA256 LABEL\nai4math t2-release-inspect ROOT BARE_REPO LABEL [EXPECTED_TAG_OID]\nai4math t3-discover ROOT BARE_REPO ANCHOR_MODULE BASELINE_COMMIT\nai4math t3-discover-claim ROOT BARE_REPO CLAIM_ID\nai4math t3-preview ROOT BARE_REPO ANCHOR_MODULE OLD_COMMIT REVIEW_COMMIT\nai4math t3-apply ROOT BARE_REPO ANCHOR_MODULE OLD_COMMIT REVIEW_COMMIT PLAN_SHA256\nai4math t3-recheck ROOT BARE_REPO PLAN_SHA256 NATIVE_ENGINE [ENGINE_DATA]\nai4math t3-worklist ROOT BARE_REPO PLAN_SHA256 LIMIT [AFTER_CLAIM]\nai4math t3-recheck-one ROOT BARE_REPO PLAN_SHA256 CLAIM NATIVE_ENGINE [ENGINE_DATA]\nai4math t3-recheck-batch ROOT BARE_REPO PLAN_SHA256 LIMIT NATIVE_ENGINE [ENGINE_DATA] [--after CLAIM EVENT_SEQ]\nai4math t3-sweep-status ROOT BARE_REPO PLAN_SHA256\nai4math t3-sweep-step ROOT BARE_REPO PLAN_SHA256 LIMIT NATIVE_ENGINE [ENGINE_DATA] [--restart]\nai4math trip-breaker ROOT BARE_REPO BREAKER MODULE DEFINITION REASON [--critical]
ai4math pivot-preview ROOT BARE_REPO FACT_SHA256 BREAKER REASON
ai4math pivot-apply ROOT BARE_REPO FACT_SHA256 BREAKER REASON APPROVED_ADR_ID
ai4math pivot-recover ROOT BARE_REPO
ai4math pivot-abandon ROOT BARE_REPO APPROVED_ADR_ID
ai4math breaker-probes-preview ROOT BARE_REPO BREAKER THEOREM [THEOREM]
ai4math breaker-probes-arm ROOT BARE_REPO BREAKER APPROVAL_ID THEOREM [THEOREM]
ai4math breaker-probes-run ROOT BARE_REPO BREAKER APPROVAL_ID NATIVE_LEAN
ai4math task-register ROOT BARE_REPO ID TRACK MODULE TARGET [DEP_ID ...]
ai4math tasks-status ROOT
ai4math task-bind ROOT BARE_REPO ANCHOR_MODULE REVIEWED_COMMIT
ai4math task-run ROOT BARE_REPO ID CLAIM ROLE WORKER_EXE SHA256 WALL_SECONDS
ai4math task-run-model ROOT BARE_REPO ID CLAIM ROLE WALL_SECONDS
ai4math task-result ROOT ID
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
    let path = worker_dispatch::private_state_path(root)
        .context("replay requires an initialized, owner-controlled project")?;
    StateStore::open_readonly(path)
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
fn managed_native(root: &Path) -> Result<ProjectNativeLease> {
    ProjectNativeGate::new(root).try_heavy()
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
    let mut store = StateStore::open(root.join(".runtime/state.sqlite"))?;
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
    let _native = managed_native(root)?;
    let reviewed_base = match &input {
        ManagedProofInput::Candidate { reviewed_base, .. }
        | ManagedProofInput::ReviewedFile { reviewed_base, .. } => Some(*reviewed_base),
        ManagedProofInput::File(_) => None,
    };
    let engine = bound_engine(root, module)?;
    let locked = PromotionRepo::lock(repo)?;
    let state_path = root.join(".runtime/state.sqlite");
    let mut store = StateStore::open(&state_path)?;
    if let Some(base) = reviewed_base {
        ensure!(
            locked.main_head()? == base,
            "reviewed candidate belongs to an older Git snapshot"
        );
        ensure!(
            store.managed_repo()?.as_deref() == locked.path().to_str(),
            "candidate is bound to a different managed repository"
        );
        ensure!(
            !store.module_breaker_gated(engine.as_str(), module)?,
            "breaker still gates this candidate's module"
        );
        ensure!(
            store.pivot_intent()?.is_none(),
            "recover pending Pivot before candidate promotion"
        );
    }
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
    let _native = managed_native(root)?;
    let locked = PromotionRepo::lock(repo)?;
    let state_path = root.join(".runtime/state.sqlite");
    let mut store = StateStore::open(&state_path)?;
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
            let state = StateStore::open(root.join(".runtime/state.sqlite"))?.status()?;
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
            let wiki = Path::new(root).join(".mathwiki");
            ensure!(wiki.is_dir(), "initialize the project first");
            print_json(
                serde_json::json!({"indexed_entries": Wiki::open(wiki.join(".index.sqlite"))?.rebuild(&wiki)?}),
            )
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
        ["pivot-preview", root, repo, fact, id, reason] => {
            pivot::preview(Path::new(root), Path::new(repo), fact, id, reason)
        }
        ["pivot-apply", root, repo, fact, id, reason, approved] => {
            pivot::apply(Path::new(root), Path::new(repo), fact, id, reason, approved)
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
        ["task-result", root, id] => worker_dispatch::result(Path::new(root), id),
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
        ["tasks-status", root] => print_json(
            StateStore::open_readonly(Path::new(root).join(".runtime/state.sqlite"))?
                .task_scheduler_status()?,
        ),
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
