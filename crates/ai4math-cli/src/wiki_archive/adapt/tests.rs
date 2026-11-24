#![cfg(target_os = "macos")]

use super::{apply, prepare};
use crate::{
    breaker_probes, declare_claim, init, managed_tests::git, promote_managed,
    promotion::PromotionRepo, reviewed_spec, wiki_archive, worker_dispatch,
};
use ai4math_core::{Engine, FrozenTarget, ProofBodySubmission, render_proof_body};
use ai4math_store::{
    Change, StateStore, TaskCandidateSource, TaskSchedulerStatus, TaskTrack, Trust,
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
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
const HISTORICAL_BODY: &str = "Eq.refl x";
const NEW_BODY: &str = "rfl";

// Like the ordinary worker fixture, this native executable receives only a
// bounded request on stdin and emits a deterministic, untrusted suggestion.
const ADAPTATION_WORKER: &str = r#"
#include <stdio.h>
#include <string.h>

static int in_object(const char *object, const char *fragment) {
    const char *end = strchr(object, '}');
    const char *field = strstr(object, fragment);
    return end && field && field < end;
}

int main(void) {
    char input[16384] = {0};
    size_t n = fread(input, 1, sizeof input - 1, stdin);
    if (n == 0 || n == sizeof input - 1 || ferror(stdin) ||
        strstr(input, "state.sqlite") || strstr(input, "fence")) return 31;
    if (!strstr(input, TASK_ID) || !strstr(input, BASE_COMMIT) ||
        !strstr(input, CLAIM_ID) || !strstr(input, MODULE_PATH) ||
        !strstr(input, "\"role\":\"prover_worker\"") ||
        !strstr(input, "\"target_decl\":\"adapted\"")) return 32;
    const char *frozen = strstr(input, "\"frozen_target\":{");
    if (!frozen || !in_object(frozen, FROZEN_PREFIX) ||
        !in_object(frozen, FROZEN_SUFFIX) || !in_object(frozen, FROZEN_PATH) ||
        !in_object(frozen, "\"target_id\":\"adapted\"")) return 33;
    const char *reference = strstr(input, "\"adaptation_reference\":{");
    if (!reference || !in_object(reference, ARCHIVE_ID) ||
        !in_object(reference, ORIGIN_CLAIM) || !in_object(reference, VERIFIED_COMMIT) ||
        !in_object(reference, HISTORICAL_BODY) ||
        !in_object(reference, "\"current_proof_authority\":false")) return 34;
    puts(RESULT);
    return 0;
}
"#;

fn suggestion() -> Value {
    json!({"kind": "proof_body", "proof_body": NEW_BODY})
}

fn fixture_worker(
    root: &Path,
    task_id: &str,
    reviewed: &str,
    archive_id: &str,
    promoted: &str,
) -> Result<(PathBuf, String)> {
    let src = root.join("adaptation-worker.c");
    let exe = root.join("adaptation-worker");
    fs::write(&src, ADAPTATION_WORKER)?;
    let (prefix, suffix) = FROZEN.split_once("{{PROOF_BODY}}").unwrap();
    let mut compiler = std::process::Command::new("/usr/bin/clang");
    // All fixture values are ASCII; JSON string quoting here also gives the
    // required C string escaping for quotes and the frozen module newlines.
    for (define, field, value) in [
        ("TASK_ID", "task_id", task_id),
        ("BASE_COMMIT", "base_commit", reviewed),
        ("CLAIM_ID", "claim_id", TARGET),
        ("MODULE_PATH", "module_path", NEW_MODULE),
        ("FROZEN_PATH", "file_path", NEW_MODULE),
        ("FROZEN_PREFIX", "prefix", prefix),
        ("FROZEN_SUFFIX", "suffix", suffix),
        ("ARCHIVE_ID", "archive_id", archive_id),
        ("ORIGIN_CLAIM", "origin_claim", ORIGIN),
        ("VERIFIED_COMMIT", "verified_at_commit", promoted),
        ("HISTORICAL_BODY", "historical_proof_body", HISTORICAL_BODY),
    ] {
        let member = format!("\"{field}\":{}", serde_json::to_string(value)?);
        compiler.arg(format!("-D{define}={}", serde_json::to_string(&member)?));
    }
    compiler.arg(format!(
        "-DRESULT={}",
        serde_json::to_string(&serde_json::to_string(&suggestion())?)?
    ));
    ensure!(
        compiler
            .args(["-Wall", "-Wextra", "-Werror", "-o"])
            .arg(&exe)
            .arg(&src)
            .status()?
            .success(),
        "could not build native adaptation worker fixture"
    );
    let sha = wiki_archive::sha(&fs::read(&exe)?);
    Ok((exe, sha))
}

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
        self.assert_proof_state_unchanged(&after);
        assert_eq!(self.scheduler, after.scheduler, "changed task scheduler");
        Ok(())
    }

    fn assert_proof_state_unchanged(&self, after: &Self) {
        assert_eq!(self.claims, after.claims, "changed Claim state or trust");
        assert!(self.git == after.git, "changed bare Git repository");
        assert!(self.wiki == after.wiki, "changed Wiki tree or bytes");
    }
}

