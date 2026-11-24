#![cfg(target_os = "macos")]

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

const IDENTITY: &str = "nativeExportIdentity";
const USES_AXIOM: &str = "nativeExportUsesAxiom";
const FORBIDDEN: &str = "nativeExportForbidden";
const REJECTED: &str = "Nanoda rejected the Lean export or could not complete the sandboxed check";
const SOURCE: &str = "prelude

theorem nativeExportIdentity {p : Prop} : p → p := fun h => h

axiom nativeExportForbidden {p : Prop} : p
theorem nativeExportUsesAxiom {p : Prop} : p := nativeExportForbidden
";

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn successful(output: Output, step: &str) -> Result<Vec<u8>> {
    ensure!(
        output.status.success(),
        "{step} failed ({}):\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(output.stdout)
}

fn export(leanexport: &Path, dir: &Path, theorem: &str) -> Result<Vec<u8>> {
    // leanexport invokes lean by name to locate its library; never pick up an elan shim.
    successful(
        Command::new(leanexport)
            .env_clear()
            .env("PATH", leanexport.parent().context("leanexport parent")?)
            .env("LEAN_PATH", dir)
            .current_dir(dir)
            .args(["Fixture", "--", theorem])
            .output()
            .context("spawn bundled leanexport")?,
        theorem,
    )
}

fn check(export: &Path, export_pin: &str, checker: &Path, checker_pin: &str) -> Result<Output> {
    Command::new(env!("CARGO_BIN_EXE_ai4math"))
        .current_dir(export.parent().context("export parent")?)
        .arg("t2-check-export")
        .arg(export)
        .arg(export_pin)
        .arg(checker)
        .arg(checker_pin)
        .output()
        .context("spawn ai4math t2-check-export")
}

fn rejected(output: Output, case: &str, diagnostic: &str) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{case}: unexpectedly accepted");
    assert!(
        output.stdout.is_empty(),
        "{case}: emitted stdout on failure"
    );
    assert!(
        stderr.contains(diagnostic),
        "{case}: expected {diagnostic:?}, got {stderr:?}"
    );
}

fn wrong_pin(pin: &str) -> String {
    format!(
        "{}{}",
        if pin.starts_with('0') { '1' } else { '0' },
        &pin[1..]
    )
}

fn mismatch_proof(export: &[u8]) -> Result<Vec<u8>> {
    let mut changed = false;
    let mut invalid = Vec::with_capacity(export.len());
    for line in export.split_inclusive(|byte| *byte == b'\n') {
        let mut row: Value = serde_json::from_slice(line)?;
        if let Some(theorem) = row.get_mut("thm") {
            ensure!(!changed, "expected a single exported theorem");
            let ty = theorem["type"].as_u64().context("theorem type index")?;
            let value = theorem["value"].as_u64().context("theorem proof index")?;
            ensure!(ty != value, "expected a separate theorem proof expression");
            // A well-formed reference to the proposition itself is not a proof of it.
            // Leave all other rows (including the proposition and expression table) alone.
            theorem["value"] = json!(ty);
            serde_json::to_writer(&mut invalid, &row)?;
            invalid.push(b'\n');
            changed = true;
        } else {
            invalid.extend_from_slice(line);
        }
    }
    ensure!(changed, "no theorem to mutate in selected export");
    Ok(invalid)
}

#[test]
#[ignore = "requires macOS Seatbelt and the bundled v4.35.0 lean, leanexport, and nanoda_bin"]
fn native_t2_check_export_accepts_only_a_valid_pinned_export() -> Result<()> {
    let lean = PathBuf::from(
        std::env::var_os("AI4MATH_TEST_LEAN").context("set AI4MATH_TEST_LEAN to bundled Lean")?,
    );
    ensure!(lean.is_absolute(), "AI4MATH_TEST_LEAN must be absolute");
    let bin = lean.parent().context("bundled Lean bin directory")?;
    let leanexport = bin.join("leanexport");
    let checker = bin.join("nanoda_bin");
    let checker_pin = sha256(&fs::read(&checker).context("read bundled nanoda_bin")?);

    let temp = tempfile::tempdir()?;
    let dir = temp.path().canonicalize()?;
    fs::write(dir.join("Fixture.lean"), SOURCE)?;
    successful(
        Command::new(&lean)
            .env_clear()
            .env("PATH", bin)
            .current_dir(&dir)
            .args(["-o", "Fixture.olean", "Fixture.lean"])
            .output()
            .context("spawn bundled Lean")?,
        "compile Fixture.lean",
    )?;

    let valid = export(&leanexport, &dir, IDENTITY)?;
    let meta: Value = serde_json::from_slice(valid.split(|byte| *byte == b'\n').next().unwrap())?;
    assert_eq!(meta["meta"]["lean"]["version"], "4.35.0");
    let lean_commit = meta["meta"]["lean"]["githash"]
        .as_str()
        .context("Lean commit in export header")?;
    let export_pin = sha256(&valid);
    let valid_path = dir.join("valid.ndjson");
    fs::write(&valid_path, &valid)?;
    let report: Value = serde_json::from_slice(&successful(
        check(&valid_path, &export_pin, &checker, &checker_pin)?,
        "check genuine Lean export",
    )?)?;
    assert_eq!(
        report,
        json!({
            "schema": "ai4math.t2.lean-export-check.v1",
            "advisory_only": true,
            "checker": "nanoda",
            "accepted": true,
            "export_sha256": export_pin,
            "checker_sha256": checker_pin,
            "export_format": "3.1.0",
            "lean_git_commit": lean_commit,
            "theorem_count": 1,
        })
    );

    let invalid = mismatch_proof(&valid)?;
    let invalid_path = dir.join("mismatched-proof.ndjson");
    fs::write(&invalid_path, &invalid)?;
    rejected(
        check(&invalid_path, &sha256(&invalid), &checker, &checker_pin)?,
        "pinned export with mismatched theorem proof",
        REJECTED,
    );

    let axiomatic = export(&leanexport, &dir, USES_AXIOM)?;
    let rows: Vec<Value> = String::from_utf8_lossy(&axiomatic)
        .lines()
        .map(serde_json::from_str)
        .collect::<serde_json::Result<_>>()?;
    let axiom_name = rows
        .iter()
        .find(|row| row["str"]["pre"] == 0 && row["str"]["str"] == FORBIDDEN)
        .and_then(|row| row["in"].as_u64())
        .context("forbidden axiom name in selected export")?;
    assert!(rows.iter().any(|row| row["axiom"]["name"] == axiom_name));
    let axiom_path = dir.join("axiomatic.ndjson");
    fs::write(&axiom_path, &axiomatic)?;
    rejected(
        check(&axiom_path, &sha256(&axiomatic), &checker, &checker_pin)?,
        "pinned export using a disallowed axiom",
        REJECTED,
    );

    rejected(
        check(&valid_path, &wrong_pin(&export_pin), &checker, &checker_pin)?,
        "mismatched export pin",
        "Lean export SHA-256 mismatch",
    );
    rejected(
        check(&valid_path, &export_pin, &checker, &wrong_pin(&checker_pin))?,
        "mismatched checker pin",
        "Nanoda checker SHA-256 mismatch",
    );
    Ok(())
}
