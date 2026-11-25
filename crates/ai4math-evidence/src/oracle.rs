use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{collections::HashMap, fmt::Write};

pub const MAX_ORACLE_QUERY_BYTES: usize = 32 * 1024;
const MAX_COMBINATIONS: u64 = 65_536;
const MAX_ABS: i64 = 1_000_000_000;

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
pub enum OracleOutcome {
    Counterexample,
    NoCounterexampleWithinBounds,
    Disagreement,
    Unavailable,
}

/// Exact finite-domain check embedded in a broker observation.
#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct ExactCheck {
    assignments_checked: u64,
    premises_satisfying: u64,
    counterexample: Option<Vec<(String, i64)>>,
}

impl ExactCheck {
    pub fn assignments_checked(&self) -> u64 {
        self.assignments_checked
    }

    pub fn premises_satisfying(&self) -> u64 {
        self.premises_satisfying
    }

    pub fn counterexample(&self) -> Option<&[(String, i64)]> {
        self.counterexample.as_deref()
    }

    pub fn has_counterexample(&self) -> bool {
        self.counterexample.is_some()
    }
}

/// Prepared solver script and exact finite-domain result.
#[derive(Debug)]
pub struct PreparedOracleQuery {
    script: String,
    exact_check: ExactCheck,
}

impl PreparedOracleQuery {
    pub fn script(&self) -> &str {
        &self.script
    }

    pub fn exact_check(&self) -> &ExactCheck {
        &self.exact_check
    }

    pub fn into_parts(self) -> (String, ExactCheck) {
        (self.script, self.exact_check)
    }
}

impl OracleQuery {
    /// Fingerprint of the exact parsed query, independent of JSON whitespace.
    pub fn fingerprint(&self) -> Result<String> {
        Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(self)?)))
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
            combinations *= (v.upper - v.lower + 1) as u64;
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

    /// Validate and prepare both the solver script and exact finite-domain check.
    pub fn prepare(&self) -> Result<PreparedOracleQuery> {
        let indices = self.validate()?;
        Ok(PreparedOracleQuery {
            script: self.script(&indices),
            exact_check: self.exact(&indices),
        })
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn query(relation: &str, constant: i64) -> OracleQuery {
        OracleQuery::parse(
            &serde_json::to_vec(&json!({
                "version":1,"variables":[{"name":"x","lower":-2,"upper":2}],
                "premises":[{"terms":[{"var":"x","coeff":1}],"relation":"ge","constant":0}],
                "conclusion":{"terms":[{"var":"x","coeff":-1}],"relation":relation,"constant":constant}
            }))
            .unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn enumerates_exact_bounds_and_detects_vacuity() {
        let q = query("lt", 0);
        let prepared = q.prepare().unwrap();
        let check = prepared.exact_check();
        assert_eq!(check.assignments_checked(), 5);
        assert_eq!(check.premises_satisfying(), 3);
        assert_eq!(check.counterexample(), Some(&[("x".to_string(), 0)][..]));
        assert!(prepared.script().contains("(* (- 1) v0)"));
    }

    #[test]
    fn rejects_untyped_oversized_and_ambiguous_queries() {
        assert!(OracleQuery::parse(b"(check-sat)").is_err());
        assert!(OracleQuery::parse(&vec![b' '; MAX_ORACLE_QUERY_BYTES + 1]).is_err());
        let mut value = serde_json::to_value(query("eq", 0)).unwrap();
        value["script"] = json!("(exit)");
        assert!(OracleQuery::parse(&serde_json::to_vec(&value).unwrap()).is_err());
        let mut value = serde_json::to_value(query("gt", 0)).unwrap();
        value["variables"] = json!([
            {"name":"x","lower":-1,"upper":0},
            {"name":"x","lower":-1,"upper":0}
        ]);
        assert!(OracleQuery::parse(&serde_json::to_vec(&value).unwrap()).is_err());
    }
}