#[test]
#[ignore = "requires native macOS Seatbelt, clang and absolute AI4MATH_TEST_LEAN"]
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
    fs::write(&body, HISTORICAL_BODY)?;
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
    assert_eq!(
        state.sediment_adaptation(&digest)?.as_ref(),
        Some(&approval)
    );
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

    let task_id = StateStore::sediment_adaptation_task_id(&digest)?;
    let (exe, sha) = fixture_worker(root, &task_id, &head, &archive.id, &promoted)?;
    let dispatch =
        |digest: &str| worker_dispatch::run_adaptation(root, &repo, digest, &exe, &sha, 5);
    let assert_refused = |digest: &str, expected: &str| -> Result<()> {
        let authority = Authority::read(root, &repo, &state)?;
        let event_seq = state.status()?.last_event_seq;
        let err = dispatch(digest).unwrap_err();
        assert!(format!("{err:#}").contains(expected), "{err:#}");
        assert_eq!(state.status()?.last_event_seq, event_seq);
        assert!(state.task_candidate(&task_id)?.is_none());
        authority.assert_unchanged(root, &repo, &state)
    };
    assert_refused(
        "not-a-digest",
        "invalid sediment adaptation approval SHA-256",
    )?;
    assert_refused(&"0".repeat(64), "no retained sediment adaptation approval")?;

    // Even identical module bytes on a different main OID cannot lease work.
    git(
        &review,
        &["commit", "--allow-empty", "-m", "advance main only"],
    );
    git(&review, &["push", "origin", "HEAD:refs/heads/main"]);
    let moved = git(&repo, &["rev-parse", MAIN]);
    assert_ne!(moved, head);
    assert_refused(&digest, "not current main")?;
    git(&repo, &["update-ref", MAIN, &head, &moved]);

    let dispatch_before = Authority::read(root, &repo, &state)?;
    let dispatch_seq = state.status()?.last_event_seq;
    assert!(state.lean_spec_comparison(&head)?.is_none());
    assert!(state.managed_proof_recipe(TARGET)?.is_none());
    dispatch(&digest)?;
    let candidate = state
        .task_candidate(&task_id)?
        .context("missing retained adaptation candidate")?;
    assert_eq!(candidate.task_id, task_id);
    assert_eq!(candidate.track, TaskTrack::Infrastructure);
    assert_eq!(candidate.source, TaskCandidateSource::NativeWorker);
    assert_eq!(candidate.role, "prover_worker");
    assert_eq!(candidate.executable_sha256.as_deref(), Some(sha.as_str()));
    assert_eq!(candidate.base_commit, head);
    assert_eq!(candidate.claim_id, TARGET);
    assert_eq!(candidate.module_path, NEW_MODULE);
    assert_eq!(candidate.target, "adapted");
    assert_eq!(candidate.engine, Engine::Lean4.as_str());
    assert_eq!(
        serde_json::from_str::<Value>(&candidate.result_json)?,
        suggestion()
    );
    assert!(state.task_candidate("existing")?.is_none());
    assert!(state.lean_spec_comparison(&head)?.is_none());
    assert!(state.managed_proof_recipe(TARGET)?.is_none());
    assert!(state.promotion_intent()?.is_none());
    assert_eq!(
        state.sediment_adaptation(&digest)?.as_ref(),
        Some(&approval)
    );

    let after = Authority::read(root, &repo, &state)?;
    dispatch_before.assert_proof_state_unchanged(&after);
    let mut expected_scheduler = dispatch_before.scheduler;
    expected_scheduler.infrastructure.completed += 1;
    assert_eq!(after.scheduler, expected_scheduler);
    let events = state.events_after(dispatch_seq, 4)?;
    assert_eq!(events.len(), 3);
    for (event, kind) in events
        .iter()
        .zip(["TASK_REGISTERED", "TASK_LEASED", "TASK_COMPLETED"])
    {
        assert_eq!(event.event_type, kind);
        assert_eq!(event.entity_type, "task");
        assert_eq!(event.entity_id, task_id);
    }
    assert_eq!(events[0].payload["approval_sha256"], digest);
    assert_eq!(events[1].payload["approval_sha256"], digest);
    assert_eq!(events[2].payload["untrusted_candidate"], true);

    // A successful scoped lease records at most one suggestion for this approval.
    let complete_seq = state.status()?.last_event_seq;
    let _ = dispatch(&digest);
    assert_eq!(state.status()?.last_event_seq, complete_seq);
    assert_eq!(state.task_candidate(&task_id)?.as_ref(), Some(&candidate));
    after.assert_unchanged(root, &repo, &state)?;

    // An approved head and retained worker candidate still provide no proof.
    let retain_lineage = |approval: &str, reviewed: &str| {
        wiki_archive::lineage::retain(root, &repo, approval, reviewed)
    };
    assert!(state.sediment_lineage(&digest)?.is_none());
    let err = retain_lineage(&digest, &head).unwrap_err();
    assert!(
        format!("{err:#}").contains("no exact managed promotion for the adaptation target"),
        "{err:#}"
    );
    assert!(state.sediment_lineage(&digest)?.is_none());
    assert_eq!(state.status()?.last_event_seq, complete_seq);
    assert_eq!(state.task_candidate(&task_id)?.as_ref(), Some(&candidate));
    after.assert_unchanged(root, &repo, &state)?;

    // A distinct operator step must now run the real Lean comparator against
    // the destination's reviewed spec, not replay the historical comparison.
    let origin_blob = git(&repo, &["show", &format!("{MAIN}:{OLD_MODULE}")]);
    worker_dispatch::promote(root, &repo, &task_id, &lean, None)?;
    let adapted = git(&repo, &["rev-parse", MAIN]);
    assert_ne!(adapted, head);
    assert_eq!(git(&repo, &["rev-parse", &format!("{MAIN}^")]), head);
    assert_eq!(
        git(&repo, &["show", &format!("{MAIN}:{OLD_MODULE}")]),
        origin_blob
    );
    let (prefix, suffix) = FROZEN.split_once("{{PROOF_BODY}}").unwrap();
    let rendered = render_proof_body(
        &FrozenTarget {
            engine: Engine::Lean4,
            target_id: "adapted".into(),
            file_path: NEW_MODULE.into(),
            prefix: prefix.into(),
            suffix: suffix.into(),
        },
        &ProofBodySubmission {
            target_id: "adapted".into(),
            proof_body: NEW_BODY.into(),
        },
    )?;
    assert_eq!(
        git(&repo, &["show", &format!("{MAIN}:{NEW_MODULE}")]),
        rendered.trim()
    );
    let target = state.claim(TARGET)?.context("missing destination Claim")?;
    assert_eq!(target.local_trust, Trust::L5);
    assert_eq!(target.effective_trust, Trust::L5);
    assert!(state.claim(ORIGIN)?.context("missing source Claim")?.stale);
    let comparison = state
        .lean_spec_comparison(&adapted)?
        .context("missing destination Lean comparison")?;
    assert_eq!(comparison.claim_id, TARGET);
    assert_eq!(comparison.base_commit, head);
    assert_eq!(comparison.spec_file, original_comparison.spec_file);
    assert_eq!(comparison.spec_decl, original_comparison.spec_decl);
    assert_eq!(
        comparison.spec_prefix_sha256,
        original_comparison.spec_prefix_sha256
    );
    assert_eq!(
        state.lean_spec_comparison(&promoted)?.as_ref(),
        Some(&original_comparison)
    );

    // Only the exact newly promoted main may anchor this ledger-only link.
    let retain_before = Authority::read(root, &repo, &state)?;
    let retain_seq = state.status()?.last_event_seq;
    assert!(state.sediment_lineage(&digest)?.is_none());
    let lineage = retain_lineage(&digest, &adapted)?;
    assert_eq!(lineage.approval_sha256, digest);
    assert_eq!(lineage.archive_id, archive.id);
    assert_eq!(lineage.target_claim, TARGET);
    assert_eq!(lineage.promoted_commit, adapted);
    assert_eq!(lineage.core_entry_id, format!("CORE-ADAPT-{digest}"));
    assert!(lineage.core_entry_prospective);
    assert_eq!(lineage.task_id, task_id);
    assert_eq!(
        lineage.origin_t1_receipt_sha256,
        original_comparison.candidate_receipt_sha256
    );
    assert_eq!(
        lineage.origin_comparison_receipt_sha256,
        original_comparison.receipt_sha256
    );
    assert_eq!(
        lineage.candidate_receipt_sha256,
        comparison.candidate_receipt_sha256
    );
    assert_eq!(lineage.comparison_receipt_sha256, comparison.receipt_sha256);
    assert_eq!(lineage.event_seq, retain_seq + 1);
    assert_eq!(state.status()?.last_event_seq, retain_seq + 1);
    assert_eq!(state.sediment_lineage(&digest)?.as_ref(), Some(&lineage));
    retain_before.assert_unchanged(root, &repo, &state)?;

    // A previously retained digest must not bypass the reviewed-head guard.
    let err = retain_lineage(&digest, &head).unwrap_err();
    assert!(format!("{err:#}").contains("not current main"), "{err:#}");
    let unknown = "0".repeat(64);
    let err = retain_lineage(&unknown, &adapted).unwrap_err();
    assert!(
        format!("{err:#}").contains("no retained sediment adaptation approval"),
        "{err:#}"
    );
    assert!(state.sediment_lineage(&unknown)?.is_none());
    assert_eq!(retain_lineage(&digest, &adapted)?, lineage);
    assert_eq!(state.sediment_lineage(&digest)?.as_ref(), Some(&lineage));
    assert_eq!(state.status()?.last_event_seq, retain_seq + 1);
    let events = state.events_after(retain_seq, 2)?;
    assert_eq!(events.len(), 1);
    let event = &events[0];
    assert_eq!(event.event_type, "SEDIMENT_ADAPTATION_LINEAGE_RETAINED");
    assert_eq!(event.event_seq, lineage.event_seq);
    assert_eq!(event.entity_type, "claim");
    assert_eq!(event.entity_id, TARGET);
    assert_eq!(event.git_commit.as_deref(), Some(adapted.as_str()));
    assert_eq!(event.payload["approval_sha256"], digest);
    assert_eq!(event.payload["archive_id"], archive.id);
    assert_eq!(event.payload["core_entry_id"], lineage.core_entry_id);
    assert_eq!(state.task_candidate(&task_id)?.as_ref(), Some(&candidate));
    assert!(state.task_candidate("existing")?.is_none());
    assert_eq!(
        state.sediment_adaptation(&digest)?.as_ref(),
        Some(&approval)
    );
    assert_eq!(
        state.lean_spec_comparison(&adapted)?.as_ref(),
        Some(&comparison)
    );
    retain_before.assert_unchanged(root, &repo, &state)?;
    Ok(())
}
