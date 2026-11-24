use super::*;
use std::{
    path::PathBuf,
    process::{Command, Output},
};

const MAIN: &str = "refs/heads/main";
const STATE: &str = ".runtime/state.sqlite";
const MODULE: &str = "Proof.lean";
const BODY: &str = "True.intro";

fn git_output(dir: &Path, args: &[&str]) -> Output {
    Command::new("/usr/bin/git")
        .current_dir(dir)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .args([
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@localhost",
            "-c",
            "commit.gpgSign=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args(args)
        .output()
        .unwrap()
}

pub(super) fn git(dir: &Path, args: &[&str]) -> String {
    let out = git_output(dir, args);
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().into()
}

fn setup(root: &Path) -> Result<PathBuf> {
    let source = root.join("source");
    fs::create_dir(&source)?;
    init(root, Engine::Lean4)?;
    git(&source, &["init", "--initial-branch=main"]);
    fs::write(
        source.join(MODULE),
        "theorem target : True := {{PROOF_BODY}}
",
    )?;
    git(&source, &["add", MODULE]);
    git(&source, &["commit", "-m", "freeze target"]);
    git(root, &["clone", "--bare", "source", "repo.git"]);
    declare_claim(root, "C", MODULE, "target", "Target", "True")?;
    Ok(root.join("repo.git"))
}

fn child_commit(repo: &Path, base: &str, message: &str) -> String {
    let tree = git(repo, &["rev-parse", &format!("{base}^{{tree}}")]);
    git(repo, &["commit-tree", &tree, "-p", base, "-m", message])
}

fn assert_unpromoted(ledger: &StateStore, expected: Option<&PromotionIntent>) -> Result<()> {
    assert_eq!(ledger.promotion_intent()?.as_ref(), expected);
    assert!(ledger.managed_proof_recipe("C")?.is_none());
    assert_eq!(
        ledger.claim("C")?.unwrap().local_trust,
        ai4math_store::Trust::L0
    );
    assert!(
        ledger
            .events_after(0, 100)?
            .iter()
            .all(|e| e.event_type != "PROMOTION_COMMITTED")
    );
    Ok(())
}

fn assert_promoted(
    root: &Path,
    repo: &Path,
    intent: &PromotionIntent,
    source_sha256: &str,
) -> Result<()> {
    let commit = intent.new_commit.as_str();
    let mut ledger = StateStore::open(root.join(STATE))?;
    let recipe = ledger
        .managed_proof_recipe("C")?
        .context("missing finalized proof recipe")?;
    assert_eq!(recipe.claim_id, intent.claim_id);
    assert_eq!(recipe.base_commit, intent.base_commit);
    assert_eq!(recipe.promoted_commit, commit);
    assert_eq!(recipe.file_path, MODULE);
    assert_eq!(recipe.target_decl, "target");
    assert_eq!(recipe.proof_body, BODY);
    assert_eq!(recipe.source_sha256, source_sha256);
    assert_eq!(
        ledger.claim("C")?.unwrap().local_trust,
        ai4math_store::Trust::L5
    );
    assert!(ledger.promotion_intent()?.is_none());
    let events: Vec<_> = ledger
        .events_after(0, 100)?
        .into_iter()
        .filter(|e| e.event_type == "PROMOTION_COMMITTED")
        .collect();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].git_commit.as_deref(), Some(commit));
    let facts = ledger.briefing_facts()?;
    assert!(
        facts
            .milestones
            .items
            .iter()
            .any(|item| { item.claim_id == "C" && item.git_commit.as_deref() == Some(commit) })
    );
    let locked = PromotionRepo::lock(repo)?;
    assert!(locked.pending_ref(&root.join(STATE))?.current()?.is_none());
    Ok(())
}

