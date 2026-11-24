use super::*;
use crate::{declare_claim, init, managed_tests::git};
use ai4math_core::Engine;
use std::fs;

fn fixture(root: &Path) -> Result<(PathBuf, String)> {
    init(root, Engine::Lean4)?;
    let work = root.join("source");
    fs::create_dir(&work)?;
    git(&work, &["init", "--initial-branch=main"]);
    fs::write(work.join("Defs.lean"), "def x : Nat := 2\n")?;
    fs::write(
        work.join("Proof.lean"),
        "import Defs\ntheorem target : x = 2 := {{PROOF_BODY}}\n",
    )?;
    fs::write(work.join("ai4math-modules.txt"), "Defs.lean\nProof.lean\n")?;
    git(&work, &["add", "."]);
    git(&work, &["commit", "-m", "freeze theorem and dependency"]);
    git(root, &["clone", "--bare", "source", "repo.git"]);
    let repo = root.join("repo.git");
    let head = git(&repo, &["rev-parse", "refs/heads/main"]);
    declare_claim(
        root,
        "C",
        "Proof.lean",
        "target",
        "Fixture theorem",
        "x is two",
    )?;
    crate::worker_dispatch::bind(root, &repo, "Proof.lean", &head)?;
    Ok((repo, head))
}

#[test]
fn archive_refuses_unproved_claim_and_symbolic_review_without_writing() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path();
    let (repo, head) = fixture(root)?;
    let state = StateStore::open_readonly(root.join(".runtime/state.sqlite"))?;
    let seq = state.status()?.last_event_seq;
    let locked = PromotionRepo::try_lock(&repo)?;
    assert!(
        prepare(root, &locked, &state, "C", &head)
            .unwrap_err()
            .to_string()
            .contains("effective L5")
    );
    assert!(prepare(root, &locked, &state, "C", "HEAD").is_err());
    drop(locked);
    assert!(archive(root, &repo, "C", &head, &"0".repeat(64)).is_err());
    assert_eq!(state.status()?.last_event_seq, seq);
    assert_eq!(fs::read_dir(root.join(".mathwiki/sediment"))?.count(), 0);
    Ok(())
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "requires native macOS Seatbelt and absolute AI4MATH_TEST_LEAN"]
fn native_managed_wiki_archive_preserves_cold_provenance_without_authority() -> Result<()> {
    use ai4math_core::WikiStratum;
    use ai4math_store::{Change, Wiki, WikiQuery};
    let temp = tempfile::tempdir()?;
    let root = temp.path();
    let (repo, base) = fixture(root)?;
    let lean = PathBuf::from(std::env::var_os("AI4MATH_TEST_LEAN").context("set native Lean")?);
    let body = root.join("body.txt");
    fs::write(&body, "by\n  rfl")?;
    crate::promote_managed(root, &repo, "C", "Proof.lean", "target", &lean, &body, None)?;
    let head = git(&repo, &["rev-parse", "refs/heads/main"]);
    let mut state = StateStore::open(root.join(".runtime/state.sqlite"))?;
    assert_eq!(state.claim("C")?.unwrap().effective_trust, Trust::L5);
    let seq = state.status()?.last_event_seq;
    let locked = PromotionRepo::try_lock(&repo)?;
    let (p, artifact) = prepare(root, &locked, &state, "C", &head)?;
    assert_eq!(p.verified_at_commit, head);
    assert_eq!(p.dependency_order, ["Defs.lean"]);
    assert_eq!(p.dependency_hashes["Defs.lean"], sha(b"def x : Nat := 2\n"));
    let abs = root.join(&p.relative_path);
    assert!(!abs.exists());
    drop(locked);
    assert!(archive(root, &repo, "C", &base, &p.approval_sha256).is_err());
    assert!(archive(root, &repo, "C", &head, &"a".repeat(64)).is_err());
    assert!(!abs.exists());
    archive(root, &repo, "C", &head, &p.approval_sha256)?;
    assert_eq!(fs::read_to_string(&abs)?, artifact.markdown());
    archive(root, &repo, "C", &head, &p.approval_sha256)?;
    assert_eq!(state.status()?.last_event_seq, seq);
    assert_eq!(state.claim("C")?.unwrap().effective_trust, Trust::L5);
    assert_eq!(git(&repo, &["rev-parse", "refs/heads/main"]), head);
    let wiki_root = root.join(".mathwiki");
    let mut wiki = Wiki::open(wiki_root.join(".index.sqlite"))?;
    assert_eq!(wiki.rebuild(&wiki_root)?, 1);
    assert!(wiki.search(WikiQuery::default())?.hits.is_empty());
    let hits = wiki
        .search(WikiQuery {
            scope: WikiStratum::Sediment,
            ..WikiQuery::default()
        })?
        .hits;
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].summary.id, p.id);
    assert!(
        wiki.provenance(&p.id, 1, 16)?
            .edges
            .iter()
            .any(|e| e.dst_id == "C" && e.edge_type == "born_from_claim")
    );
    state.mark_t3(
        &["C"],
        &[],
        Change {
            actor: "fixture",
            git_commit: Some(&head),
        },
    )?;
    assert!(archive(root, &repo, "C", &head, &p.approval_sha256).is_err());
    assert_eq!(fs::read_to_string(&abs)?, artifact.markdown());
    Ok(())
}
