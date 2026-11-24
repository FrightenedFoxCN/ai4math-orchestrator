use std::{fs, process::Command};

#[test]
fn cli_rejects_raw_smt_before_opening_solvers() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let q = dir.path().join("input");
    fs::write(&q, "(check-sat)\n")?;
    let out = Command::new(env!("CARGO_BIN_EXE_ai4math"))
        .current_dir(dir.path())
        .arg("oracle-probe")
        .arg(q)
        .args(["/no-such-z3", "/no-such-cvc5"])
        .output()?;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("not SMT-LIB"));
    assert!(out.stdout.is_empty());
    Ok(())
}

#[test]
#[cfg(target_os = "macos")]
#[ignore = "requires native Seatbelt and AI4MATH_TEST_Z3 / AI4MATH_TEST_CVC5"]
fn native_oracle_cli_has_no_project_side_effects() -> anyhow::Result<()> {
    use serde_json::{Value, json};
    let dir = tempfile::tempdir()?;
    let q = dir.path().join("query.json");
    let input = json!({"version":1,"variables":[{"name":"n","lower":0,"upper":3}],"premises":[],
        "conclusion":{"terms":[{"var":"n","coeff":1}],"relation":"lt","constant":3}});
    let bytes = serde_json::to_vec(&input)?;
    fs::write(&q, &bytes)?;
    let z3 = std::env::var("AI4MATH_TEST_Z3")?;
    let cvc5 = std::env::var("AI4MATH_TEST_CVC5")?;
    for (solver, outcome) in [
        (cvc5.as_str(), "counterexample"),
        ("/usr/bin/true", "unavailable"),
    ] {
        let out = Command::new(env!("CARGO_BIN_EXE_ai4math"))
            .current_dir(dir.path())
            .arg("oracle-probe")
            .arg(&q)
            .args([z3.as_str(), solver])
            .output()?;
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let value: Value = serde_json::from_slice(&out.stdout)?;
        assert_eq!(value["evidence_kind"], "advisory_finite_integer_encoding");
        assert_eq!(value["observation"]["outcome"], outcome, "{value}");
        assert_eq!(value["observation"]["query"], input);
        assert_eq!(
            value["observation"]["exact_check"]["counterexample"],
            json!([["n", 3]])
        );
        assert_eq!(
            value["observation"]["query_sha256"].as_str().unwrap().len(),
            64
        );
        assert_eq!(fs::read(&q)?, bytes);
        assert_eq!(fs::read_dir(dir.path())?.count(), 1);
    }
    Ok(())
}