fn prepare_native(
    tx: &Promotion<'_>,
    exec: &Path,
) -> Result<(PromotionIntent, VerifiedCandidate, RegressionArtifacts)> {
    let (result, artifacts) =
        verify_and_regress(tx, Engine::Lean4, MODULE, "target", exec, BODY, None)?;
    let commit = tx.prepare_commit(result.source().as_bytes(), result.receipt())?;
    let intent = PromotionIntent {
        repo: tx.repo().to_str().unwrap().into(),
        claim_id: "C".into(),
        base_commit: tx.base().into(),
        new_commit: commit,
        file_path: MODULE.into(),
        target_decl: "target".into(),
        proof_body: BODY.into(),
    };
    Ok((intent, result, artifacts))
}

#[test]
fn managed_pending_pin_survives_gc_and_orphan_cleanup_is_ledger_scoped() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let root = temp.path();
    let repo = setup(root)?;
    let other_root = root.join("other");
    init(&other_root, Engine::Lean4)?;
    let base = git(&repo, &["rev-parse", MAIN]);
    let own = child_commit(&repo, &base, "unfinished pin before SQLite prepare");
    let other = child_commit(&repo, &base, "another ledger's pending commit");
    {
        let locked = PromotionRepo::lock(&repo)?;
        let pin = locked.pending_ref(&root.join(STATE))?;
        pin.ensure_pinned(&own)?;
        assert!(pin.ensure_pinned(&other).is_err());
        assert!(pin.remove(&other).is_err());
        locked
            .pending_ref(&other_root.join(STATE))?
            .ensure_pinned(&other)?;
    }
    git(&repo, &["pack-refs", "--all"]);
    git(&repo, &["gc", "--prune=now"]);
    assert_eq!(git(&repo, &["cat-file", "-t", &own]), "commit");
    assert_eq!(git(&repo, &["rev-parse", MAIN]), base);

    // Both a crash before journaling and a crash after SQLite finalization leave
    // a pin without an intent. Recovery should find and remove only this slot.
    recover_promotion(root, &repo, Path::new("not-a-toolchain"), None)?;
    recover_promotion(root, &repo, Path::new("not-a-toolchain"), None)?;
    {
        let locked = PromotionRepo::lock(&repo)?;
        assert!(locked.pending_ref(&root.join(STATE))?.current()?.is_none());
        assert_eq!(
            locked
                .pending_ref(&other_root.join(STATE))?
                .current()?
                .as_deref(),
            Some(other.as_str())
        );
    }
    git(&repo, &["gc", "--prune=now"]);
    assert!(
        !git_output(&repo, &["cat-file", "-e", &own])
            .status
            .success()
    );
    assert_eq!(git(&repo, &["cat-file", "-t", &other]), "commit");
    assert_unpromoted(&StateStore::open(root.join(STATE))?, None)
}

#[test]
fn managed_git_cas_rejects_concurrent_main_update_without_unpinning() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let root = temp.path();
    let repo = setup(root)?;
    let locked = PromotionRepo::lock(&repo)?;
    let tx = locked.snapshot(MODULE, None)?;
    let pin = locked.pending_ref(&root.join(STATE))?;
    let candidate = child_commit(&repo, tx.base(), "prepared candidate");
    pin.ensure_pinned(&candidate)?;
    let competing = child_commit(&repo, tx.base(), "concurrent writer");
    // This independent Git process changes main while the coordinator is holding
    // a snapshot. No timing races or delays are needed to reproduce stale CAS.
    git(&repo, &["update-ref", MAIN, &competing, tx.base()]);
    let error = tx.advance(&candidate).unwrap_err();
    assert!(error.to_string().contains("main did not advance"));
    assert_eq!(git(&repo, &["rev-parse", MAIN]), competing);
    assert_eq!(pin.current()?.as_deref(), Some(candidate.as_str()));
    assert!(!tx.currently_published(&candidate)?);
    Ok(())
}

