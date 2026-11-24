//! Operator-only advisory Oracle workflow. No proof or Claim-graph authority.
use super::{bound_engine, print_json, promotion::PromotionRepo};
use ai4math_broker::{
    Broker, MAX_ORACLE_QUERY_BYTES, OracleObservation, OracleQuery, OracleSolvers,
};
use ai4math_store::{OracleApproval, RetainedOracleObservation, StateStore};
use anyhow::{Context, Result, ensure};
use std::{fs, io::Read, path::Path};

fn read_query(path: &Path) -> Result<OracleQuery> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path).context("open Oracle query file")?;
    ensure!(
        file.metadata()?.is_file(),
        "Oracle query must be a regular file"
    );
    let mut bytes = Vec::new();
    file.take((MAX_ORACLE_QUERY_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    OracleQuery::parse(&bytes)
}

pub(crate) fn probe(path: &Path, z3: &Path, cvc5: &Path) -> Result<()> {
    let query = read_query(path)?;
    let solvers = OracleSolvers::new(z3, cvc5)?;
    let observation = Broker::new().probe_bounded_oracle(&solvers, query)?;
    print_json(
        serde_json::json!({"evidence_kind":"advisory_finite_integer_encoding","observation":observation}),
    )
}

fn prepare(
    root: &Path,
    git: &PromotionRepo,
    store: &StateStore,
    claim: &str,
    query_sha: &str,
    solvers: [&str; 2],
) -> Result<OracleApproval> {
    let commit = git.main_head()?;
    let repo = git
        .path()
        .to_str()
        .context("managed repository path is not UTF-8")?;
    let approval = store.oracle_approval(repo, claim, &commit, query_sha, solvers)?;
    let (module, _) = approval
        .formal_target
        .split_once('#')
        .context("missing Oracle formal target module")?;
    ensure!(
        bound_engine(root, module)?.as_str() == approval.engine,
        "Oracle target must use the project primary engine"
    );
    // Confirm that the declared target module is tracked in this reviewed project.
    // This does not prove semantic correspondence between the query and target.
    git.snapshot(module, Some(&commit))?;
    ensure!(
        git.main_head()? == commit,
        "main moved while preparing Oracle approval"
    );
    Ok(approval)
}

fn current_approval(
    root: &Path,
    repo: &Path,
    claim: &str,
    query: &OracleQuery,
    solvers: &OracleSolvers,
) -> Result<OracleApproval> {
    let lock = PromotionRepo::lock(repo)?;
    let store = StateStore::open_readonly(root.join(".runtime/state.sqlite"))?;
    prepare(
        root,
        &lock,
        &store,
        claim,
        &query.fingerprint()?,
        solvers.fingerprints(),
    )
}

pub(crate) fn preview(
    root: &Path,
    repo: &Path,
    claim: &str,
    path: &Path,
    z3: &Path,
    cvc5: &Path,
) -> Result<()> {
    let query = read_query(path)?;
    let solvers = OracleSolvers::new(z3, cvc5)?;
    let approval = current_approval(root, repo, claim, &query, &solvers)?;
    print_json(
        serde_json::json!({"evidence_kind":"advisory_finite_integer_encoding","approval":approval,"query":query}),
    )
}

fn validate_sha(value: &str) -> Result<()> {
    ensure!(
        value.len() == 64
            && value
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)),
        "expected lowercase SHA-256"
    );
    Ok(())
}

pub(crate) fn retain(
    root: &Path,
    repo: &Path,
    claim: &str,
    approved: &str,
    path: &Path,
    z3: &Path,
    cvc5: &Path,
) -> Result<()> {
    validate_sha(approved)?;
    let _native = crate::managed_native(root)?;
    let query = read_query(path)?;
    let solvers = OracleSolvers::new(z3, cvc5)?;
    let approval = current_approval(root, repo, claim, &query, &solvers)?;
    ensure!(
        approval.approval_sha256 == approved,
        "Oracle approval does not match the current query, solver selection, Claim or Git snapshot; rerun oracle-preview"
    );
    // Drop the repository lock during solver runs. Reacquire it and resolve
    // the same context immediately before the atomic advisory append.
    let observation = Broker::new().probe_bounded_oracle(&solvers, query)?;
    print_json(record_current(root, repo, &approval, &observation)?)
}

fn record_current(
    root: &Path,
    repo: &Path,
    approval: &OracleApproval,
    observation: &OracleObservation,
) -> Result<RetainedOracleObservation> {
    let lock = PromotionRepo::lock(repo)?;
    let state = root.join(".runtime/state.sqlite");
    let store = StateStore::open_readonly(&state)?;
    let live = prepare(
        root,
        &lock,
        &store,
        &approval.claim_id,
        observation.query_sha256(),
        observation.solver_sha256s(),
    )?;
    ensure!(
        live == *approval,
        "Oracle approval changed during the probe; evidence not retained"
    );
    drop(store);
    let mut store = StateStore::open(&state)?;
    ensure!(
        lock.main_head()? == approval.commit,
        "main moved before the Oracle append"
    );
    // The sidecar lock serializes project coordinators. A raw external Git
    // update is not a participant in this transaction; the row names the
    // precise observed commit and never claims what main is after the append.
    store.record_oracle_observation(approval, observation)
}

pub(crate) fn read_retained(root: &Path, id: &str) -> Result<()> {
    validate_sha(id)?;
    let store = StateStore::open_readonly(root.join(".runtime/state.sqlite"))?;
    print_json(
        store
            .oracle_observation(id)?
            .context("unknown retained Oracle observation")?,
    )
}

/// Explicit scheduling action, never an automatic consequence of a solver result.
pub(crate) fn hold(root: &Path, repo: &Path, task: &str, id: &str, seconds: u64) -> Result<()> {
    validate_sha(id)?;
    ensure!(
        (1..=3600).contains(&seconds),
        "Oracle hold duration must be between 1 and 3600 seconds"
    );
    let lock = PromotionRepo::lock(repo)?;
    let path = root.join(".runtime/state.sqlite");
    let store = StateStore::open_readonly(&path)?;
    let observation = store
        .oracle_observation(id)?
        .context("unknown retained Oracle observation")?;
    let approval = &observation.approval;
    let live = prepare(
        root,
        &lock,
        &store,
        &approval.claim_id,
        &approval.query_sha256,
        approval.solver_sha256s.each_ref().map(String::as_str),
    )?;
    ensure!(
        live == *approval,
        "Oracle observation is not from the current reviewed project context"
    );
    drop(store);
    let mut store = StateStore::open(&path)?;
    ensure!(
        lock.main_head()? == approval.commit,
        "main moved before the Oracle hold"
    );
    print_json(store.hold_task_for_oracle(&approval.repo, &approval.commit, task, id, seconds)?)
}

pub(crate) fn release_hold(root: &Path, task: &str, hold: &str) -> Result<()> {
    validate_sha(hold)?;
    let mut store = StateStore::open(root.join(".runtime/state.sqlite"))?;
    print_json(store.release_oracle_task_hold(task, hold)?)
}

pub(crate) fn hold_status(root: &Path, task: &str) -> Result<()> {
    let store = StateStore::open_readonly(root.join(".runtime/state.sqlite"))?;
    print_json(serde_json::json!({"hold":store.oracle_task_hold(task)?}))
}

#[cfg(test)]
mod tests;
