//! Advisory Oracle evidence. Neither approval context nor observations confer trust.
use super::{Change, StateStore, event};
use crate::retained::valid_id;
use ai4math_core::{Engine, validate_source_path};
use ai4math_evidence::{OracleObservationEvidence, OracleQuery};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, path::Path};

pub(super) const RECORDED: &str = "ORACLE_OBSERVATION_RECORDED";
const MAX_REPO: usize = 4096;
const MAX_CLAIM_ID: usize = 256;
const MAX_MODULE: usize = 256;
const MAX_DECL: usize = 256;
const MAX_TARGET: usize = MAX_MODULE + 1 + MAX_DECL;
const MAX_STATEMENT: usize = 16 * 1024;
const MAX_OBSERVATION: usize = 128 * 1024;

/// Exact ledger context to review with the query and solver binaries. The hash
/// binds what was presented; a value of this type alone does not prove review.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OracleApproval {
    pub approval_sha256: String,
    pub claim_id: String,
    pub commit: String,
    pub repo: String,
    pub engine: String,
    pub formal_target: String,
    pub informal_statement: String,
    pub query_sha256: String,
    /// Canonical order: Z3, then cvc5.
    pub solver_sha256s: [String; 2],
}

/// Durable readback only; JSON cannot be used as a sealed broker result.
#[derive(Debug, Serialize)]
pub struct RetainedOracleObservation {
    pub id: String,
    pub approval: OracleApproval,
    pub observation: Value,
    pub event_seq: i64,
}

fn digest(value: impl Serialize) -> Result<String> {
    Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(&value)?)))
}

impl OracleApproval {
    fn digest(&self) -> Result<String> {
        // Tuple order and domain are a versioned wire format, not dependent on
        // object key order or on the serialization of the approval itself.
        digest((
            "ai4math:oracle-approval:v1",
            &self.claim_id,
            &self.commit,
            &self.repo,
            &self.engine,
            &self.formal_target,
            &self.informal_statement,
            &self.query_sha256,
            &self.solver_sha256s,
        ))
    }
}

fn record_id(approval_sha256: &str, observation_sha256: &str) -> Result<String> {
    digest((
        "ai4math:retained-oracle-observation:v1",
        approval_sha256,
        observation_sha256,
    ))
}

fn canonical_target(engine: &str, target: &str, module: &str) -> bool {
    let Ok(engine) = engine.parse::<Engine>() else {
        return false;
    };
    target.split_once('#').is_some_and(|(file, decl)| {
        file == module
            && validate_source_path(engine, file).is_ok()
            && decl.len() <= MAX_DECL
            && decl.split('.').all(|part| !part.is_empty())
            && !decl
                .chars()
                .any(|c| c.is_control() || c.is_whitespace() || matches!(c, '#' | '/' | '\\' | ':'))
    })
}

