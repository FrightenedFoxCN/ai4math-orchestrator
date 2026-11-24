#![cfg(target_os = "macos")]

use ai4math_store::{Change, NewClaim, StateStore, Trust};
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

const ANCHOR: &str = "ZAuditFixtureBasis.lean";
const DOWNSTREAM: &str = "AAuditFixtureDependent.lean";
const BASIS: &str = "def auditFixtureValue : Nat := 2
";
const VALID: &str = "import ZAuditFixtureBasis
theorem auditFixtureProof : auditFixtureValue = 2 := rfl
";
const BROKEN: &str = "import ZAuditFixtureBasis
theorem auditFixtureProof : auditFixtureValue = 2 := auditFixtureMissingProof
";
const CLAIM: &str = "native-audit-oracle";
const STATEMENT: &str = "Private fixture statement not needed by the audit.";
const STATE: &str = ".runtime/state.sqlite";
const WAL: &str = ".runtime/state.sqlite-wal";

fn checked(output: Output) -> Result<Vec<u8>> {
    ensure!(
        output.status.success(),
        "command failed ({}): {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(output.stdout)
}

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("/usr/bin/git")
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .current_dir(dir)
        .args([
            "-c",
            "user.name=T2 Audit Fixture",
            "-c",
            "user.email=t2-audit@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()?;
    Ok(String::from_utf8(checked(out)?)?.trim().into())
}

fn ai(root: &Path, command: &str) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_ai4math"));
    cmd.arg(command).arg(root);
    cmd
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn file_sha256(path: &Path) -> Result<String> {
    let mut digest = Sha256::new();
    std::io::copy(&mut fs::File::open(path)?, &mut digest)?;
    Ok(format!("{:x}", digest.finalize()))
}

// Include repository files outside refs as well: new objects, reflogs, retained artifacts,
// or an index would be a durable change even if the reviewed ref did not move.
fn repo_snapshot(repo: &Path) -> Result<BTreeMap<PathBuf, Option<String>>> {
    let mut snapshot = BTreeMap::new();
    let mut dirs = vec![repo.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            let kind = entry.file_type()?;
            let digest = if kind.is_dir() {
                dirs.push(path.clone());
                None
            } else {
                ensure!(kind.is_file(), "unexpected Git repository entry: {path:?}");
                Some(file_sha256(&path)?)
            };
            snapshot.insert(path.strip_prefix(repo)?.to_path_buf(), digest);
        }
    }
    Ok(snapshot)
}

#[test]
#[ignore = "requires operator-approved native macOS Seatbelt and absolute AI4MATH_TEST_LEAN"]
fn native_t2_audit_joins_one_reviewed_build_and_ledger_without_authority() -> Result<()> {
    let lean = PathBuf::from(
        std::env::var_os("AI4MATH_TEST_LEAN").context("set AI4MATH_TEST_LEAN to native Lean")?,
    );
    ensure!(lean.is_absolute(), "expected an absolute native Lean path");
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    let source = root.join("source");
    let repo = root.join("managed.git");
    fs::create_dir(&source)?;
    git(&source, &["init", "--quiet", "--initial-branch=main"])?;
    fs::write(source.join(ANCHOR), BASIS)?;
    fs::write(source.join(DOWNSTREAM), VALID)?;
    // Deliberately not lexical order: the downstream module needs the anchor's compiled object.
    fs::write(
        source.join("ai4math-modules.txt"),
        format!(
            "{ANCHOR}
{DOWNSTREAM}
"
        ),
    )?;
    git(
        &source,
        &["add", "--", ANCHOR, DOWNSTREAM, "ai4math-modules.txt"],
    )?;
    git(
        &source,
        &["commit", "--quiet", "-m", "reviewed Lean fixture"],
    )?;
    git(
        &root,
        &["clone", "--bare", "--quiet", "source", "managed.git"],
    )?;
    let head = git(&repo, &["rev-parse", "--verify", "refs/heads/main"])?;
    checked(ai(&root, "init").arg("lean4").output()?)?;
    checked(
        ai(&root, "task-bind")
            .arg(&repo)
            .args([ANCHOR, &head])
            .output()?,
    )?;

    // Keep the real store connection open so the WAL bytes can also be compared.
    let mut store = StateStore::open(root.join(STATE))?;
    let change = Change {
        actor: "t2-native-audit-fixture",
        git_commit: Some(&head),
    };
    store.create_claim(
        NewClaim {
            id: CLAIM,
            title: "Native audit fixture",
            informal_statement: STATEMENT,
            engine: "lean4",
            formal_target: Some("auditFixtureProof"),
            module_path: Some(DOWNSTREAM),
            critical: true,
        },
        change,
    )?;
    store.set_local_trust(CLAIM, Trust::L4a, change)?;
    let seq = store.status()?.last_event_seq;
    let expected_debt = serde_json::to_value(store.scan_trust_debt()?)?;
    let sqlite_snapshot = || -> Result<_> {
        // The shared-memory WAL index is transient; the database and WAL are durable.
        Ok((
            file_sha256(&root.join(STATE))?,
            file_sha256(&root.join(WAL))?,
        ))
    };
    let sqlite_before = sqlite_snapshot()?;
    let git_before = repo_snapshot(&repo)?;
    let audit = |reviewed: &str| {
        ai(&root, "t2-audit")
            .arg(&repo)
            .args([ANCHOR, reviewed])
            .arg(&lean)
            .output()
    };

    let pass = checked(audit(&head)?)?;
    let report: Value = serde_json::from_slice(&pass)?;
    assert_eq!(report["schema"], "ai4math.t2.audit.v1");
    assert_eq!(report["advisory_only"], true);
    assert_eq!(report["reviewed_commit"], head.as_str());
    assert_eq!(report["ledger_event_seq"], seq);

    let build = &report["clean_build"];
    assert_eq!(build["schema"], "ai4math.t2.clean-build.v1");
    assert_eq!(build["advisory_only"], true);
    assert_eq!(build["reviewed_commit"], report["reviewed_commit"]);
    assert_eq!(build["ledger_event_seq"], report["ledger_event_seq"]);
    assert_eq!(build["engine"], "lean4");
    assert_eq!(build.get("external_checker_passed"), Some(&Value::Null));
    assert_eq!(build["compiler_sha256"], file_sha256(&lean)?);
    assert!(build["compile_ms"].as_u64().is_some());
    let modules = build["modules"]
        .as_array()
        .context("expected built modules")?;
    assert_eq!(modules.len(), 2);
    for (module, (path, source)) in modules.iter().zip([(ANCHOR, BASIS), (DOWNSTREAM, VALID)]) {
        assert_eq!(module["path"], path);
        assert_eq!(module["source_sha256"], sha256(source.as_bytes()));
        assert!(module["compile_ms"].as_u64().is_some());
    }

    let debt = &report["trust_debt"];
    assert_eq!(debt["schema"], "ai4math.t2.trust-debt-scan.v1");
    assert_eq!(debt["advisory_only"], true);
    assert_eq!(debt["ledger_event_seq"], report["ledger_event_seq"]);
    assert_eq!(debt["finding_count"], 2);
    for (finding, role) in debt["findings"]
        .as_array()
        .context("expected debt findings")?
        .iter()
        .zip(["direct", "effective_ceiling"])
    {
        assert_eq!(finding["claim_id"], CLAIM);
        assert_eq!(finding["kind"], "oracle");
        assert_eq!(finding["role"], role);
        assert_eq!(finding["local_trust"], "L4a");
        assert_eq!(finding["effective_trust"], "L4a");
    }
    assert_eq!(debt, &expected_debt);
    assert!(!String::from_utf8(pass)?.contains(STATEMENT));
    assert_eq!(
        sqlite_snapshot()?,
        sqlite_before,
        "successful audit wrote SQLite"
    );
    assert_eq!(
        repo_snapshot(&repo)?,
        git_before,
        "successful audit wrote Git"
    );
    assert_eq!(store.status()?.last_event_seq, seq);
    let claim = store.claim(CLAIM)?.context("missing audit fixture claim")?;
    assert_eq!(claim.local_trust, Trust::L4a);
    assert_eq!(claim.effective_trust, Trust::L4a);
    assert!(claim.receipt_id.is_none());
    assert!(store.managed_proof_recipe(CLAIM)?.is_none());

    // Move main ourselves, then re-baseline Git. The still-valid anchor must not produce
    // a partial build or debt report when the downstream proof fails to elaborate.
    fs::write(source.join(DOWNSTREAM), BROKEN)?;
    git(&source, &["add", "--", DOWNSTREAM])?;
    git(
        &source,
        &["commit", "--quiet", "-m", "broken downstream proof"],
    )?;
    git(&source, &["push", "--quiet", "../managed.git", "main"])?;
    let broken_head = git(&repo, &["rev-parse", "--verify", "refs/heads/main"])?;
    assert_ne!(head, broken_head);
    let git_before = repo_snapshot(&repo)?;
    let rejected = audit(&broken_head)?;
    let error = String::from_utf8_lossy(&rejected.stderr);
    assert!(!rejected.status.success(), "broken build accepted: {error}");
    assert!(
        rejected.stdout.is_empty(),
        "failed audit emitted a partial report"
    );
    assert!(error.contains(DOWNSTREAM), "wrong module failed: {error}");
    assert!(
        error.contains("auditFixtureMissingProof"),
        "missing Lean diagnostic: {error}"
    );
    assert_eq!(
        sqlite_snapshot()?,
        sqlite_before,
        "rejected audit wrote SQLite"
    );
    assert_eq!(
        repo_snapshot(&repo)?,
        git_before,
        "rejected audit wrote Git"
    );
    assert_eq!(store.status()?.last_event_seq, seq);
    Ok(())
}
