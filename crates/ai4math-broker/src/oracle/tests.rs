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
    assert_eq!(check.counterexample(), Some(&[("x".into(), 0)][..]));
    assert!(check.has_counterexample());
    assert!(prepared.script().contains("(* (- 1) v0)"));

    let mut value = serde_json::to_value(q).unwrap();
    value["premises"][0]["constant"] = json!(3);
    let q = OracleQuery::parse(&serde_json::to_vec(&value).unwrap()).unwrap();
    let prepared = q.prepare().unwrap();
    let check = prepared.exact_check();
    assert_eq!(check.assignments_checked(), 5);
    assert_eq!(check.premises_satisfying(), 0);
    assert_eq!(check.counterexample(), None);
    assert!(!check.has_counterexample());
}

#[test]
fn rejects_untyped_oversized_or_ambiguous_queries() {
    assert!(OracleQuery::parse(b"(check-sat)").is_err());
    assert!(OracleQuery::parse(&vec![b' '; MAX_ORACLE_QUERY_BYTES + 1]).is_err());
    let mut value = serde_json::to_value(query("eq", 0)).unwrap();
    value["variables"][0]["upper"] = json!(65_536);
    assert!(OracleQuery::parse(&serde_json::to_vec(&value).unwrap()).is_err());
    let mut value = serde_json::to_value(query("eq", 0)).unwrap();
    value["conclusion"]["terms"][0]["var"] = json!("x) (exit)");
    assert!(OracleQuery::parse(&serde_json::to_vec(&value).unwrap()).is_err());
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
fn observation_wire_and_identity_golden_are_stable() -> Result<()> {
    let query_bytes = br#"{"version":1,"variables":[{"name":"x","lower":0,"upper":0}],"premises":[],"conclusion":{"terms":[{"var":"x","coeff":1}],"relation":"eq","constant":0}}"#;
    let query = OracleQuery::parse(query_bytes)?;
    assert_eq!(
        serde_json::to_string(&query)?,
        std::str::from_utf8(query_bytes)?
    );
    let (script, exact_check) = query.prepare()?.into_parts();
    assert!(script.contains("(assert (not (= (+ 0 (* 1 v0)) 0)))"));
    let query_sha256 = query.fingerprint()?;
    assert_eq!(
        query_sha256,
        "7c7dd77974ea13d9d77e3a5f7cd23d64747a07a16b1508ba9b30d24ab6f39fb7"
    );
    let solvers = [
        SolverReport {
            engine: "z3",
            executable_sha256: "4".repeat(64),
            status: SolverStatus::Unsat,
            exit_code: None,
        },
        SolverReport {
            engine: "cvc5",
            executable_sha256: "5".repeat(64),
            status: SolverStatus::Unsat,
            exit_code: None,
        },
    ];
    let outcome = classify(SolverStatus::Unsat, SolverStatus::Unsat, false);
    let observation_sha256 = digest(&serde_json::to_vec(&(
        "ai4math:bounded-oracle:v1",
        &query_sha256,
        &solvers,
        outcome,
        &exact_check,
    ))?);
    assert_eq!(
        observation_sha256,
        "8eec91d2edf68a50bb3cfb3eac7a7f331de3abbffa25c8ff2339d8f5fc92463c"
    );
    let observation = OracleObservation {
        observation_sha256,
        query_sha256,
        outcome,
        query,
        exact_check,
        solvers,
    };
    let expected = concat!(
        r#"{"observation_sha256":""#,
        "8eec91d2edf68a50bb3cfb3eac7a7f331de3abbffa25c8ff2339d8f5fc92463c",
        r#"","query_sha256":"7c7dd77974ea13d9d77e3a5f7cd23d64747a07a16b1508ba9b30d24ab6f39fb7","outcome":"no_counterexample_within_bounds","query":"#,
        r#"{"version":1,"variables":[{"name":"x","lower":0,"upper":0}],"premises":[],"conclusion":{"terms":[{"var":"x","coeff":1}],"relation":"eq","constant":0}}"#,
        r#","exact_check":{"assignments_checked":1,"premises_satisfying":1,"counterexample":null},"solvers":[{"engine":"z3","executable_sha256":""#,
        "4444444444444444444444444444444444444444444444444444444444444444",
        r#"","status":"unsat"},{"engine":"cvc5","executable_sha256":""#,
        "5555555555555555555555555555555555555555555555555555555555555555",
        r#"","status":"unsat"}]}"#,
    );
    assert_eq!(serde_json::to_string(&observation)?, expected);
    assert_eq!(
        OracleObservationEvidence::raw_json(&observation)?,
        serde_json::to_value(&observation)?
    );
    Ok(())
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
