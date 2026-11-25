#![cfg(unix)]

use super::*;
use crate::{declare_claim, init, managed_tests::git, wiki_archive};
use ai4math_store::StateStore;
use serde_json::Value;
use std::{collections::BTreeMap, fs, path::PathBuf};

const CLAIM: &str = "C";
const DEFS: &str = "Defs.lean";
const TARGET: &str = "Proof.lean";
const SPEC: &str = "TrustedSpec.lean";
const MAIN: &str = "refs/heads/main";

struct Fixture<'a> {
    root: &'a Path,
    repo: PathBuf,
    base: String,
}

impl<'a> Fixture<'a> {
    fn new(root: &'a Path, with_spec: bool) -> Result<Self> {
        init(root, Engine::Lean4)?;
        let source = root.join("source");
        fs::create_dir(&source)?;
        git(&source, &["init", "--initial-branch=main"]);
        fs::write(source.join(DEFS), "def x : Nat := 2\n")?;
        fs::write(
            source.join(TARGET),
            "import Defs\ntheorem target : x = x := {{PROOF_BODY}}\n",
        )?;
        let manifest = if with_spec {
            // Keep Defs after the reviewed-spec prefix so it can change independently.
            fs::write(source.join(SPEC), "def approved : Prop := 2 = 2\n")?;
            "TrustedSpec.lean\nDefs.lean\nProof.lean\n"
        } else {
            "Defs.lean\nProof.lean\n"
        };
        fs::write(source.join("ai4math-modules.txt"), manifest)?;
        git(&source, &["add", "."]);
        git(
            &source,
            &["commit", "-m", "freeze the sediment replay fixture"],
        );
        git(root, &["clone", "--bare", "source", "repo.git"]);
        let repo = root.join("repo.git");
        let base = git(&repo, &["rev-parse", MAIN]);
        declare_claim(
            root,
            CLAIM,
            TARGET,
            "target",
            "Fixture theorem",
            "x equals x",
        )?;
        crate::worker_dispatch::bind(root, &repo, TARGET, &base)?;
        if with_spec {
            crate::reviewed_spec::bind(root, &repo, CLAIM, SPEC, "approved", &base)?;
        }
        Ok(Self { root, repo, base })
    }

    fn advance_main(&self, edits: &[(&str, &str)]) -> Result<String> {
        let review = self.root.join("review");
        if !review.exists() {
            git(self.root, &["clone", "repo.git", "review"]);
        }
        for (file, contents) in edits {
            fs::write(review.join(file), contents)?;
        }
        git(&review, &["add", "-A"]);
        git(
            &review,
            &["commit", "--allow-empty", "-m", "reviewed fixture edit"],
        );
        git(&review, &["push", "origin", "HEAD:refs/heads/main"]);
        Ok(git(&self.repo, &["rev-parse", MAIN]))
    }

    fn store(&self) -> Result<StateStore> {
        StateStore::open_readonly(self.root.join(".runtime/state.sqlite"))
    }

    fn missing_lean(&self) -> PathBuf {
        self.root.join("nonexistent-lean")
    }

    fn observe(&self, id: &str, reviewed: &str, lean: &Path) -> Result<Result<Report>> {
        let before = self.snapshot()?;
        let result = run(self.root, &self.repo, id, reviewed, lean);
        let after = self.snapshot()?;
        assert_eq!(before.seq, after.seq, "changed ledger event sequence");
        before.assert_same_authority(&after);
        Ok(result)
    }

    fn snapshot(&self) -> Result<Observation> {
        let state = self.store()?;
        let mut wiki = BTreeMap::new();
        snapshot_tree(&self.root.join(".mathwiki"), &mut wiki)?;
        Ok(Observation {
            seq: state.status()?.last_event_seq,
            claim: serde_json::to_value(state.claim(CLAIM)?)?,
            refs: git(
                &self.repo,
                &["for-each-ref", "--format=%(refname) %(objectname)"],
            ),
            wiki,
        })
    }

