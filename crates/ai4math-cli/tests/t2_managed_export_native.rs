#![cfg(target_os = "macos")]

use ai4math_broker::ProjectNativeGate;
use ai4math_store::{Change, NewClaim, StateStore, Trust};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

const BASIS: &str = "ZManagedExportBasis.lean";
const TARGET: &str = "AManagedExportTarget.lean";
const IMPORTED: &str = "ManagedExportFixture.importedIdentity";
const THEOREM: &str = "ManagedExportFixture.localIdentity";
const DEFINITION: &str = "ManagedExportFixture.localDefinition";
const AXIOMATIC: &str = "ManagedExportFixture.axiomaticIdentity";
const CLAIM: &str = "managed-export-advisory-fixture";
const BASIS_SOURCE: &str = "prelude
namespace ManagedExportFixture
theorem importedIdentity {p : Prop} : p → p := fun h => h
end ManagedExportFixture
";
const TARGET_SOURCE: &str = "prelude
import ZManagedExportBasis
namespace ManagedExportFixture
theorem localIdentity {p : Prop} : p → p := @importedIdentity p
def localDefinition {p : Prop} : p → p := @importedIdentity p
axiom forbiddenIdentity {p : Prop} : p → p
theorem axiomaticIdentity {p : Prop} : p → p := @forbiddenIdentity p
end ManagedExportFixture
";