pub(super) fn read_approval(
    db: &Connection,
    repo: &str,
    claim_id: &str,
    commit: &str,
    query_sha256: &str,
    solver_sha256s: [&str; 2],
) -> Result<OracleApproval> {
    ensure!(
        repo.len() <= MAX_REPO
            && Path::new(repo).is_absolute()
            && !repo.chars().any(char::is_control),
        "expected a bounded absolute managed repository path"
    );
    ensure!(
        !claim_id.is_empty()
            && claim_id.len() <= MAX_CLAIM_ID
            && claim_id == claim_id.trim()
            && !claim_id.chars().any(char::is_control),
        "invalid or oversized Oracle Claim ID"
    );
    ensure!(
        (commit.len() == 40
            && commit
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
            || valid_id(commit),
        "expected a full lowercase Git commit"
    );
    ensure!(
        valid_id(query_sha256) && solver_sha256s.iter().all(|s| valid_id(s)),
        "expected lowercase SHA-256 for the Oracle query and solver binaries"
    );

    // Use one SQLite snapshot even outside the write transaction, and bound
    // database-supplied strings by bytes before copying them out of SQLite.
    let (engine, formal_target, module, informal_statement): (String, String, String, String) = db
        .query_row(
            "SELECT c.engine,c.formal_target,c.module_path,c.informal_statement
             FROM claims c JOIN managed_repo r ON r.singleton=1 AND r.repo=?1
             WHERE c.claim_id=?2
               AND NOT EXISTS(SELECT 1 FROM promotion_intent)
               AND NOT EXISTS(SELECT 1 FROM pivot_resume_intent)
               AND length(CAST(c.formal_target AS BLOB)) BETWEEN 1 AND ?3
               AND length(CAST(c.module_path AS BLOB)) BETWEEN 1 AND ?4
               AND length(CAST(c.informal_statement AS BLOB)) BETWEEN 1 AND ?5",
            params![repo, claim_id, MAX_TARGET, MAX_MODULE, MAX_STATEMENT],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?
        .context("Oracle approval requires this exact bound repository, a bounded formal Claim, and no pending promotion or Pivot")?;
    ensure!(
        canonical_target(&engine, &formal_target, &module),
        "expected a canonical Claim target module#declaration matching its stored module and engine"
    );
    ensure!(
        !informal_statement.trim().is_empty() && !informal_statement.contains('\0'),
        "invalid Claim informal statement for Oracle approval"
    );
    let mut approval = OracleApproval {
        approval_sha256: String::new(),
        claim_id: claim_id.into(),
        commit: commit.into(),
        repo: repo.into(),
        engine,
        formal_target,
        informal_statement,
        query_sha256: query_sha256.into(),
        solver_sha256s: solver_sha256s.map(str::to_owned),
    };
    approval.approval_sha256 = approval.digest()?;
    Ok(approval)
}

pub(super) fn read_observation(
    db: &Connection,
    id: &str,
) -> Result<Option<RetainedOracleObservation>> {
    let row = db.query_row(
        "SELECT approval_sha256,claim_id,git_commit,repo,engine,formal_target,informal_statement,
                query_sha256,z3_sha256,cvc5_sha256,observation_sha256,observation_json,event_seq
         FROM oracle_observations WHERE id=?1",
        [id],
        |r| Ok((
            OracleApproval {
                approval_sha256: r.get(0)?, claim_id: r.get(1)?, commit: r.get(2)?, repo: r.get(3)?,
                engine: r.get(4)?, formal_target: r.get(5)?, informal_statement: r.get(6)?,
                query_sha256: r.get(7)?, solver_sha256s: [r.get(8)?, r.get(9)?],
            },
            r.get::<_, String>(10)?, r.get::<_, String>(11)?, r.get(12)?,
        )),
    ).optional()?;
    let Some((approval, observation_sha256, observation, event_seq)) = row else {
        return Ok(None);
    };
    ensure!(
        approval.digest()? == approval.approval_sha256
            && record_id(&approval.approval_sha256, &observation_sha256)? == id,
        "retained Oracle observation identity mismatch"
    );
    let observation: Value =
        serde_json::from_str(&observation).context("decode retained Oracle observation")?;
    ensure!(
        observation
            .get("observation_sha256")
            .and_then(Value::as_str)
            == Some(&observation_sha256),
        "retained Oracle observation fingerprint mismatch"
    );
    Ok(Some(RetainedOracleObservation {
        id: id.into(),
        approval,
        observation,
        event_seq,
    }))
}

// This is a read-only v1 wire shape, never deserialized into the broker's
// sealed OracleObservation. Field order in the hashed substructures matters.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CounterexampleEvidenceV1 {
    observation_sha256: String,
    query_sha256: String,
    outcome: String,
    query: Value,
    exact_check: ExactWitnessV1,
    solvers: [SatSolverV1; 2],
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ExactWitnessV1 {
    assignments_checked: u64,
    premises_satisfying: u64,
    counterexample: Vec<(String, i64)>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SatSolverV1 {
    engine: String,
    executable_sha256: String,
    status: String,
}

pub(super) fn require_corroborated_counterexample(
    retained: &RetainedOracleObservation,
) -> Result<()> {
    ensure!(
        retained.observation.get("outcome").and_then(Value::as_str) == Some("counterexample"),
        "Oracle task hold requires a corroborated counterexample"
    );
    let body: CounterexampleEvidenceV1 = serde_json::from_value(retained.observation.clone())
        .context("decode retained corroborated counterexample evidence")?;
    let query = OracleQuery::parse(&serde_json::to_vec(&body.query)?)?;
    let approval = &retained.approval;
    ensure!(
        body.query_sha256 == approval.query_sha256 && query.fingerprint()? == body.query_sha256,
        "retained counterexample does not match the approved query"
    );
    ensure!(
        approval.solver_sha256s[0] != approval.solver_sha256s[1]
            && body
                .solvers
                .iter()
                .zip(["z3", "cvc5"])
                .zip(&approval.solver_sha256s)
                .all(|((solver, engine), sha)| solver.status == "sat"
                    && solver.engine == engine
                    && solver.executable_sha256 == *sha),
        "Oracle task hold requires SAT from both approved solvers"
    );
    ensure!(
        digest((
            "ai4math:bounded-oracle:v1",
            &body.query_sha256,
            &body.solvers,
            &body.outcome,
            &body.exact_check
        ))? == body.observation_sha256,
        "retained counterexample evidence fingerprint mismatch"
    );
    ensure!(
        exact_witness_matches(&body.query, &body.exact_check) == Some(true),
        "retained exact witness does not represent a bounded counterexample"
    );
    Ok(())
}

// The broker parser above checks shape, domain size, and coefficient bounds.
// Recheck just the stored assignment, not the entire bounded search or the solvers.
fn exact_witness_matches(query: &Value, exact: &ExactWitnessV1) -> Option<bool> {
    let vars = query.get("variables")?.as_array()?;
    if vars.len() != exact.counterexample.len() {
        return Some(false);
    }
    let mut values = BTreeMap::new();
    let mut assignments = 1u64;
    for (var, (name, value)) in vars.iter().zip(&exact.counterexample) {
        let lower = var.get("lower")?.as_i64()?;
        let upper = var.get("upper")?.as_i64()?;
        if var.get("name")?.as_str()? != name || !(lower..=upper).contains(value) {
            return Some(false);
        }
        values.insert(name.as_str(), *value);
        assignments *= (upper - lower + 1) as u64;
    }
    if exact.assignments_checked != assignments
        || !(1..=assignments).contains(&exact.premises_satisfying)
    {
        return Some(false);
    }
    let holds = |pred: &Value| -> Option<bool> {
        let lhs = pred
            .get("terms")?
            .as_array()?
            .iter()
            .try_fold(0i128, |acc, term| {
                let value = values.get(term.get("var")?.as_str()?)?;
                let coeff = term.get("coeff")?.as_i64()?;
                Some(acc + i128::from(*value) * i128::from(coeff))
            })?;
        let rhs = i128::from(pred.get("constant")?.as_i64()?);
        Some(match pred.get("relation")?.as_str()? {
            "le" => lhs <= rhs,
            "lt" => lhs < rhs,
            "eq" => lhs == rhs,
            "ge" => lhs >= rhs,
            "gt" => lhs > rhs,
            "ne" => lhs != rhs,
            _ => return None,
        })
    };
    for premise in query.get("premises")?.as_array()? {
        if !holds(premise)? {
            return Some(false);
        }
    }
    Some(!holds(query.get("conclusion")?)?)
}

impl StateStore {
    /// Read the current ledger context. The coordinator establishes the real
    /// Git snapshot under its repository lock and presents the query for review.
    pub fn oracle_approval(
        &self,
        repo: &str,
        claim_id: &str,
        commit: &str,
        query_sha256: &str,
        solver_sha256s: [&str; 2],
    ) -> Result<OracleApproval> {
        read_approval(
            &self.db,
            repo,
            claim_id,
            commit,
            query_sha256,
            solver_sha256s,
        )
    }

    /// Retain a sealed Oracle result as advisory evidence only. No outcome
    /// updates Claim trust, workflow state, alerts, or the Wiki.
    pub fn record_oracle_observation(
        &mut self,
        approval: &OracleApproval,
        observation: &dyn OracleObservationEvidence,
    ) -> Result<RetainedOracleObservation> {
        let solver_sha256s = observation.solver_sha256s();
        ensure!(
            approval.query_sha256 == observation.query_sha256()
                && approval.solver_sha256s.each_ref().map(String::as_str) == solver_sha256s,
            "Oracle observation does not match the approved query and solver hashes"
        );
        ensure!(
            valid_id(&approval.approval_sha256),
            "invalid Oracle approval ID"
        );
        ensure!(valid_id(observation.id()), "invalid broker observation ID");
        let id = record_id(&approval.approval_sha256, observation.id())?;
        let body = observation.raw_json()?;
        let encoded = serde_json::to_string(&body)?;
        ensure!(
            encoded.len() <= MAX_OBSERVATION,
            "Oracle observation exceeds the retention budget"
        );
        self.write(|tx| {
            let live = read_approval(
                tx, &approval.repo, &approval.claim_id, &approval.commit,
                observation.query_sha256(), solver_sha256s,
            )?;
            ensure!(live == *approval, "Oracle approval no longer matches the live Claim context");
            if let Some(previous) = read_observation(tx, &id)? {
                ensure!(
                    previous.approval == live && previous.observation == body,
                    "conflicting Oracle observation replay"
                );
                return Ok(previous);
            }

            let event_seq = event(
                tx,
                Change { actor: "broker", git_commit: Some(&live.commit) },
                RECORDED,
                "claim",
                &live.claim_id,
                json!({
                    "id": id,
                    "approval_sha256": live.approval_sha256,
                    "observation_sha256": observation.id(),
                    "query_sha256": live.query_sha256,
                    "outcome": observation.outcome(),
                }),
            )?;
            tx.execute(
                "INSERT INTO oracle_observations (
                    id,approval_sha256,claim_id,git_commit,repo,engine,formal_target,informal_statement,
                    query_sha256,z3_sha256,cvc5_sha256,observation_sha256,observation_json,event_seq
                 ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
                params![
                    id, live.approval_sha256, live.claim_id, live.commit, live.repo, live.engine,
                    live.formal_target, live.informal_statement, live.query_sha256,
                    live.solver_sha256s[0], live.solver_sha256s[1], observation.id(), encoded, event_seq,
                ],
            )?;
            Ok(RetainedOracleObservation { id, approval: live, observation: body, event_seq })
        })
    }

    /// Read historical evidence, even if the Claim changed. Legacy read-only
    /// ledgers may lack the additive table; this never initializes the schema.
    pub fn oracle_observation(&self, id: &str) -> Result<Option<RetainedOracleObservation>> {
        ensure!(valid_id(id), "invalid retained Oracle observation ID");
        match read_observation(&self.db, id) {
            Err(_) if !self.db.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='oracle_observations')",
                [], |r| r.get::<_, bool>(0),
            )? => Ok(None),
            result => result,
        }
    }
}

#[cfg(test)]
mod hold_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NewClaim;

    const REPO: &str = "/managed/project";
    const CLAIM: &str = "claim:gap";
    const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
    const QUERY: &str = "1111111111111111111111111111111111111111111111111111111111111111";
    const Z3: &str = "2222222222222222222222222222222222222222222222222222222222222222";
    const CVC5: &str = "3333333333333333333333333333333333333333333333333333333333333333";
    const CHANGE: Change<'_> = Change {
        actor: "test",
        git_commit: Some(COMMIT),
    };

    fn claim(store: &mut StateStore, id: &str) -> Result<()> {
        store.create_claim(
            NewClaim {
                id,
                title: "Gap",
                informal_statement: "The gap is positive.\nAdditional context is preserved.",
                engine: "lean4",
                formal_target: Some("Proofs/Gap.lean#Gap.positive"),
                module_path: Some("Proofs/Gap.lean"),
                critical: true,
            },
            CHANGE,
        )
    }

    fn approval(store: &StateStore) -> Result<OracleApproval> {
        store.oracle_approval(REPO, CLAIM, COMMIT, QUERY, [Z3, CVC5])
    }

    #[test]
    fn approval_is_read_only_and_binds_every_field() -> Result<()> {
        let mut store = StateStore::open(":memory:")?;
        claim(&mut store, CLAIM)?;
        assert!(approval(&store).is_err());
        store.bind_task_repository(REPO, COMMIT)?;
        let seq = store.status()?.last_event_seq;
        let original = approval(&store)?;
        assert!(valid_id(&original.approval_sha256));
        assert_eq!(approval(&store)?, original);
        assert_eq!(store.status()?.last_event_seq, seq);
        assert!(
            store
                .oracle_approval("/managed/project/", CLAIM, COMMIT, QUERY, [Z3, CVC5])
                .is_err()
        );
        assert!(store.oracle_approval(REPO, CLAIM, COMMIT, QUERY, [CVC5, Z3])? != original);

        let mutations: [fn(&mut OracleApproval); 8] = [
            |a| a.claim_id.push('2'),
            |a| a.commit.replace_range(0..1, "a"),
            |a| a.repo.push('2'),
            |a| a.engine = "agda".into(),
            |a| a.formal_target.push('2'),
            |a| a.informal_statement.push('\n'),
            |a| a.query_sha256 = Z3.into(),
            |a| a.solver_sha256s.swap(0, 1),
        ];
        for mutate in mutations {
            let mut changed = original.clone();
            mutate(&mut changed);
            assert_ne!(changed.digest()?, original.approval_sha256);
        }
        let mut changed = original.clone();
        changed.approval_sha256.clear();
        assert_eq!(changed.digest()?, original.approval_sha256);
        assert_ne!(
            record_id(&original.approval_sha256, Z3)?,
            record_id(&original.approval_sha256, CVC5)?
        );
        assert_ne!(
            record_id(&original.approval_sha256, Z3)?,
            record_id(QUERY, Z3)?
        );
        Ok(())
    }

    #[test]
    fn approval_rejects_invalid_context_and_pending_git_work() -> Result<()> {
        let mut store = StateStore::open(":memory:")?;
        store.bind_task_repository(REPO, COMMIT)?;
        claim(&mut store, CLAIM)?;
        assert!(
            store
                .oracle_approval(REPO, CLAIM, &COMMIT.to_uppercase(), QUERY, [Z3, CVC5])
                .is_err()
        );
        assert!(
            store
                .oracle_approval(REPO, CLAIM, COMMIT, "short", [Z3, CVC5])
                .is_err()
        );
        assert!(
            store
                .oracle_approval(REPO, CLAIM, COMMIT, QUERY, [&"A".repeat(64), CVC5])
                .is_err()
        );
        let seq = store.status()?.last_event_seq;
        let approved = approval(&store)?;
        for (target, module) in [
            (None, Some("Proofs/Gap.lean")),
            (Some("Proofs/Gap.lean#Gap.positive"), None),
            (
                Some("Proofs/Gap.lean#Gap.positive"),
                Some("Proofs/gap.lean"),
            ),
            (Some("Proofs/Gap.lean#"), Some("Proofs/Gap.lean")),
            (
                Some("Proofs/Gap.lean#Gap.positive#extra"),
                Some("Proofs/Gap.lean"),
            ),
            (
                Some("Proofs/Gap.lean#Gap ..positive"),
                Some("Proofs/Gap.lean"),
            ),
            (Some("../Gap.lean#positive"), Some("../Gap.lean")),
            (Some("Gap.agda#positive"), Some("Gap.agda")),
        ] {
            store.db.execute(
                "UPDATE claims SET formal_target=?1,module_path=?2 WHERE claim_id=?3",
                params![target, module, CLAIM],
            )?;
            assert!(
                approval(&store).is_err(),
                "accepted {target:?} and {module:?}"
            );
        }
        store.db.execute("UPDATE claims SET formal_target=?1,module_path='Proofs/Gap.lean',informal_statement=?2 WHERE claim_id=?3",
            params![approved.formal_target,"é".repeat(MAX_STATEMENT / 2 + 1),CLAIM])?;
        assert!(approval(&store).is_err());
        store.db.execute(
            "UPDATE claims SET informal_statement='Statement changed' WHERE claim_id=?1",
            [CLAIM],
        )?;
        assert_ne!(approval(&store)?.approval_sha256, approved.approval_sha256);
        store.db.execute("INSERT INTO promotion_intent VALUES(1,?1,?2,?3,?3,'Proofs/Gap.lean','positive','test')", params![REPO,CLAIM,COMMIT])?;
        assert!(approval(&store).is_err());
        store.db.execute("DELETE FROM promotion_intent", [])?;
        store.db.execute(
            "INSERT INTO pivot_resume_intent VALUES(1,'ADR-test','{}',?1)",
            [COMMIT],
        )?;
        assert!(approval(&store).is_err());
        assert_eq!(store.status()?.last_event_seq, seq);
        Ok(())
    }

    #[test]
    fn only_sealed_api_can_emit_oracle_event() -> Result<()> {
        let mut store = StateStore::open(":memory:")?;
        assert!(
            store
                .append_event(CHANGE, RECORDED, "claim", CLAIM, json!({}))
                .is_err()
        );
        assert_eq!(store.status()?.last_event_seq, 0);
        Ok(())
    }

    #[test]
    fn historical_readback_is_immutable_and_legacy_read_is_empty() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("state.sqlite");
        let mut store = StateStore::open(&path)?;
        store.bind_task_repository(REPO, COMMIT)?;
        claim(&mut store, CLAIM)?;
        let reviewed = approval(&store)?;
        let seq = store.status()?.last_event_seq;
        let id = record_id(&reviewed.approval_sha256, QUERY)?;
        let observation = json!({"observation_sha256": QUERY, "outcome": "unavailable"});
        assert!(store.oracle_observation(&id)?.is_none());
        // Direct SQLite fixture tests schema/readback independently of the
        // production append path. Native CLI tests use a real sealed result.
        store.db.execute(
            "INSERT INTO oracle_observations (
                id,approval_sha256,claim_id,git_commit,repo,engine,formal_target,informal_statement,
                query_sha256,z3_sha256,cvc5_sha256,observation_sha256,observation_json,event_seq
             ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?9,?12,?13)",
            params![
                id,
                reviewed.approval_sha256,
                CLAIM,
                COMMIT,
                REPO,
                reviewed.engine,
                reviewed.formal_target,
                reviewed.informal_statement,
                QUERY,
                Z3,
                CVC5,
                observation.to_string(),
                seq
            ],
        )?;
        for sql in [
            "UPDATE oracle_observations SET observation_json='{}'",
            "DELETE FROM oracle_observations",
            "INSERT OR REPLACE INTO oracle_observations SELECT * FROM oracle_observations",
            "INSERT OR REPLACE INTO oracle_observations
             SELECT printf('%064x',3),approval_sha256,claim_id,git_commit,repo,engine,formal_target,
                 informal_statement,query_sha256,z3_sha256,cvc5_sha256,observation_sha256,
                 observation_json,event_seq-1 FROM oracle_observations",
        ] {
            let error = store.db.execute(sql, []).unwrap_err();
            assert!(error.to_string().contains("immutable"), "{error}");
        }
        store.db.execute(
            "UPDATE claims SET informal_statement='Different now' WHERE claim_id=?1",
            [CLAIM],
        )?;
        let read_store = StateStore::open_readonly(&path)?;
        assert_ne!(approval(&read_store)?, reviewed);
        let retained = read_store.oracle_observation(&id)?.unwrap();
        assert_eq!(retained.id, id);
        assert_eq!(retained.approval, reviewed);
        assert_eq!(retained.observation, observation);
        assert_eq!(retained.event_seq, seq);
        drop(read_store);
        store.db.execute("DROP TABLE oracle_observations", [])?;
        let read_store = StateStore::open_readonly(&path)?;
        assert!(read_store.oracle_observation(&id)?.is_none());
        assert!(read_store.oracle_observation("not-an-id").is_err());
        Ok(())
    }
}