    #[cfg(target_os = "macos")]
    fn promote_and_archive(&self, lean: &Path) -> Result<(String, String)> {
        let body = self.root.join("body.txt");
        fs::write(&body, "rfl")?;
        crate::promote_managed(
            self.root, &self.repo, CLAIM, TARGET, "target", lean, &body, None,
        )?;
        let promoted = git(&self.repo, &["rev-parse", MAIN]);
        let (preview, _) = {
            let locked = PromotionRepo::try_lock(&self.repo)?;
            wiki_archive::prepare(self.root, &locked, &self.store()?, CLAIM, &promoted)?
        };
        wiki_archive::archive(
            self.root,
            &self.repo,
            CLAIM,
            &promoted,
            &preview.approval_sha256,
        )?;
        Ok((promoted, preview.id))
    }

    #[cfg(target_os = "macos")]
    fn mark_claim_stale(&self, head: &str) -> Result<()> {
        StateStore::open(self.root.join(".runtime/state.sqlite"))?.mark_t3(
            &[CLAIM],
            &[],
            ai4math_store::Change {
                actor: "replay fixture",
                git_commit: Some(head),
            },
        )?;
        assert!(
            self.store()?
                .claim(CLAIM)?
                .context("missing fixture Claim")?
                .stale
        );
        Ok(())
    }
}

struct Observation {
    seq: i64,
    claim: Value,
    refs: String,
    wiki: BTreeMap<PathBuf, Option<Vec<u8>>>,
}

impl Observation {
    fn assert_same_authority(&self, after: &Self) {
        assert_eq!(self.claim, after.claim, "changed Claim state");
        assert_eq!(self.refs, after.refs, "changed repository refs");
        assert!(self.wiki == after.wiki, "changed Wiki tree or bytes");
    }
}

fn snapshot_tree(root: &Path, tree: &mut BTreeMap<PathBuf, Option<Vec<u8>>>) -> Result<()> {
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        let kind = entry.file_type()?;
        if kind.is_dir() {
            tree.insert(path.clone(), None);
            snapshot_tree(&path, tree)?;
        } else {
            assert!(kind.is_file(), "unexpected fixture Wiki entry: {path:?}");
            tree.insert(path.clone(), Some(fs::read(path)?));
        }
    }
    Ok(())
}

