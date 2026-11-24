use super::*;
use ai4math_store::Trust;
use managed_tests::git;
use std::path::PathBuf;

const MODULE: &str = "Goal.v";
const SOURCE: &str =
    "Require Import Support.\nTheorem target : True.\nProof.\n  exact ({{PROOF_BODY}}).\nQed.\n";

fn setup(root: &Path) -> Result<PathBuf> {
    init(root, Engine::Rocq)?;
    let src = root.join("source");
    fs::create_dir(&src)?;
    git(&src, &["init", "--initial-branch=main"]);
    fs::write(src.join("Support.v"), "Definition holds : True := I.\n")?;
    fs::write(src.join(MODULE), SOURCE)?;
    fs::write(src.join("ai4math-modules.txt"), "Support.v\nGoal.v\n")?;
    git(&src, &["add", "."]);
    git(&src, &["commit", "-m", "frozen Rocq modules"]);
    git(root, &["clone", "--bare", "source", "repo.git"]);
    declare_claim(
        root,
        "rocq-claim",
        MODULE,
        "target",
        "Truth fixture",
        "Truth is inhabited.",
    )?;
    Ok(root.join("repo.git"))
}

#[test]
fn rocq_manifest_and_primary_binding_reject_mixed_engines() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path();
    let repo = setup(root)?;
    assert_eq!(bound_engine(root, MODULE)?, Engine::Rocq);
    assert!(bound_engine(root, "Goal.lean").is_err());
    let lock = PromotionRepo::lock(&repo)?;
    assert_eq!(
        lock.snapshot(MODULE, None)?
            .project()?
            .iter()
            .map(|(name, _)| *name)
            .collect::<Vec<_>>(),
        vec!["Support.v", "Goal.v"]
    );
    drop(lock);
    let source = root.join("source");
    fs::rename(source.join("Support.v"), source.join("Support.lean"))?;
    fs::write(source.join("ai4math-modules.txt"), "Support.lean\nGoal.v\n")?;
    git(&source, &["add", "."]);
    git(&source, &["commit", "-m", "mixed languages"]);
    git(
        &source,
        &["push", repo.to_str().unwrap(), "HEAD:refs/heads/main"],
    );
    assert!(
        PromotionRepo::lock(&repo)?
            .snapshot(MODULE, None)
            .err()
            .context("mixed project unexpectedly allowed")?
            .to_string()
            .contains("same proof engine")
    );
    Ok(())
}

#[test]
#[cfg(target_os = "macos")]
#[ignore = "requires native Seatbelt and AI4MATH_TEST_ROCQ / AI4MATH_TEST_ROCQ_DATA"]
fn managed_rocq_promotion_rejects_bad_proofs_then_publishes_closed_term() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path();
    let repo = setup(root)?;
    let base = git(&repo, &["rev-parse", "refs/heads/main"]);
    let exe =
        PathBuf::from(env::var_os("AI4MATH_TEST_ROCQ").context("missing native Rocq executable")?);
    let data =
        env::var("AI4MATH_TEST_ROCQ_DATA").context("missing native Rocq coqlib directory")?;
    let body = root.join("body.txt");
    let store = StateStore::open_readonly(root.join(".runtime/state.sqlite"))?;
    let before = store.status()?.last_event_seq;
    for rejected in ["I).\nAdmitted.\n(*", "(fun (n : nat) => n)"] {
        fs::write(&body, rejected)?;
        ensure!(
            promote_managed(
                root,
                &repo,
                "rocq-claim",
                MODULE,
                "target",
                &exe,
                &body,
                Some(&data)
            )
            .is_err(),
            "admitted invalid Rocq term"
        );
        assert_eq!(git(&repo, &["rev-parse", "refs/heads/main"]), base);
        assert_eq!(store.status()?.last_event_seq, before);
        assert!(store.promotion_intent()?.is_none());
    }
    fs::write(&body, "Support.holds")?;
    promote_managed(
        root,
        &repo,
        "rocq-claim",
        MODULE,
        "target",
        &exe,
        &body,
        Some(&data),
    )?;
    let head = git(&repo, &["rev-parse", "refs/heads/main"]);
    assert_ne!(head, base);
    assert!(git(&repo, &["show", "refs/heads/main:Goal.v"]).contains("Support.holds"));
    assert!(!git(&repo, &["show", "refs/heads/main:Goal.v"]).contains("Print Assumptions"));
    let claim = store.claim("rocq-claim")?.unwrap();
    assert_eq!(claim.local_trust, Trust::L5);
    assert!(claim.receipt_id.is_some());
    assert!(
        repo.join("ai4math-golden")
            .join(head)
            .join("Goal.vo")
            .is_file()
    );
    assert!(store.promotion_intent()?.is_none());
    Ok(())
}

#[test]
#[cfg(target_os = "macos")]
#[ignore = "requires native Seatbelt and AI4MATH_TEST_ROCQ / AI4MATH_TEST_ROCQ_DATA"]
fn rocq_recovers_exactly_once_after_git_publication() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path();
    let repo = setup(root)?;
    let state = root.join(".runtime/state.sqlite");
    let exe =
        PathBuf::from(env::var_os("AI4MATH_TEST_ROCQ").context("missing native Rocq executable")?);
    let data = env::var("AI4MATH_TEST_ROCQ_DATA")?;
    let prepared = {
        let locked = PromotionRepo::lock(&repo)?;
        let snapshot = locked.snapshot(MODULE, None)?;
        let pin = locked.pending_ref(&state)?;
        let mut store = StateStore::open(&state)?;
        let (candidate, artifacts) = verify_and_regress(
            &snapshot,
            Engine::Rocq,
            MODULE,
            "target",
            &exe,
            "Support.holds",
            Some(&data),
        )?;
        let commit = snapshot.prepare_commit(candidate.source().as_bytes(), candidate.receipt())?;
        let intent = PromotionIntent {
            repo: locked.path().to_str().unwrap().into(),
            claim_id: "rocq-claim".into(),
            base_commit: snapshot.base().into(),
            new_commit: commit.clone(),
            file_path: MODULE.into(),
            target_decl: "target".into(),
            proof_body: "Support.holds".into(),
        };
        let receipt = VerifiedReceipt::from_promotion(&candidate, &artifacts)?;
        journal_promotion(&mut store, &pin, &intent, &receipt)?;
        snapshot.advance(&commit)?;
        assert_eq!(store.claim("rocq-claim")?.unwrap().local_trust, Trust::L0);
        commit
    };
    assert!(recover_promotion(root, &repo, &exe, None).is_err());
    assert!(
        StateStore::open_readonly(&state)?
            .promotion_intent()?
            .is_some()
    );
    recover_promotion(root, &repo, &exe, Some(&data))?;
    let store = StateStore::open_readonly(&state)?;
    let seq = store.status()?.last_event_seq;
    recover_promotion(root, &repo, &exe, Some(&data))?;
    assert_eq!(store.status()?.last_event_seq, seq);
    assert_eq!(git(&repo, &["rev-parse", "refs/heads/main"]), prepared);
    assert_eq!(store.claim("rocq-claim")?.unwrap().local_trust, Trust::L5);
    assert!(store.promotion_intent()?.is_none());
    Ok(())
}
