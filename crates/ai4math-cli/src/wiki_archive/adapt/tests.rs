#![cfg(target_os = "macos")]

use super::{apply, prepare};
use crate::{
    breaker_probes, declare_claim, init, managed_tests::git, promote_managed,
    promotion::PromotionRepo, reviewed_spec, wiki_archive, worker_dispatch,
};
use ai4math_core::Engine;
use ai4math_store::{Change, StateStore, TaskSchedulerStatus, TaskTrack, Trust};
use anyhow::{Context, Result};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

const MAIN: &str = "refs/heads/main";
const DB: &str = ".runtime/state.sqlite";
const ORIGIN: &str = "HistoricalClaim";
const TARGET: &str = "AdaptedClaim";
const OLD_MODULE: &str = "Proof.lean";
const NEW_MODULE: &str = "Adapt.lean";
const SPEC: &str = "TrustedSpec.lean";
const SPEC_DECL: &str = "approved";
const FROZEN: &str = "import Defs\ntheorem adapted : x - 1 = 2 := {{PROOF_BODY}}\n";

type Tree = BTreeMap<PathBuf, Option<Vec<u8>>>;

// Compare the bare repo's objects and refs, not merely the name of main.
fn snapshot_tree(root: &Path, tree: &mut Tree) -> Result<()> {
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        let kind = entry.file_type()?;
        if kind.is_dir() {
            tree.insert(path.clone(), None);
            snapshot_tree(&path, tree)?;
        } else {
            assert!(kind.is_file(), "unexpected fixture entry: {path:?}");
            tree.insert(path.clone(), Some(fs::read(path)?));
        }
    }
    Ok(())
}

struct Authority {
    claims: [Value; 2],
    scheduler: TaskSchedulerStatus,
    git: Tree,
    wiki: Tree,
}

impl Authority {
    fn read(root: &Path, repo: &Path, state: &StateStore) -> Result<Self> {
        let mut git = Tree::new();
        let mut wiki = Tree::new();
        snapshot_tree(repo, &mut git)?;
        snapshot_tree(&root.join(".mathwiki"), &mut wiki)?;
        Ok(Self {
            claims: [
                serde_json::to_value(state.claim(ORIGIN)?)?,
                serde_json::to_value(state.claim(TARGET)?)?,
            ],
            scheduler: state.task_scheduler_status()?,
            git,
            wiki,
        })
    }

    fn assert_unchanged(&self, root: &Path, repo: &Path, state: &StateStore) -> Result<()> {
        let after = Self::read(root, repo, state)?;
        assert_eq!(self.claims, after.claims, "changed Claim state or trust");
        assert_eq!(self.scheduler, after.scheduler, "changed task scheduler");
        assert!(self.git == after.git, "changed bare Git repository");
        assert!(self.wiki == after.wiki, "changed Wiki tree or bytes");
        Ok(())
    }
}