#[test]
fn recheck_hard_errors_for_stale_review_or_unbacked_archive_without_writes() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let fixture = Fixture::new(dir.path(), false)?;
    let advanced = fixture.advance_main(&[])?;
    let source = "theorem target : True := by trivial\n";
    let unbacked = ai4math_store::render_sediment(ai4math_store::SedimentInput {
        claim_id: CLAIM,
        engine: "lean4",
        verified_at_commit: &fixture.base,
        source_module: TARGET,
        target_decl: "target",
        source,
        source_sha256: &wiki_archive::sha(source.as_bytes()),
        dependency_hashes: &BTreeMap::new(),
        dependency_order: &[],
    })?;
    ai4math_store::publish_sediment(fixture.root, &unbacked)?;
    for (reviewed, message) in [
        (&fixture.base, "reviewed archive base is not current main"),
        (&advanced, "no exact managed promotion"),
    ] {
        let error = fixture
            .observe(unbacked.id(), reviewed, &fixture.missing_lean())?
            .unwrap_err();
        assert!(format!("{error:#}").contains(message), "{error:#}");
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn native_lean() -> Result<PathBuf> {
    Ok(PathBuf::from(
        std::env::var_os("AI4MATH_TEST_LEAN").context("set native Lean")?,
    ))
}

#[cfg(target_os = "macos")]
fn assert_report(
    report: &Report,
    expected: Status,
    reason: &str,
    archive_id: &str,
    head: &str,
) -> Result<()> {
    assert_eq!(report.status, expected, "{report:?}");
    let json = serde_json::to_value(report)?;
    assert_eq!(json["reason"], reason);
    assert_eq!(json["persisted"], false);
    assert!(report.observation.is_none());
    assert_eq!(json["current_proof_authority"], false);
    assert_eq!(json["preview"]["archive_id"], archive_id);
    assert_eq!(json["preview"]["current_main"], head);
    assert_eq!(json["preview"]["current_proof_authority"], false);
    if expected != Status::Verified {
        assert!(report.candidate_receipt_sha256.is_none());
        assert!(report.comparison_receipt_sha256.is_none());
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn digest(value: &Option<String>) -> &str {
    let digest = value.as_deref().expect("missing native receipt digest");
    assert_eq!(digest.len(), 64);
    assert!(
        digest
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    );
    digest
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "requires native macOS Seatbelt and absolute AI4MATH_TEST_LEAN"]
fn native_ordinary_recheck_verifies_or_defers_without_changing_authority() -> Result<()> {
    let lean = native_lean()?;
    let dir = tempfile::tempdir()?;
    let fixture = Fixture::new(dir.path(), false)?;
    let (promoted, id) = fixture.promote_and_archive(&lean)?;
    let original = fixture
        .store()?
        .claim(CLAIM)?
        .context("missing promoted Claim")?
        .receipt_id
        .context("missing original promotion receipt")?;
    let reviewed = fixture.advance_main(&[])?;
    assert_ne!(reviewed, promoted);
    fixture.mark_claim_stale(&reviewed)?;
    let missing = fixture.missing_lean();
    let unavailable = fixture.observe(&id, &reviewed, &missing)??;
    assert_report(
        &unavailable,
        Status::Unavailable,
        "lean_toolchain_unavailable",
        &id,
        &reviewed,
    )?;
    let verified = fixture.observe(&id, &reviewed, &lean)??;
    assert_report(
        &verified,
        Status::Verified,
        "replayed_in_isolation",
        &id,
        &reviewed,
    )?;
    assert_ne!(digest(&verified.candidate_receipt_sha256), original);
    assert!(verified.comparison_receipt_sha256.is_none());

    let before = fixture.snapshot()?;
    let retained = run_with_retention(fixture.root, &fixture.repo, &id, &reviewed, &lean, true)?;
    assert_eq!(retained.status, Status::Verified);
    assert!(retained.persisted && !retained.current_proof_authority);
    let observation = retained
        .observation
        .context("missing ordinary retained observation")?;
    assert!(observation.comparison_receipt_sha256.is_none());
    assert_eq!(
        fixture.store()?.sediment_replay(&observation.id)?,
        Some(observation.clone())
    );
    super::result(fixture.root, &observation.id)?;
    let after = fixture.snapshot()?;
    assert_eq!(after.seq, before.seq + 1);
    before.assert_same_authority(&after);

    let changed = fixture.advance_main(&[(DEFS, "def x : Nat := Nat.succ 1\n")])?;
    let deferred = fixture.observe(&id, &changed, &missing)??;
    assert_report(
        &deferred,
        Status::Deferred,
        "unbound_prefix_changed",
        &id,
        &changed,
    )?;
    let stale = fixture.observe(&id, &reviewed, &missing)?.unwrap_err();
    assert!(
        format!("{stale:#}").contains("reviewed archive base is not current main"),
        "{stale:#}"
    );
    Ok(())
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "requires native macOS Seatbelt and absolute AI4MATH_TEST_LEAN"]
fn native_reviewed_spec_recheck_compares_against_unchanged_spec_prefix() -> Result<()> {
    let lean = native_lean()?;
    let dir = tempfile::tempdir()?;
    let fixture = Fixture::new(dir.path(), true)?;
    let (promoted, id) = fixture.promote_and_archive(&lean)?;
    let store = fixture.store()?;
    let original = store
        .lean_spec_comparison(&promoted)?
        .context("missing original comparison")?;
    let equivalent = fixture.advance_main(&[(DEFS, "def x : Nat := Nat.succ 1\n")])?;
    fixture.mark_claim_stale(&equivalent)?;
    let verified = fixture.observe(&id, &equivalent, &lean)??;
    assert_report(
        &verified,
        Status::Verified,
        "replayed_in_isolation",
        &id,
        &equivalent,
    )?;
    assert_ne!(
        digest(&verified.candidate_receipt_sha256),
        original.candidate_receipt_sha256
    );
    assert_ne!(
        digest(&verified.comparison_receipt_sha256),
        original.receipt_sha256
    );
    assert!(store.lean_spec_comparison(&equivalent)?.is_none());

    // rfl still proves x = x, but at this head it is no longer the reviewed 2 = 2.
    let different = fixture.advance_main(&[(DEFS, "def x : Nat := 3\n")])?;
    let deferred = fixture.observe(&id, &different, &lean)??;
    assert_report(
        &deferred,
        Status::Deferred,
        "reviewed_spec_comparison_not_confirmed",
        &id,
        &different,
    )?;
    assert!(store.lean_spec_comparison(&different)?.is_none());

    // Even definitionally equal changes to the approved prefix require a new review.
    let spec_edited = fixture.advance_main(&[(SPEC, "def approved : Prop := Nat.succ 1 = 2\n")])?;
    let deferred = fixture.observe(&id, &spec_edited, &fixture.missing_lean())??;
    assert_report(
        &deferred,
        Status::Deferred,
        "current_reviewed_spec_or_prefix_changed",
        &id,
        &spec_edited,
    )?;
    assert_eq!(store.lean_spec_comparison(&promoted)?, Some(original));
    Ok(())
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "requires native macOS Seatbelt and absolute AI4MATH_TEST_LEAN"]
fn native_reviewed_spec_recheck_retains_only_verified_observations() -> Result<()> {
    let lean = native_lean()?;
    let dir = tempfile::tempdir()?;
    let fixture = Fixture::new(dir.path(), true)?;
    let (promoted, id) = fixture.promote_and_archive(&lean)?;
    let store = fixture.store()?;
    let original = store
        .lean_spec_comparison(&promoted)?
        .context("missing original comparison")?;

    // The proof still checks with x = 3, but it no longer matches the reviewed spec.
    let different = fixture.advance_main(&[(DEFS, "def x : Nat := 3\n")])?;
    fixture.mark_claim_stale(&different)?;
    let before = fixture.snapshot()?;
    let deferred = run_with_retention(fixture.root, &fixture.repo, &id, &different, &lean, true)?;
    assert_report(
        &deferred,
        Status::Deferred,
        "reviewed_spec_comparison_not_confirmed",
        &id,
        &different,
    )?;
    let after = fixture.snapshot()?;
    assert_eq!(after.seq, before.seq, "retained a failed replay");
    before.assert_same_authority(&after);
    assert!(store.lean_spec_comparison(&different)?.is_none());

    let reviewed = fixture.advance_main(&[(DEFS, "def x : Nat := Nat.succ 1\n")])?;
    let before = fixture.snapshot()?;
    assert_eq!(before.claim["stale"], true);
    assert!(before.claim["receipt_id"].is_string());
    let report = run_with_retention(fixture.root, &fixture.repo, &id, &reviewed, &lean, true)?;
    assert_eq!(report.status, Status::Verified, "{report:?}");
    assert!(report.persisted);
    assert!(!report.current_proof_authority);
    assert!(!report.preview.current_proof_authority);
    let observation = report
        .observation
        .as_ref()
        .context("missing retained observation")?;
    let retained = store
        .sediment_replay(&observation.id)?
        .context("missing persisted observation")?;
    assert_eq!(
        serde_json::to_value(observation)?,
        serde_json::to_value(&retained)?
    );
    assert_eq!(retained.archive_id, id);
    assert_eq!(retained.claim_id, CLAIM);
    assert_eq!(retained.promoted_commit, promoted);
    assert_eq!(retained.reviewed_commit, reviewed);
    assert_eq!(
        retained.candidate_receipt_sha256,
        digest(&report.candidate_receipt_sha256)
    );
    assert_eq!(
        retained.comparison_receipt_sha256.as_deref(),
        Some(digest(&report.comparison_receipt_sha256))
    );
    assert_ne!(
        retained.candidate_receipt_sha256,
        original.candidate_receipt_sha256
    );
    assert_ne!(
        retained.comparison_receipt_sha256.as_deref(),
        Some(original.receipt_sha256.as_str())
    );

    let after = fixture.snapshot()?;
    assert_eq!(after.seq, before.seq + 1);
    before.assert_same_authority(&after);
    let events = store.events_after(before.seq, 2)?;
    assert_eq!(events.len(), 1);
    let event = &events[0];
    assert_eq!(event.event_seq, retained.event_seq);
    assert_eq!(event.event_type, "SEDIMENT_REPLAY_RETAINED");
    assert_eq!(event.entity_id, CLAIM);
    assert_eq!(event.git_commit.as_deref(), Some(reviewed.as_str()));
    assert_eq!(event.payload["id"], retained.id);
    assert!(store.lean_spec_comparison(&reviewed)?.is_none());
    assert_eq!(store.lean_spec_comparison(&promoted)?, Some(original));
    Ok(())
}
