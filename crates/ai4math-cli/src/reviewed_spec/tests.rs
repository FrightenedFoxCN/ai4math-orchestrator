use super::*;
use crate::{declare_claim, init, managed_tests::git, promote_managed};
use ai4math_store::{StateStore, Trust};
use std::{fs, path::PathBuf, process::Command};

fn ordinary_is_ancestor(repo: &Path, old: &str, new: &str) -> Result<bool> {
    let output = Command::new("/usr/bin/git")
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .arg("--git-dir")
        .arg(repo)
        .args(["merge-base", "--is-ancestor", old, new])
        .output()?;
    anyhow::ensure!(
        output.status.success() || output.status.code() == Some(1),
        "fixture Git ancestry failed"
    );
    Ok(output.status.success())
}

const ID: &str = "reviewed";
const SPEC: &str = "TrustedSpec.lean";
const TARGET: &str = "Goal.lean";
const MAIN: &str = "refs/heads/main";

fn setup(root: &Path, theorem_type: &str) -> Result<(PathBuf, PathBuf, String)> {
    init(root, Engine::Lean4)?;
    let checkout = root.join("checkout");
    fs::create_dir(&checkout)?;
    git(&checkout, &["init", "--initial-branch=main"]);
    fs::write(checkout.join("Support.lean"), "def fixed : Nat := 1\n")?;
    fs::write(
        checkout.join(SPEC),
        "import Support\ndef approved : Prop := Nat.succ 0 = fixed\n",
    )?;
    fs::write(
        checkout.join(TARGET),
        format!("theorem target : {theorem_type} := {HOLE}\n"),
    )?;
    fs::write(
        checkout.join("ai4math-modules.txt"),
        "Support.lean\nTrustedSpec.lean\nGoal.lean\n",
    )?;
    git(&checkout, &["add", "."]);
    git(
        &checkout,
        &["commit", "-m", "reviewed specification and separate target"],
    );
    let base = git(&checkout, &["rev-parse", "HEAD"]);
    git(root, &["clone", "--bare", "checkout", "repo.git"]);
    declare_claim(
        root,
        ID,
        TARGET,
        "target",
        "Reviewed goal",
        "A registered claim",
    )?;
    Ok((checkout, root.join("repo.git"), base))
}

fn commit_all(checkout: &Path, repo: &Path) -> String {
    git(checkout, &["add", "."]);
    git(checkout, &["commit", "-m", "fixture change"]);
    git(
        checkout,
        &["push", repo.to_str().unwrap(), "HEAD:refs/heads/main"],
    );
    git(checkout, &["rev-parse", "HEAD"])
}

#[test]
fn requires_exact_current_review_and_unchanged_ordered_prefix() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path();
    let (checkout, repo, base) = setup(root, "1 = 1")?;
    let store = StateStore::open_readonly(root.join(".runtime/state.sqlite"))?;
    let start = store.status()?.last_event_seq;
    assert!(bind(root, &repo, ID, TARGET, "target", &base).is_err());
    assert!(store.lean_spec(ID)?.is_none());
    assert_eq!(store.status()?.last_event_seq, start);
    bind(root, &repo, ID, SPEC, "approved", &base)?;
    let end = store.status()?.last_event_seq;
    bind(root, &repo, ID, SPEC, "approved", &base)?;
    assert_eq!(store.status()?.last_event_seq, end);
    assert!(bind(root, &repo, ID, SPEC, "changed", &base).is_err());
    assert_eq!(store.claim(ID)?.unwrap().local_trust, Trust::L0);
    fs::write(checkout.join("Support.lean"), "def fixed : Nat := 2\n")?;
    let new_head = commit_all(&checkout, &repo);
    assert!(bind(root, &repo, ID, SPEC, "approved", &base).is_err());
    let body = root.join("body.txt");
    fs::write(&body, "rfl")?;
    let err = promote_managed(
        root,
        &repo,
        ID,
        TARGET,
        "target",
        Path::new("/missing/lean"),
        &body,
        None,
    )
    .unwrap_err();
    assert!(
        format!("{err:#}").contains("ordered dependency prefix changed"),
        "{err:#}"
    );
    assert_eq!(git(&repo, &["rev-parse", MAIN]), new_head);
    assert!(store.promotion_intent()?.is_none());
    assert_eq!(store.status()?.last_event_seq, end);
    Ok(())
}

#[test]
fn reviewed_snapshot_uses_physical_ancestry_under_grafts() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path();
    let (_, repo, base) = setup(root, "1 = 1")?;
    let tree = git(&repo, &["rev-parse", &format!("{base}^{{tree}}")]);
    let child = git(
        &repo,
        &["commit-tree", &tree, "-p", &base, "-m", "physical child"],
    );
    let unrelated = git(&repo, &["commit-tree", &tree, "-m", "unrelated root"]);
    let binding = ReviewedLeanSpec {
        repo: PromotionRepo::lock(&repo)?.path().to_str().unwrap().into(),
        claim_id: ID.into(),
        approved_git_commit: base.clone(),
        spec_file: SPEC.into(),
        spec_decl: "approved".into(),
        spec_prefix_sha256: "0".repeat(64),
    };

    git(&repo, &["update-ref", MAIN, &child, &base]);
    fs::write(
        repo.join("info/grafts"),
        format!("{child}\n{unrelated} {base}\n"),
    )?;
    let locked = PromotionRepo::lock(&repo)?;
    let tx = locked.snapshot(TARGET, None)?;
    check_snapshot(&locked, &tx, &binding)?;
    assert!(!ordinary_is_ancestor(&repo, &base, &child)?);
    drop(locked);

    git(&repo, &["update-ref", MAIN, &unrelated, &child]);
    let locked = PromotionRepo::lock(&repo)?;
    let tx = locked.snapshot(TARGET, None)?;
    assert!(ordinary_is_ancestor(&repo, &base, &unrelated)?);
    let error = check_snapshot(&locked, &tx, &binding).unwrap_err();
    assert!(
        error.to_string().contains(
            "candidate base does not descend from the reviewed Lean specification snapshot"
        )
    );
    Ok(())
}