#[test]
#[ignore = "requires native macOS Seatbelt and absolute AI4MATH_TEST_LEAN"]
fn native_sediment_adaptation_approves_only_a_fresh_target_without_granting_authority() -> Result<()>
{
    let lean = PathBuf::from(std::env::var_os("AI4MATH_TEST_LEAN").context("set native Lean")?);
    let dir = tempfile::tempdir()?;
    let root = dir.path();
    init(root, Engine::Lean4)?;
    let source = root.join("source");
    fs::create_dir(&source)?;
    git(&source, &["init", "--initial-branch=main"]);
    // Defs is outside the reviewed spec prefix so it can drift independently.
    fs::write(source.join(SPEC), "def approved : Prop := 2 = 2\n")?;
    fs::write(source.join("Defs.lean"), "def x : Nat := 2\n")?;
    fs::write(
        source.join(OLD_MODULE),
        "import Defs\ntheorem original : x = x := {{PROOF_BODY}}\n",
    )?;
    fs::write(
        source.join("ai4math-modules.txt"),
        "TrustedSpec.lean\nDefs.lean\nProof.lean\n",
    )?;
    git(&source, &["add", "."]);
    git(&source, &["commit", "-m", "freeze historical target"]);
    git(root, &["clone", "--bare", "source", "repo.git"]);
    let repo = root.join("repo.git");
    let base = git(&repo, &["rev-parse", MAIN]);
    declare_claim(
        root,
        ORIGIN,
        OLD_MODULE,
        "original",
        "Historical target",
        "x equals x",
    )?;
    worker_dispatch::bind(root, &repo, OLD_MODULE, &base)?;
    reviewed_spec::bind(root, &repo, ORIGIN, SPEC, SPEC_DECL, &base)?;
    let body = root.join("body.txt");
    fs::write(&body, "rfl")?;
    promote_managed(
        root, &repo, ORIGIN, OLD_MODULE, "original", &lean, &body, None,
    )?;
    let promoted = git(&repo, &["rev-parse", MAIN]);
    let state = StateStore::open_readonly(root.join(DB))?;
    let archive = {
        let locked = PromotionRepo::try_lock(&repo)?;
        wiki_archive::prepare(root, &locked, &state, ORIGIN, &promoted)?.0
    };
    wiki_archive::archive(root, &repo, ORIGIN, &promoted, &archive.approval_sha256)?;
    let original_comparison = state
        .lean_spec_comparison(&promoted)?
        .context("missing historical comparison")?;

    git(root, &["clone", "repo.git", "review"]);
    let review = root.join("review");
    // The old statement now means 3 = 3; the new frozen statement still means 2 = 2.
    fs::write(review.join("Defs.lean"), "def x : Nat := 3\n")?;
    fs::write(review.join(NEW_MODULE), FROZEN)?;
    fs::write(
        review.join("ai4math-modules.txt"),
        "TrustedSpec.lean\nDefs.lean\nProof.lean\nAdapt.lean\n",
    )?;
    git(&review, &["add", "-A"]);
    git(
        &review,
        &["commit", "-m", "review drift and freeze adaptation target"],
    );
    git(&review, &["push", "origin", "HEAD:refs/heads/main"]);
    let head = git(&repo, &["rev-parse", MAIN]);
    assert_ne!(head, promoted);
    StateStore::open(root.join(DB))?.mark_t3(
        &[ORIGIN],
        &[],
        Change {
            actor: "adaptation fixture",
            git_commit: Some(&head),
        },
    )?;
    declare_claim(
        root,
        TARGET,
        NEW_MODULE,
        "adapted",
        "Fresh target",
        "x minus one is two",
    )?;
    breaker_probes::enroll(
        root,
        &repo,
        "existing",
        TaskTrack::Infrastructure,
        OLD_MODULE,
        "original",
        &[],
    )?;
    assert_eq!(state.task_scheduler_status()?.infrastructure.ready, 1);
    assert!(state.claim(ORIGIN)?.context("missing source Claim")?.stale);
    assert_eq!(
        state
            .claim(TARGET)?
            .context("missing destination Claim")?
            .effective_trust,
        Trust::L0
    );

    let preview = |reviewed: &str, target: &str| {
        let locked = PromotionRepo::try_lock(&repo)?;
        prepare(root, &locked, &state, &archive.id, reviewed, target)
    };
    let record = |reviewed: &str, target: &str, digest: &str| {
        apply(root, &repo, &archive.id, reviewed, target, digest)
    };
    let unbound = Authority::read(root, &repo, &state)?;
    let unbound_seq = state.status()?.last_event_seq;
    let err = preview(&head, TARGET).unwrap_err();
    assert!(
        format!("{err:#}").contains("lacks a reviewed Lean specification"),
        "{err:#}"
    );
    assert_eq!(state.status()?.last_event_seq, unbound_seq);
    unbound.assert_unchanged(root, &repo, &state)?;

    reviewed_spec::bind(root, &repo, TARGET, SPEC, SPEC_DECL, &head)?;
    let before = Authority::read(root, &repo, &state)?;
    let seq = state.status()?.last_event_seq;
    for (reviewed, target, expected) in [
        (head.as_str(), ORIGIN, "distinct reviewed destination Claim"),
        (promoted.as_str(), TARGET, "not current main"),
    ] {
        let err = preview(reviewed, target).unwrap_err();
        assert!(format!("{err:#}").contains(expected), "{err:#}");
    }
    let plan = preview(&head, TARGET)?;
    let digest = plan.approval_sha256()?;
    assert_eq!(plan.archive_id, archive.id);
    assert_eq!(plan.origin_claim, ORIGIN);
    assert_eq!(plan.promoted_commit, promoted);
    assert_eq!(plan.reviewed_commit, head);
    assert_eq!(plan.target_claim, TARGET);
    assert_eq!(plan.target_module, NEW_MODULE);
    assert_eq!(plan.target_decl, "adapted");
    assert_eq!(
        plan.frozen_source_sha256,
        wiki_archive::sha(FROZEN.as_bytes())
    );
    assert_eq!(plan.spec_file, original_comparison.spec_file);
    assert_eq!(plan.spec_decl, original_comparison.spec_decl);
    assert_eq!(
        plan.spec_prefix_sha256,
        original_comparison.spec_prefix_sha256
    );
    assert_eq!(plan.expected_event_seq, seq);
    assert!(state.sediment_adaptation(&digest)?.is_none());
    assert_eq!(state.status()?.last_event_seq, seq);
    before.assert_unchanged(root, &repo, &state)?;

    let err = record(&head, TARGET, &"0".repeat(64)).unwrap_err();
    assert!(
        format!("{err:#}").contains("differs from current preview"),
        "{err:#}"
    );
    assert!(record(&promoted, TARGET, &digest).is_err());
    assert!(record(&head, ORIGIN, &digest).is_err());
    assert!(state.sediment_adaptation(&digest)?.is_none());
    assert_eq!(state.status()?.last_event_seq, seq);
    before.assert_unchanged(root, &repo, &state)?;

    let approval = record(&head, TARGET, &digest)?;
    assert_eq!(approval.approval_sha256, digest);
    assert_eq!(approval.plan, plan);
    assert_eq!(approval.event_seq, seq + 1);
    assert_eq!(state.sediment_adaptation(&digest)?, Some(approval.clone()));
    before.assert_unchanged(root, &repo, &state)?;

    // A duplicate may be rejected or return the same record, but must not write.
    if let Ok(duplicate) = record(&head, TARGET, &digest) {
        assert_eq!(duplicate, approval);
    }
    assert_eq!(state.sediment_adaptation(&digest)?, Some(approval));
    assert_eq!(state.status()?.last_event_seq, seq + 1);
    let events = state.events_after(seq, 2)?;
    assert_eq!(events.len(), 1);
    let event = &events[0];
    assert_eq!(event.event_type, "SEDIMENT_ADAPTATION_APPROVED");
    assert_eq!(event.entity_type, "claim");
    assert_eq!(event.entity_id, TARGET);
    assert_eq!(event.event_seq, seq + 1);
    assert_eq!(event.git_commit.as_deref(), Some(head.as_str()));
    assert_eq!(event.payload["approval_sha256"], digest);
    assert_eq!(event.payload["archive_id"], archive.id);
    before.assert_unchanged(root, &repo, &state)?;
    Ok(())
}
