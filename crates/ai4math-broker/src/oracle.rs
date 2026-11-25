//! Bounded, advisory integer counterexample probes. No proof authority.
use super::{Broker, Lane, Sha256, digest, resources, sandbox};
use ai4math_evidence::OracleObservationEvidence;
pub use ai4math_evidence::{ExactCheck, MAX_ORACLE_QUERY_BYTES, OracleOutcome, OracleQuery};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use sha2::Digest;
use std::{
    fs,
    io::{self, Read, Seek},
    path::{Path, PathBuf},
    time::Duration,
};

const MAX_SOLVER_BYTES: u64 = 256 * 1024 * 1024;
const SOLVER_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum SolverStatus {
    Sat,
    Unsat,
    Unknown,
    Failed,
    InvalidOutput,
}
#[derive(Debug, Serialize)]
struct SolverReport {
    engine: &'static str,
    executable_sha256: String,
    status: SolverStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    exit_code: Option<i32>,
}
#[derive(Debug, Serialize)]
#[must_use]
pub struct OracleObservation {
    observation_sha256: String,
    query_sha256: String,
    outcome: OracleOutcome,
    query: OracleQuery,
    exact_check: ExactCheck,
    solvers: [SolverReport; 2],
}
impl OracleObservation {
    pub fn id(&self) -> &str {
        &self.observation_sha256
    }

    pub fn query_sha256(&self) -> &str {
        &self.query_sha256
    }

    pub fn solver_sha256s(&self) -> [&str; 2] {
        self.solvers
            .each_ref()
            .map(|r| r.executable_sha256.as_str())
    }

    pub fn outcome(&self) -> OracleOutcome {
        self.outcome
    }
}

// SAFETY: Production observations are created only after typed query preparation
// completes the bounded exact check and both operator-selected native solvers
// finish. Fields remain private and no observation deserializer is exposed.
unsafe impl OracleObservationEvidence for OracleObservation {
    fn id(&self) -> &str {
        self.id()
    }

    fn query_sha256(&self) -> &str {
        self.query_sha256()
    }

    fn solver_sha256s(&self) -> [&str; 2] {
        self.solver_sha256s()
    }

    fn outcome(&self) -> OracleOutcome {
        self.outcome()
    }

    fn raw_json(&self) -> Result<serde_json::Value> {
        Ok(serde_json::to_value(self)?)
    }
}

struct Solver {
    name: &'static str,
    executable: PathBuf,
    sha256: String,
    flag: &'static str,
}
pub struct OracleSolvers([Solver; 2]);
fn solver_digest(path: &Path) -> Result<String> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let mut f = options.open(path)?;
    let metadata = f.metadata()?;
    ensure!(
        metadata.is_file() && metadata.len() <= MAX_SOLVER_BYTES,
        "solver must be a bounded regular file"
    );
    let mut magic = [0; 4];
    f.read_exact(&mut magic)?;
    ensure!(
        matches!(
            magic,
            [0xcf, 0xfa, 0xed, 0xfe] | [0xca, 0xfe, 0xba, 0xbe] | [0xca, 0xfe, 0xba, 0xbf]
        ),
        "pass an operator-trusted native Mach-O solver, not a script or shim"
    );
    f.rewind()?;
    let mut hash = Sha256::new();
    let bytes = io::copy(&mut f.take(MAX_SOLVER_BYTES + 1), &mut hash)?;
    ensure!(
        bytes == metadata.len(),
        "solver file changed while being read"
    );
    Ok(format!("{:x}", hash.finalize()))
}
impl Solver {
    fn new(name: &'static str, path: &Path, flag: &'static str) -> Result<Self> {
        ensure!(path.is_absolute(), "solver path must be absolute");
        let executable = path
            .canonicalize()
            .with_context(|| format!("resolve trusted {name} executable"))?;
        let sha256 = solver_digest(&executable)?;
        Ok(Self {
            name,
            executable,
            sha256,
            flag,
        })
    }
    fn run(&self, script: &str) -> Result<SolverReport> {
        let room = tempfile::tempdir()?;
        let scratch = room.path().join("scratch");
        fs::create_dir(&scratch)?;
        fs::write(room.path().join("query.smt2"), script)?;
        ensure!(
            solver_digest(&self.executable)? == self.sha256,
            "solver binary changed since selection"
        );
        let log = sandbox::run(sandbox::Invocation {
            executable: &self.executable,
            args: &[self.flag.into(), "query.smt2".into()],
            room: room.path(),
            writable: &scratch,
            reads: &[],
            env: &[],
            timeout: SOLVER_TIMEOUT,
        });
        let (status, exit_code) = match log {
            Ok(log) => (parse_status(&log), None),
            Err(error) => (
                SolverStatus::Failed,
                error
                    .downcast_ref::<sandbox::ChildExit>()
                    .and_then(|exit| exit.status.code()),
            ),
        };
        ensure!(
            solver_digest(&self.executable)? == self.sha256,
            "solver binary changed while running"
        );
        Ok(SolverReport {
            engine: self.name,
            executable_sha256: self.sha256.clone(),
            status,
            exit_code,
        })
    }
}
impl OracleSolvers {
    /// Canonical order: operator-selected Z3 then cvc5.
    pub fn fingerprints(&self) -> [&str; 2] {
        self.0.each_ref().map(|solver| solver.sha256.as_str())
    }