#[test]
#[cfg(target_os = "macos")]
#[ignore = "requires native Seatbelt and AI4MATH_TEST_LEAN"]
fn independently_compares_defeq_and_rejects_distinct_provable_statement() -> Result<()> {
    let exe = PathBuf::from(std::env::var_os("AI4MATH_TEST_LEAN").context("set native Lean")?);
    for (theorem_type, proof, expected) in [("1 = 1", "rfl", true), ("True", "True.intro", false)] {
        let dir = tempfile::tempdir()?;
        let root = dir.path();
        let (checkout, repo, base) = setup(root, theorem_type)?;
        bind(root, &repo, ID, SPEC, "approved", &base)?;
        fs::write(
            checkout.join(TARGET),
            format!(
                "-- candidate review after spec approval\ntheorem target : {theorem_type} := {HOLE}\n"
            ),
        )?;
        let candidate_base = commit_all(&checkout, &repo);
        let body = root.join("body.txt");
        fs::write(&body, proof)?;
        let store = StateStore::open_readonly(root.join(".runtime/state.sqlite"))?;
        let seq = store.status()?.last_event_seq;
        let result = promote_managed(root, &repo, ID, TARGET, "target", &exe, &body, None);
        let head = git(&repo, &["rev-parse", MAIN]);
        if expected {
            result?;
            assert_ne!(head, candidate_base);
            assert_eq!(store.claim(ID)?.unwrap().local_trust, Trust::L5);
            let comparison = store
                .lean_spec_comparison(&head)?
                .context("missing durable comparison")?;
            assert_eq!(comparison.spec_file, SPEC);
            assert_eq!(comparison.spec_decl, "approved");
            assert_eq!(comparison.base_commit, candidate_base);
            assert_eq!(
                comparison.spec_prefix_sha256,
                store.lean_spec(ID)?.unwrap().spec_prefix_sha256
            );
        } else {
            let err = result.unwrap_err();
            assert!(
                format!("{err:#}").contains("specification comparator failed"),
                "{err:#}"
            );
            assert_eq!(head, candidate_base);
            assert!(store.lean_spec_comparison(&head)?.is_none());
            assert_eq!(store.status()?.last_event_seq, seq);
            assert_eq!(store.claim(ID)?.unwrap().local_trust, Trust::L0);
        }
        assert!(store.promotion_intent()?.is_none());
    }
    Ok(())
}

#[test]
#[cfg(target_os = "macos")]
#[ignore = "requires native Seatbelt and AI4MATH_TEST_LEAN"]
fn managed_recovery_recompares_and_persists_once() -> Result<()> {
    use crate::{
        PromotionIntent, VerifiedReceipt, journal_promotion, recover_promotion, verify_promotion,
    };
    let exe = PathBuf::from(std::env::var_os("AI4MATH_TEST_LEAN").context("set native Lean")?);
    let dir = tempfile::tempdir()?;
    let root = dir.path();
    let (_, repo, base) = setup(root, "1 = 1")?;
    bind(root, &repo, ID, SPEC, "approved", &base)?;
    let state = root.join(".runtime/state.sqlite");
    let prepared = {
        let locked = PromotionRepo::lock(&repo)?;
        let snapshot = locked.snapshot(TARGET, None)?;
        let mut store = StateStore::open(&state)?;
        let pin = locked.pending_ref(&state)?;
        let spec = store
            .lean_spec(ID)?
            .context("reviewed specification missing")?;
        let (candidate, artifacts, compared) = verify_promotion(
            &snapshot,
            Engine::Lean4,
            TARGET,
            "target",
            &exe,
            "rfl",
            None,
            Some(&spec),
        )?;
        let compared = compared.context("missing comparison")?;
        let commit = snapshot.prepare_commit(candidate.source().as_bytes(), candidate.receipt())?;
        let intent = PromotionIntent {
            repo: locked.path().to_str().unwrap().into(),
            claim_id: ID.into(),
            base_commit: base,
            new_commit: commit.clone(),
            file_path: TARGET.into(),
            target_decl: "target".into(),
            proof_body: "rfl".into(),
        };
        let unchecked = VerifiedReceipt::from_promotion(&candidate, &artifacts)?;
        assert!(journal_promotion(&mut store, &pin, &intent, &unchecked).is_err());
        assert!(pin.current()?.is_none());
        let receipt = VerifiedReceipt::from_compared_promotion(&candidate, &artifacts, &compared)?;
        journal_promotion(&mut store, &pin, &intent, &receipt)?;
        snapshot.advance(&commit)?;
        assert!(store.lean_spec_comparison(&commit)?.is_none());
        assert_eq!(store.claim(ID)?.unwrap().local_trust, Trust::L0);
        (commit, compared.receipt_sha256().to_owned())
    };
    recover_promotion(root, &repo, &exe, None)?;
    let store = StateStore::open_readonly(&state)?;
    assert_eq!(
        store
            .lean_spec_comparison(&prepared.0)?
            .unwrap()
            .receipt_sha256,
        prepared.1
    );
    assert_eq!(store.claim(ID)?.unwrap().local_trust, Trust::L5);
    assert!(store.promotion_intent()?.is_none());
    let seq = store.status()?.last_event_seq;
    recover_promotion(root, &repo, &exe, None)?;
    assert_eq!(store.status()?.last_event_seq, seq);
    assert_eq!(git(&repo, &["rev-parse", MAIN]), prepared.0);
    Ok(())
}
