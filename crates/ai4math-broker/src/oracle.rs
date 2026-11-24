//! Bounded, advisory integer counterexample probes. No proof authority.
use super::{Broker, Digest, Lane, Sha256, digest, resources, sandbox};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fmt::Write,
    fs,
    io::{self, Read, Seek},
    path::{Path, PathBuf},
    time::Duration,
};

pub const MAX_ORACLE_QUERY_BYTES: usize = 32 * 1024;
const MAX_COMBINATIONS: u64 = 65_536;
const MAX_ABS: i64 = 1_000_000_000;
const MAX_SOLVER_BYTES: u64 = 256 * 1024 * 1024;
const SOLVER_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OracleQuery {
    version: u8,
    variables: Vec<Variable>,
    premises: Vec<Predicate>,
    conclusion: Predicate,
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Variable {
    name: String,
    lower: i64,
    upper: i64,
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Term {
    var: String,
    coeff: i64,
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum Relation {
    Le,
    Lt,
    Eq,
    Ge,
    Gt,
    Ne,
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Predicate {
    terms: Vec<Term>,
    relation: Relation,
    constant: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum SolverStatus {
    Sat,
    Unsat,
    Unknown,
    Failed,
    InvalidOutput,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OracleOutcome {
    Counterexample,
    NoCounterexampleWithinBounds,
    Disagreement,
    Unavailable,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
struct ExactCheck {
    assignments_checked: u64,
    premises_satisfying: u64,
    counterexample: Option<Vec<(String, i64)>>,
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

impl OracleQuery {
    /// Fingerprint of the exact parsed query, independent of JSON whitespace.
    pub fn fingerprint(&self) -> Result<String> {
        Ok(digest(&serde_json::to_vec(self)?))
    }

    pub fn parse(bytes: &[u8]) -> Result<Self> {
        ensure!(
            bytes.len() <= MAX_ORACLE_QUERY_BYTES,
            "Oracle query exceeds its byte budget"
        );
        let query: Self =
            serde_json::from_slice(bytes).context("expected a typed Oracle query, not SMT-LIB")?;
        query.validate()?;
        Ok(query)
    }
    fn validate(&self) -> Result<HashMap<&str, usize>> {
        ensure!(self.version == 1, "unsupported Oracle query version");
        ensure!(
            (1..=8).contains(&self.variables.len()) && self.premises.len() <= 16,
            "invalid variable or premise count"
        );
        let bounded = |n: i64| (-MAX_ABS..=MAX_ABS).contains(&n);
        let mut indices = HashMap::new();
        let mut combinations = 1;
        for (i, v) in self.variables.iter().enumerate() {
            ensure!(
                !v.name.is_empty()
                    && v.name.len() <= 32
                    && v.name.starts_with(|c: char| c.is_ascii_alphabetic())
                    && v.name
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_'),
                "invalid Oracle variable name"
            );
            ensure!(
                indices.insert(v.name.as_str(), i).is_none(),
                "duplicate Oracle variable"
            );
            ensure!(
                bounded(v.lower) && bounded(v.upper) && v.lower <= v.upper,
                "invalid Oracle integer bounds"
            );
            combinations *= (v.upper - v.lower + 1) as u64; // capped after every factor, so no overflow
            ensure!(
                combinations <= MAX_COMBINATIONS,
                "Oracle finite domain exceeds 65,536 assignments"
            );
        }
        for p in self.premises.iter().chain([&self.conclusion]) {
            ensure!(
                p.terms.len() <= 8 && bounded(p.constant),
                "invalid Oracle linear predicate size or constant"
            );
            let mut seen = 0u16;
            for term in &p.terms {
                let i = *indices
                    .get(term.var.as_str())
                    .context("unknown Oracle variable")?;
                let mask = 1u16 << i;
                ensure!(
                    seen & mask == 0 && bounded(term.coeff),
                    "duplicate term or out-of-range coefficient"
                );
                seen |= mask;
            }
        }
        Ok(indices)
    }
    fn exact(&self, indices: &HashMap<&str, usize>) -> ExactCheck {
        let mut values: Vec<_> = self.variables.iter().map(|v| v.lower).collect();
        let mut exact = ExactCheck {
            assignments_checked: 0,
            premises_satisfying: 0,
            counterexample: None,
        };
        loop {
            exact.assignments_checked += 1;
            if self.premises.iter().all(|p| p.holds(indices, &values)) {
                exact.premises_satisfying += 1;
                if exact.counterexample.is_none() && !self.conclusion.holds(indices, &values) {
                    exact.counterexample = Some(
                        self.variables
                            .iter()
                            .zip(&values)
                            .map(|(v, n)| (v.name.clone(), *n))
                            .collect(),
                    );
                }
            }
            let Some(i) = (0..values.len())
                .rev()
                .find(|&i| values[i] < self.variables[i].upper)
            else {
                break;
            };
            values[i] += 1;
            for (value, v) in values[i + 1..].iter_mut().zip(&self.variables[i + 1..]) {
                *value = v.lower;
            }
        }
        exact
    }
    fn script(&self, indices: &HashMap<&str, usize>) -> String {
        let mut s = String::from("(set-logic QF_LIA)\n");
        for (i, v) in self.variables.iter().enumerate() {
            writeln!(
                s,
                "(declare-const v{i} Int)\n(assert (<= {} v{i}))\n(assert (<= v{i} {}))",
                smt_int(v.lower),
                smt_int(v.upper)
            )
            .unwrap();
        }
        for p in &self.premises {
            writeln!(s, "(assert {})", p.smt(indices)).unwrap();
        }
        writeln!(
            s,
            "(assert (not {}))\n(check-sat)\n(exit)",
            self.conclusion.smt(indices)
        )
        .unwrap();
        s
    }
}
fn smt_int(n: i64) -> String {
    if n < 0 {
        format!("(- {})", n.unsigned_abs())
    } else {
        n.to_string()
    }
}
impl Predicate {
    fn holds(&self, indices: &HashMap<&str, usize>, values: &[i64]) -> bool {
        let v = self
            .terms
            .iter()
            .map(|t| i128::from(t.coeff) * i128::from(values[indices[t.var.as_str()]]))
            .sum::<i128>();
        let c = i128::from(self.constant);
        match self.relation {
            Relation::Le => v <= c,
            Relation::Lt => v < c,
            Relation::Eq => v == c,
            Relation::Ge => v >= c,
            Relation::Gt => v > c,
            Relation::Ne => v != c,
        }
    }
    fn smt(&self, indices: &HashMap<&str, usize>) -> String {
        let mut lhs = "0".to_owned();
        for t in &self.terms {
            lhs = format!(
                "(+ {lhs} (* {} v{}))",
                smt_int(t.coeff),
                indices[t.var.as_str()]
            );
        }
        let op = match self.relation {
            Relation::Le => "<=",
            Relation::Lt => "<",
            Relation::Eq => "=",
            Relation::Ge => ">=",
            Relation::Gt => ">",
            Relation::Ne => "distinct",
        };
        format!("({op} {lhs} {})", smt_int(self.constant))
    }
}

impl Broker {
    /// An advisory check of the supplied encoding, never a kernel proof or a trust transition.
    pub fn probe_bounded_oracle(
        &self,
        solvers: &OracleSolvers,
        query: OracleQuery,
    ) -> Result<OracleObservation> {
        let indices = query.validate()?;
        let _lease = self
            .resources
            .acquire(Lane::Fast, resources::host_pressure()?)?;
        let script = query.script(&indices);
        let exact_check = query.exact(&indices);
        let [z3, cvc5] = &solvers.0;
        let solvers = [z3.run(&script)?, cvc5.run(&script)?];
        let outcome = classify(
            solvers[0].status,
            solvers[1].status,
            exact_check.counterexample.is_some(),
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