fn checked(output: Output, step: &str) -> Result<Vec<u8>> {
    ensure!(
        output.status.success(),
        "{step} failed ({}):
stdout: {}
stderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    Ok(output.stdout)
}

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("/usr/bin/git")
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .current_dir(dir)
        .args([
            "-c",
            "user.name=T2 Managed Export Fixture",
            "-c",
            "user.email=t2-managed-export@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()?;
    Ok(String::from_utf8(checked(output, args[0])?)?.trim().into())
}

fn ai(root: &Path, subcommand: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ai4math"));
    command.current_dir(root).arg(subcommand).arg(root);
    command
}

fn file_sha256(path: &Path) -> Result<String> {
    let mut digest = Sha256::new();
    std::io::copy(&mut fs::File::open(path)?, &mut digest)?;
    Ok(format!("{:x}", digest.finalize()))
}

fn wrong_pin(pin: &str) -> String {
    format!(
        "{}{}",
        if pin.starts_with('0') { '1' } else { '0' },
        &pin[1..]
    )
}

fn hex_field(value: &Value, len: usize) -> bool {
    value.as_str().is_some_and(|s| {
        s.len() == len
            && s.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

// Compare names and bytes across the whole fixture: Git refs, objects and reflogs,
// SQLite including its WAL, and any accidentally retained build/export artifacts.
// SQLite's shared-memory WAL index is transient and may change on read-only opens.
fn snapshot(dir: &Path) -> Result<BTreeMap<PathBuf, Option<String>>> {
    let mut result = BTreeMap::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(parent) = pending.pop() {
        for entry in fs::read_dir(parent)? {
            let entry = entry?;
            let path = entry.path();
            let relative = path.strip_prefix(dir)?;
            if relative == Path::new("project/.runtime/state.sqlite-shm") {
                continue;
            }
            let kind = entry.file_type()?;
            let digest = if kind.is_dir() {
                pending.push(path.clone());
                None
            } else {
                ensure!(kind.is_file(), "unexpected fixture entry: {path:?}");
                Some(file_sha256(&path)?)
            };
            result.insert(relative.to_path_buf(), digest);
        }
    }
    Ok(result)
}

fn rejected(output: Output, case: &str, diagnostic: &str) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{case}: unexpectedly accepted");
    assert!(output.stdout.is_empty(), "{case}: emitted a partial report");
    assert!(
        stderr.contains(diagnostic),
        "{case}: expected {diagnostic:?}, got {stderr:?}"
    );
}

#[test]
#[ignore = "requires native macOS Seatbelt and AI4MATH_TEST_LEAN from the bundled toolchain"]
fn native_t2_check_managed_checks_the_owned_theorem_without_authority() -> Result<()> {
    let lean = PathBuf::from(
        std::env::var_os("AI4MATH_TEST_LEAN").context("set AI4MATH_TEST_LEAN to bundled Lean")?,
    );
    ensure!(lean.is_absolute(), "AI4MATH_TEST_LEAN must be absolute");
    let lean = lean
        .canonicalize()
        .context("resolve native Lean executable")?;
    let bin = lean.parent().context("bundled Lean bin directory")?;
    let sibling_pin = |name: &str| -> Result<String> {
        let path = bin.join(name);
        ensure!(
            path.canonicalize()? == path,
            "expected canonical bundled {name}"
        );
        file_sha256(&path).with_context(|| format!("hash bundled {name}"))
    };
    let exporter_pin = sibling_pin("leanexport")?;
    let checker_pin = sibling_pin("nanoda_bin")?;

    let temp = tempfile::tempdir()?;
    let fixture = temp.path().canonicalize()?;
    let root = fixture.join("project");
    let source = fixture.join("source");
    let repo = fixture.join("managed.git");
    fs::create_dir(&root)?;
    fs::create_dir(&source)?;
    git(&source, &["init", "--quiet", "--initial-branch=main"])?;
    fs::write(source.join(BASIS), BASIS_SOURCE)?;
    fs::write(source.join(TARGET), TARGET_SOURCE)?;
    // Nonlexical order ensures Lean needs the manifest's predecessor build.
    fs::write(
        source.join("ai4math-modules.txt"),
        format!(
            "{BASIS}
{TARGET}
"
        ),
    )?;
    git(
        &source,
        &["add", "--", BASIS, TARGET, "ai4math-modules.txt"],
    )?;
    git(&source, &["commit", "--quiet", "-m", "add Lean fixture"])?;
    let stale = git(&source, &["rev-parse", "HEAD"])?;
    git(
        &source,
        &[
            "commit",
            "--allow-empty",
            "--quiet",
            "-m",
            "review Lean fixture",
        ],
    )?;
    git(
        &fixture,
        &[
            "clone",
            "--bare",
            "--no-hardlinks",
            "--quiet",
            "source",
            "managed.git",
        ],
    )?;
    let head = git(&repo, &["rev-parse", "--verify", "refs/heads/main"])?;
    assert_ne!(head, stale);
    checked(ai(&root, "init").arg("lean4").output()?, "init project")?;
    checked(
        ai(&root, "task-bind")
            .arg(&repo)
            .args([BASIS, &head])
            .output()?,
        "bind reviewed repository",
    )?;
    // Initialize the coordination inode before taking our no-writes baseline.
    drop(ProjectNativeGate::new(&root).try_heavy()?);

    // Keep the writable store connected so the WAL remains available for comparison.
    let mut store = StateStore::open(root.join(".runtime/state.sqlite"))?;
    store.create_claim(
        NewClaim {
            id: CLAIM,
            title: "Native managed export fixture",
            informal_statement: "A managed theorem check is not a promotion.",
            engine: "lean4",
            formal_target: Some(THEOREM),
            module_path: Some(TARGET),
            critical: true,
        },
        Change {
            actor: "t2-managed-export-fixture",
            git_commit: Some(&head),
        },
    )?;
    let seq = store.status()?.last_event_seq;
    let before = snapshot(&fixture)?;
    let assert_unchanged = |case: &str| -> Result<()> {
        assert_eq!(
            snapshot(&fixture)?,
            before,
            "{case}: wrote durable fixture files"
        );
        assert_eq!(
            store.status()?.last_event_seq,
            seq,
            "{case}: changed ledger generation"
        );
        let claim = store.claim(CLAIM)?.context("missing fixture Claim")?;
        assert_eq!(claim.local_trust, Trust::L0);
        assert_eq!(claim.effective_trust, Trust::L0);
        assert!(claim.receipt_id.is_none());
        assert!(store.managed_proof_recipe(CLAIM)?.is_none());
        Ok(())
    };
    let run = |decl: &str, commit: &str, exporter: &str, checker: &str| {
        ai(&root, "t2-check-managed")
            .arg(&repo)
            .args([TARGET, decl, commit])
            .arg(&lean)
            .args([exporter, checker])
            .output()
    };

    let report: Value = serde_json::from_slice(&checked(
        run(THEOREM, &head, &exporter_pin, &checker_pin)?,
        "check reviewed local theorem",
    )?)?;
    let build = &report["build"];
    let check = &report["independent_check"];
    assert_eq!(
        report,
        json!({
            "schema": "ai4math.t2.managed-theorem-check.v1",
            "advisory_only": true,
            "reviewed_commit": head,
            "ledger_event_seq": seq,
            "target_module": TARGET,
            "declaration": THEOREM,
            "build": build,
            "independent_check": check,
            "exporter_sha256": exporter_pin,
        })
    );
    assert_eq!(build["compiler_sha256"], file_sha256(&lean)?);
    assert!(hex_field(&build["source_set_sha256"], 64));
    assert!(build["compile_ms"].as_u64().is_some());
    let modules = build["modules"].as_array().context("built modules")?;
    assert_eq!(modules.len(), 2);
    for (module, (path, source)) in modules
        .iter()
        .zip([(BASIS, BASIS_SOURCE), (TARGET, TARGET_SOURCE)])
    {
        assert_eq!(module["path"], path);
        assert_eq!(
            module["source_sha256"],
            format!("{:x}", Sha256::digest(source.as_bytes()))
        );
        assert!(module["compile_ms"].as_u64().is_some());
    }
    assert_eq!(
        *check,
        json!({
            "schema": "ai4math.t2.lean-export-check.v1",
            "advisory_only": true,
            "checker": "nanoda",
            "accepted": true,
            "export_sha256": check["export_sha256"],
            "checker_sha256": checker_pin,
            "export_format": "3.1.0",
            "lean_git_commit": check["lean_git_commit"],
            "theorem_count": 2,
        })
    );
    assert!(hex_field(&check["export_sha256"], 64));
    assert!(hex_field(&check["lean_git_commit"], 40));
    assert_unchanged("accepted local theorem")?;

    let membership = "Lean declaration is not a public theorem owned by target module";
    for (declaration, diagnostic) in [
        (IMPORTED, membership),
        (DEFINITION, membership),
        (AXIOMATIC, "Nanoda rejected the Lean export"),
    ] {
        rejected(
            run(declaration, &head, &exporter_pin, &checker_pin)?,
            declaration,
            diagnostic,
        );
        assert_unchanged(declaration)?;
    }
    rejected(
        run(THEOREM, &stale, &exporter_pin, &checker_pin)?,
        "stale reviewed main",
        "reviewed T2 commit is not current main",
    );
    assert_unchanged("stale reviewed main")?;
    rejected(
        run(THEOREM, &head, &wrong_pin(&exporter_pin), &checker_pin)?,
        "stale exporter pin",
        "exporter SHA-256 mismatch",
    );
    assert_unchanged("stale exporter pin")?;
    rejected(
        run(THEOREM, &head, &exporter_pin, &wrong_pin(&checker_pin))?,
        "stale checker pin",
        "Nanoda checker SHA-256 mismatch",
    );
    assert_unchanged("stale checker pin")?;

    // The selected theorem has already passed at the end of the target prefix.
    // A separately broken module later in the manifest must suppress that success.
    const LATER: &str = "LaterManagedExport.lean";
    fs::write(
        source.join(LATER),
        "prelude\nimport AManagedExportTarget\ntheorem broken {p q : Prop} : p → q := fun h => h\n",
    )?;
    fs::write(
        source.join("ai4math-modules.txt"),
        format!("{BASIS}\n{TARGET}\n{LATER}\n"),
    )?;
    git(&source, &["add", "--", LATER, "ai4math-modules.txt"])?;
    git(
        &source,
        &["commit", "--quiet", "-m", "break later tracked module"],
    )?;
    git(&source, &["push", "--quiet", "../managed.git", "main"])?;
    let broken_head = git(&repo, &["rev-parse", "--verify", "refs/heads/main"])?;
    assert_ne!(head, broken_head);
    let before_rejected = snapshot(&fixture)?;
    rejected(
        run(THEOREM, &broken_head, &exporter_pin, &checker_pin)?,
        "failed later module",
        "build isolated Lean module \"LaterManagedExport.lean\"",
    );
    assert_eq!(
        snapshot(&fixture)?,
        before_rejected,
        "failed later module wrote durable fixture files"
    );
    assert_eq!(store.status()?.last_event_seq, seq);
    assert_eq!(
        store
            .claim(CLAIM)?
            .context("missing fixture Claim")?
            .local_trust,
        Trust::L0
    );
    Ok(())
}