#[test]
#[ignore = "requires native host without outer Seatbelt and AI4MATH_TEST_LEAN"]
fn managed_promotion_recovers_after_git_publication_and_is_idempotent() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let root = temp.path();
    let repo = setup(root)?;
    let exec = PathBuf::from(env::var_os("AI4MATH_TEST_LEAN").context("set native Lean binary")?);
    let (intent, source_sha256) = {
        let mut store = StateStore::open(root.join(STATE))?;
        let locked = PromotionRepo::lock(&repo)?;
        let tx = locked.snapshot(MODULE, None)?;
        let pin = locked.pending_ref(&root.join(STATE))?;
        let (intent, result, artifacts) = prepare_native(&tx, &exec)?;
        let not_regressed = VerifiedReceipt::from_broker(&result)?;
        assert!(journal_promotion(&mut store, &pin, &intent, &not_regressed).is_err());
        assert!(pin.current()?.is_none());
        assert_unpromoted(&store, None)?;

        let receipt = VerifiedReceipt::from_promotion(&result, &artifacts)?;
        journal_promotion(&mut store, &pin, &intent, &receipt)?;
        tx.advance(&intent.new_commit)?;
        assert_unpromoted(&store, Some(&intent))?;
        // Drop both locks before finalizing; the restart uses only disk state.
        (intent, result.receipt().patch_sha256().to_owned())
    };
    // A later fast-forward does not erase publication of the prepared commit.
    let later = child_commit(&repo, &intent.new_commit, "later main commit");
    git(&repo, &["update-ref", MAIN, &later, &intent.new_commit]);
    recover_promotion(root, &repo, &exec, None)?;
    recover_promotion(root, &repo, &exec, None)?;
    assert_eq!(git(&repo, &["rev-parse", MAIN]), later);
    assert_promoted(root, &repo, &intent, &source_sha256)
}

#[test]
#[ignore = "requires native host without outer Seatbelt and AI4MATH_TEST_LEAN"]
fn managed_promotion_recovers_after_gc_and_concurrent_cas_failure() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let root = temp.path();
    let repo = setup(root)?;
    let exec = PathBuf::from(env::var_os("AI4MATH_TEST_LEAN").context("set native Lean binary")?);
    let (intent, competing, source_sha256) = {
        let mut store = StateStore::open(root.join(STATE))?;
        let locked = PromotionRepo::lock(&repo)?;
        let tx = locked.snapshot(MODULE, None)?;
        let pin = locked.pending_ref(&root.join(STATE))?;
        let (intent, result, artifacts) = prepare_native(&tx, &exec)?;
        let receipt = VerifiedReceipt::from_promotion(&result, &artifacts)?;
        journal_promotion(&mut store, &pin, &intent, &receipt)?;
        let competing = child_commit(&repo, tx.base(), "concurrent main update");
        git(&repo, &["update-ref", MAIN, &competing, tx.base()]);
        assert!(tx.advance(&intent.new_commit).is_err());
        assert!(finish_promotion(&tx, &mut store, &pin, &intent, &receipt).is_err());
        assert_unpromoted(&store, Some(&intent))?;
        (
            intent,
            competing,
            result.receipt().patch_sha256().to_owned(),
        )
    };
    git(&repo, &["gc", "--prune=now"]);
    assert_eq!(
        git(&repo, &["cat-file", "-t", &intent.new_commit]),
        "commit"
    );
    let error = recover_promotion(root, &repo, &exec, None).unwrap_err();
    assert!(error.to_string().contains("main diverged"));
    assert_eq!(git(&repo, &["rev-parse", MAIN]), competing);
    assert_unpromoted(&StateStore::open(root.join(STATE))?, Some(&intent))?;
    {
        let locked = PromotionRepo::lock(&repo)?;
        assert_eq!(
            locked.pending_ref(&root.join(STATE))?.current()?.as_deref(),
            Some(intent.new_commit.as_str())
        );
    }

    // Once the operator restores the original base, recovery re-verifies and
    // publishes the *same* prepared commit, not a regenerated replacement.
    git(
        &repo,
        &["update-ref", MAIN, &intent.base_commit, &competing],
    );
    recover_promotion(root, &repo, &exec, None)?;
    recover_promotion(root, &repo, &exec, None)?;
    assert_eq!(git(&repo, &["rev-parse", MAIN]), intent.new_commit);
    assert_promoted(root, &repo, &intent, &source_sha256)
}

