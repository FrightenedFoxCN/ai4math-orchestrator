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
    let mut q = query("lt", 0);
    let check = q.exact(&q.validate().unwrap());
    assert_eq!(
        check,
        ExactCheck {
            assignments_checked: 5,
            premises_satisfying: 3,
            counterexample: Some(vec![("x".into(), 0)])
        }
    );
    assert!(q.script(&q.validate().unwrap()).contains("(* (- 1) v0)"));
    q.premises[0].constant = 3;
    let check = q.exact(&q.validate().unwrap());
    assert_eq!(
        check,
        ExactCheck {
            assignments_checked: 5,
            premises_satisfying: 0,
            counterexample: None
        }
    );
}

#[test]
fn rejects_untyped_oversized_or_ambiguous_queries() {
    assert!(OracleQuery::parse(b"(check-sat)").is_err());
    assert!(OracleQuery::parse(&vec![b' '; MAX_ORACLE_QUERY_BYTES + 1]).is_err());
    let mut q = query("eq", 0);
    q.variables[0].upper = 65_536;
    assert!(q.validate().is_err());
    q.variables[0].upper = 2;
    q.conclusion.terms[0].var = "x) (exit)".into();
    assert!(q.validate().is_err());
    let mut value = serde_json::to_value(query("ne", 0)).unwrap();
    value["script"] = json!("(exit)");
    assert!(OracleQuery::parse(&serde_json::to_vec(&value).unwrap()).is_err());
    let mut value = serde_json::to_value(query("gt", 0)).unwrap();
    value["variables"] =
        json!([{"name":"x","lower":-1,"upper":0},{"name":"x","lower":-1,"upper":0}]);
    assert!(OracleQuery::parse(&serde_json::to_vec(&value).unwrap()).is_err());
}

#[test]
fn reports_disagreement_or_unavailability_without_picking_a_solver() {
    use SolverStatus::*;
    assert_eq!(parse_status("sat\n(extra log)\n"), InvalidOutput);
    assert_eq!(classify(Sat, Unsat, true), OracleOutcome::Disagreement);
    assert_eq!(classify(Sat, Sat, false), OracleOutcome::Disagreement);
    assert_eq!(classify(Unsat, Unsat, true), OracleOutcome::Disagreement);
    assert_eq!(classify(Sat, Unknown, true), OracleOutcome::Unavailable);
    assert_eq!(classify(Failed, Unsat, false), OracleOutcome::Unavailable);
    assert_eq!(
        classify(InvalidOutput, Sat, true),
        OracleOutcome::Unavailable
    );
}

#[test]
#[cfg(target_os = "macos")]
#[ignore = "requires native Seatbelt and AI4MATH_TEST_Z3 / AI4MATH_TEST_CVC5"]
fn native_bounded_oracle_corroborates_exact_results() -> Result<()> {
    let z3 = PathBuf::from(std::env::var_os("AI4MATH_TEST_Z3").context("set native z3")?);
    let cvc5 = PathBuf::from(std::env::var_os("AI4MATH_TEST_CVC5").context("set native cvc5")?);
    let solvers = OracleSolvers::new(&z3, &cvc5)?;
    assert!(OracleSolvers::new(&z3, &z3).is_err());
    let broker = Broker::new();
    for (relation, outcome) in [
        ("lt", OracleOutcome::Counterexample),
        ("le", OracleOutcome::NoCounterexampleWithinBounds),
    ] {
        let observation = broker.probe_bounded_oracle(&solvers, query(relation, 0))?;
        assert_eq!(observation.outcome(), outcome, "{observation:?}");
        assert_eq!(
            observation.observation_sha256,
            broker
                .probe_bounded_oracle(&solvers, query(relation, 0))?
                .observation_sha256
        );
    }
    Ok(())
}