    /// Executables are selected by the operator, never by the query or an agent.
    pub fn new(z3: &Path, cvc5: &Path) -> Result<Self> {
        let z3 = Solver::new("z3", z3, "-smt2")?;
        let cvc5 = Solver::new("cvc5", cvc5, "--lang=smt2")?;
        ensure!(
            z3.sha256 != cvc5.sha256,
            "select two distinct trusted solver binaries"
        );
        Ok(Self([z3, cvc5]))
    }
}
fn parse_status(log: &str) -> SolverStatus {
    match log {
        "sat\n" => SolverStatus::Sat,
        "unsat\n" => SolverStatus::Unsat,
        "unknown\n" => SolverStatus::Unknown,
        _ => SolverStatus::InvalidOutput,
    }
}
fn classify(a: SolverStatus, b: SolverStatus, exact_has_counterexample: bool) -> OracleOutcome {
    use SolverStatus::{Sat, Unsat};
    match (a, b, exact_has_counterexample) {
        (Sat, Sat, true) => OracleOutcome::Counterexample,
        (Unsat, Unsat, false) => OracleOutcome::NoCounterexampleWithinBounds,
        (Sat | Unsat, Sat | Unsat, _) => OracleOutcome::Disagreement,
        _ => OracleOutcome::Unavailable,
    }
}

impl Broker {
    /// An advisory check of the supplied encoding, never a kernel proof or a trust transition.
    pub fn probe_bounded_oracle(
        &self,
        solvers: &OracleSolvers,
        query: OracleQuery,
    ) -> Result<OracleObservation> {
        let (script, exact_check) = query.prepare()?.into_parts();
        let _lease = self
            .resources
            .acquire(Lane::Fast, resources::host_pressure()?)?;
        let [z3, cvc5] = &solvers.0;
        let solvers = [z3.run(&script)?, cvc5.run(&script)?];
        let outcome = classify(
            solvers[0].status,
            solvers[1].status,
            exact_check.has_counterexample(),
        );
        let query_sha256 = query.fingerprint()?;
        let observation_sha256 = digest(&serde_json::to_vec(&(
            "ai4math:bounded-oracle:v1",
            &query_sha256,
            &solvers,
            outcome,
            &exact_check,
        ))?);
        Ok(OracleObservation {
            observation_sha256,
            query_sha256,
            outcome,
            query,
            exact_check,
            solvers,
        })
    }
}

#[cfg(test)]
mod tests;