#[test]
fn managed_promotion_respects_primary_engine_before_git() -> Result<()> {
    let temp = tempfile::tempdir()?;
    init(temp.path(), Engine::Agda)?;
    let error = promote_managed(
        temp.path(),
        Path::new("irrelevant"),
        "C",
        MODULE,
        "target",
        Path::new("irrelevant"),
        Path::new("irrelevant"),
        None,
    )
    .unwrap_err();
    assert!(error.to_string().contains("primary engine"));
    Ok(())
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "requires native host without outer Seatbelt and AI4MATH_TEST_LEAN"]
fn native_t2_clean_build_is_advisory_and_fail_closed() -> Result<()> {
    use serde_json::Value;
    use sha2::{Digest, Sha256};

    const ANCHOR: &str = "Support.lean";
    const MANIFEST: &str = "ai4math-modules.txt";
    const ORDERED: &str = "Support.lean\nProof.lean\n";
    const REVERSED: &str = "Proof.lean\nSupport.lean\n";
    const SUPPORT: &str = "theorem helper : True := True.intro\n";
    const PROOF: &str = "import Support\ntheorem target : True := helper\n";
    const INVALID_PROOF: &str = "import Support\ntheorem target : False := helper\n";

    let exec = PathBuf::from(env::var_os("AI4MATH_TEST_LEAN").context("set native Lean binary")?);
    let temp = tempfile::tempdir()?;
    let root = temp.path();
    let source = root.join("source");
    let repo = root.join("repo.git");
    init(root, Engine::Lean4)?;
    fs::create_dir(&source)?;
    git(&source, &["init", "--initial-branch=main"]);
    for (path, content) in [(MANIFEST, ORDERED), (ANCHOR, SUPPORT), (MODULE, PROOF)] {
        fs::write(source.join(path), content)?;
    }
    git(&source, &["add", MANIFEST, ANCHOR, MODULE]);
    git(
        &source,
        &["commit", "-m", "review both modules in dependency order"],
    );
    git(root, &["clone", "--bare", "source", "repo.git"]);
    let reviewed = git(&repo, &["rev-parse", MAIN]);
    declare_claim(root, "C", MODULE, "target", "Target", "True")?;
    let mut store = StateStore::open(root.join(STATE))?;
    {
        let locked = PromotionRepo::try_lock(&repo)?;
        store.bind_task_repository(
            locked.path().to_str().context("UTF-8 repo path")?,
            &reviewed,
        )?;
    }
    let seq = store.status()?.last_event_seq;
    let claim = serde_json::to_value(store.claim("C")?.context("missing fixture claim")?)?;

    // Inspect the durable SQLite bytes (including the WAL, not its transient
    // index) and the full Git object/ref inventories, not just main.
    let sqlite = || -> Result<_> {
        let db = fs::read(root.join(STATE))?;
        let wal = match fs::read(root.join(".runtime/state.sqlite-wal")) {
            Ok(wal) => Some(wal),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
            Err(err) => return Err(err.into()),
        };
        Ok((db, wal))
    };
    let git_state = || -> Result<_> {
        Ok((
            fs::read(repo.join("HEAD"))?,
            git(
                &repo,
                &[
                    "for-each-ref",
                    "--format=%(refname) %(objectname) %(symref)",
                ],
            ),
            git(&repo, &["cat-file", "--batch-all-objects", "--batch-check"]),
        ))
    };
    let assert_no_authority_change = || -> Result<()> {
        assert_eq!(store.status()?.last_event_seq, seq);
        assert_eq!(
            serde_json::to_value(store.claim("C")?.context("missing fixture claim")?)?,
            claim
        );
        assert_unpromoted(&store, None)?;
        assert!(store.pivot_intent()?.is_none());
        Ok(())
    };

    let db_before = sqlite()?;
    let git_before = git_state()?;
    let report = serde_json::to_value(crate::t2_build::audit(
        root, &repo, ANCHOR, &reviewed, &exec,
    )?)?;
    assert_eq!(report["schema"], "ai4math.t2.clean-build.v1");
    assert_eq!(report["advisory_only"], true);
    assert_eq!(report["reviewed_commit"], reviewed);
    assert_eq!(report["engine"], "lean4");
    assert_eq!(report.get("external_checker_passed"), Some(&Value::Null));
    assert_eq!(report["ledger_event_seq"].as_i64(), Some(seq));
    let source_set = report["source_set_sha256"]
        .as_str()
        .context("missing source-set digest")?;
    assert_eq!(source_set.len(), 64);
    assert!(
        source_set
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    );
    let mut compiler_hash = Sha256::new();
    std::io::copy(&mut fs::File::open(&exec)?, &mut compiler_hash)?;
    assert_eq!(
        report["compiler_sha256"],
        format!("{:x}", compiler_hash.finalize())
    );
    assert!(report["compile_ms"].as_u64().is_some());
    let modules = report["modules"]
        .as_array()
        .context("missing ordered module reports")?;
    assert_eq!(modules.len(), 2);
    for (module, (path, content)) in modules.iter().zip([(ANCHOR, SUPPORT), (MODULE, PROOF)]) {
        assert_eq!(module["path"], path);
        assert_eq!(
            module["source_sha256"],
            format!("{:x}", Sha256::digest(content.as_bytes()))
        );
        assert!(module["compile_ms"].as_u64().is_some());
    }
    assert!(sqlite()? == db_before, "a successful audit wrote SQLite");
    assert!(git_state()? == git_before, "a successful audit wrote Git");
    assert_no_authority_change()?;

    let assert_rejected = |oid: &str, exe: &Path, expected: &[&str]| -> Result<()> {
        let git_before = git_state()?;
        let Err(error) = crate::t2_build::audit(root, &repo, ANCHOR, oid, exe) else {
            panic!("an audit expected to fail ({expected:?}) produced a report");
        };
        let diagnostic = format!("{error:#}");
        assert!(
            expected
                .iter()
                .all(|&fragment| diagnostic.contains(fragment)),
            "unexpected refusal: {diagnostic}"
        );
        assert!(sqlite()? == db_before, "a rejected audit wrote SQLite");
        assert!(git_state()? == git_before, "a rejected audit wrote Git");
        assert_no_authority_change()
    };
    let commit_to_main = |manifest: &str, proof: &str, message: &str| -> Result<String> {
        fs::write(source.join(MANIFEST), manifest)?;
        fs::write(source.join(MODULE), proof)?;
        git(&source, &["add", MANIFEST, MODULE]);
        git(&source, &["commit", "-m", message]);
        git(&source, &["push", "../repo.git", "main"]);
        Ok(git(&repo, &["rev-parse", MAIN]))
    };

    let wrong_order = commit_to_main(REVERSED, PROOF, "move importer before its dependency")?;
    assert_rejected(
        &wrong_order,
        &exec,
        &["tracked Lean clean build failed", MODULE, "Support"],
    )?;
    let broken = commit_to_main(ORDERED, INVALID_PROOF, "break only the downstream proof")?;
    assert_rejected(
        &broken,
        &exec,
        &["tracked Lean clean build failed", MODULE, "False"],
    )?;
    // Reject a stale but previously successful commit before resolving the compiler.
    assert_rejected(
        &reviewed,
        &root.join("missing-compiler"),
        &["reviewed T2 commit is not current main"],
    )
}
