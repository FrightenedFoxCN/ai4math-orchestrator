use super::*;
use crate::{declare_claim, init, managed_tests::git, promote_managed};
use ai4math_store::{Change, Trust};
use std::{env, fs, path::PathBuf};

fn project(root: &Path) -> Result<(PathBuf, String)> {
    init(root, Engine::Lean4)?;
    let working = root.join("work");
    fs::create_dir(&working)?;
    git(&working, &["init", "--initial-branch=main"]);
    fs::write(working.join("Defs.lean"), "def x : Nat := 2\n")?;
    fs::write(
        working.join("Proof.lean"),
        "import Defs\ntheorem target : x = 2 := {{PROOF_BODY}}\n",
    )?;
    fs::write(
        working.join("ai4math-modules.txt"),
        "Defs.lean\nProof.lean\n",
    )?;
    git(&working, &["add", "."]);
    git(&working, &["commit", "-m", "freeze proof"]);
    let old = git(&working, &["rev-parse", "HEAD"]);
    git(root, &["clone", "--bare", "work", "repo.git"]);
    declare_claim(root, "C", "Proof.lean", "target", "Target", "x = 2")?;
    Ok((root.join("repo.git"), old))
}

fn edit_defs(root: &Path, repo: &Path, source: &str, work_name: &str) -> Result<String> {
    git(root, &["clone", repo.to_str().unwrap(), work_name]);
    let work = root.join(work_name);
    fs::write(work.join("Defs.lean"), source)?;
    git(&work, &["add", "Defs.lean"]);
    git(&work, &["commit", "-m", "operator-reviewed structure"]);
    git(&work, &["push", "origin", "HEAD:refs/heads/main"]);
    Ok(git(&work, &["rev-parse", "HEAD"]))
}

fn inspect(root: &Path, repo: &Path, base: &str, head: &str) -> Result<Plan> {
    let locked = PromotionRepo::lock(repo)?;
    let mut state = StateStore::open_readonly(root.join(".runtime/state.sqlite"))?;
    plan(&mut state, &locked, "Proof.lean", base, head, Engine::Lean4)
}

#[test]
fn dry_run_is_deterministic_readonly_and_rejects_unrelated_git_input() -> Result<()> {
    let root = tempfile::tempdir()?;
    let (repo, base) = project(root.path())?;
    let new = edit_defs(root.path(), &repo, "def x : Nat := 3\n", "review")?;
    let result = inspect(root.path(), &repo, &base, &new)?;
    assert_eq!(result.changed_modules, ["Defs.lean"]);
    assert_eq!(result.affected_modules, ["Defs.lean", "Proof.lean"]);
    assert_eq!(result.claims[0].old_proof, ProofDisposition::NotL5);
    assert_eq!(
        result.plan_sha256,
        inspect(root.path(), &repo, &base, &new)?.plan_sha256
    );
    let state = StateStore::open_readonly(root.path().join(".runtime/state.sqlite"))?;
    assert_eq!(state.status()?.last_event_seq, result.ledger_event_seq);
    assert!(!state.claim("C")?.unwrap().stale);
    assert!(inspect(root.path(), &repo, &new, &base).is_err());
    assert!(inspect(root.path(), &repo, "HEAD", &new).is_err());
    Ok(())
}

#[test]
fn batch_options_require_a_complete_generation_cursor() -> Result<()> {
    assert_eq!(batch_options(&[])?, (None, None));
    assert_eq!(batch_options(&["/tool/data"])?, (Some("/tool/data"), None));
    assert_eq!(
        batch_options(&["--after", "C", "42"])?,
        (None, Some(("C", 42)))
    );
    assert_eq!(
        batch_options(&["/tool/data", "--after", "C", "42"])?,
        (Some("/tool/data"), Some(("C", 42)))
    );
    for args in [
        &["--after"][..],
        &["--after", "C"],
        &["--cursor", "C", "42"],
        &["--after", "C", "-1"],
        &["--after", "C", "+1"],
        &["--after", "C", "01"],
        &["--after", "", "1"],
        &["--after", "C", "9223372036854775808"],
    ] {
        assert!(batch_options(args).is_err(), "accepted {args:?}");
    }
    assert!(
        recheck_batch(
            Path::new("/unused"),
            Path::new("/unused"),
            "unused",
            9,
            Path::new("/unused"),
            &[]
        )
        .unwrap_err()
        .to_string()
        .contains("1..=8")
    );
    Ok(())
}

#[test]
fn t3_worklist_requires_applied_digest_and_current_main_without_state_writes() -> Result<()> {
    let root = tempfile::tempdir()?;
    let (repo, base) = project(root.path())?;
    crate::worker_dispatch::bind(root.path(), &repo, "Proof.lean", &base)?;
    let first = edit_defs(
        root.path(),
        &repo,
        "def x : Nat := Nat.succ 1\n",
        "plan-first",
    )?;
    let p = inspect(root.path(), &repo, &base, &first)?;
    assert!(worklist(root.path(), &repo, &p.plan_sha256, 1, None).is_err());
    apply(
        root.path(),
        &repo,
        "Proof.lean",
        &base,
        &first,
        &p.plan_sha256,
    )?;
    let path = root.path().join(".runtime/state.sqlite");
    let state = StateStore::open_readonly(&path)?;
    let seq = state.status()?.last_event_seq;
    worklist(root.path(), &repo, &p.plan_sha256, 1, None)?;
    assert!(worklist(root.path(), &repo, &p.plan_sha256, 0, None).is_err());
    assert!(worklist(root.path(), &repo, &p.plan_sha256, 1, Some("not-approved")).is_err());
    assert_eq!(state.status()?.last_event_seq, seq);
    edit_defs(root.path(), &repo, "def x : Nat := 3\n", "move-main")?;
    assert!(
        worklist(root.path(), &repo, &p.plan_sha256, 1, None)
            .unwrap_err()
            .to_string()
            .contains("no longer matches main")
    );
    assert_eq!(state.status()?.last_event_seq, seq);
    Ok(())
}

#[test]
#[cfg(target_os = "macos")]
#[ignore = "requires an outer-unsandboxed host and AI4MATH_TEST_LEAN"]
fn native_t3_uses_committed_sources_and_resumes_without_repeat_downgrades() -> Result<()> {
    let root = tempfile::tempdir()?;
    let (repo, _) = project(root.path())?;
    declare_claim(
        root.path(),
        "A",
        "Defs.lean",
        "x",
        "Unproved marker",
        "x is defined",
    )?;
    let file = root.path().join("body.txt");
    fs::write(&file, "by\n  rfl")?;
    let exe = PathBuf::from(env::var_os("AI4MATH_TEST_LEAN").context("set Lean executable")?);
    promote_managed(
        root.path(),
        &repo,
        "C",
        "Proof.lean",
        "target",
        &exe,
        &file,
        None,
    )?;
    let promoted = git(&repo, &["rev-parse", "refs/heads/main"]);
    let first = edit_defs(
        root.path(),
        &repo,
        "def x : Nat := Nat.succ 1\n",
        "review-one",
    )?;
    let preview = inspect(root.path(), &repo, &promoted, &first)?;
    assert!(matches!(
        preview
            .claims
            .iter()
            .find(|claim| claim.claim_id == "C")
            .unwrap()
            .old_proof,
        ProofDisposition::Ready { .. }
    ));
    let mut state = StateStore::open(root.path().join(".runtime/state.sqlite"))?;
    state.append_event(
        Change {
            actor: "fixture",
            git_commit: Some(&first),
        },
        "UNRELATED",
        "claim",
        "C",
        serde_json::json!({}),
    )?;
    assert!(
        apply(
            root.path(),
            &repo,
            "Proof.lean",
            &promoted,
            &first,
            &preview.plan_sha256
        )
        .is_err()
    );
    let preview = inspect(root.path(), &repo, &promoted, &first)?;
    apply(
        root.path(),
        &repo,
        "Proof.lean",
        &promoted,
        &first,
        &preview.plan_sha256,
    )?;
    assert!(state.claim("C")?.unwrap().stale);
    let seq = state.status()?.last_event_seq;
    let first_page = state.t3_worklist(&preview.plan_sha256, 1, None)?;
    assert_eq!(first_page.next_after_claim.as_deref(), Some("A"));
    let page = state.t3_worklist(&preview.plan_sha256, 1, Some("A"))?;
    assert_eq!(page.entries.len(), 1);
    assert_eq!(page.entries[0].claim_id, "C");
    assert_eq!(
        page.entries[0].status,
        ai4math_store::T3WorklistStatus::Pending
    );
    assert!(
        recheck_one(
            root.path(),
            &repo,
            &preview.plan_sha256,
            "out-of-scope",
            &exe,
            None
        )
        .is_err()
    );
    assert_eq!(state.status()?.last_event_seq, seq);
    recheck_batch(root.path(), &repo, &preview.plan_sha256, 1, &exe, &[])?;
    assert_eq!(state.status()?.last_event_seq, seq);
    assert!(state.claim("C")?.unwrap().stale);
    state.append_event(
        Change {
            actor: "fixture",
            git_commit: Some(&first),
        },
        "UNRELATED",
        "claim",
        "C",
        serde_json::json!({}),
    )?;
    let stale_cursor = seq.to_string();
    assert!(
        recheck_batch(
            root.path(),
            &repo,
            &preview.plan_sha256,
            1,
            &exe,
            &["--after", "A", &stale_cursor]
        )
        .is_err()
    );
    assert!(state.claim("C")?.unwrap().stale);
    let seq = state.status()?.last_event_seq;
    recheck_batch(root.path(), &repo, &preview.plan_sha256, 1, &exe, &[])?;
    assert_eq!(state.status()?.last_event_seq, seq);
    recheck_batch(
        root.path(),
        &repo,
        &preview.plan_sha256,
        1,
        &exe,
        &["--after", "A", &seq.to_string()],
    )?;
    assert_eq!(state.claim("C")?.unwrap().local_trust, Trust::L5);
    assert!(!state.claim("C")?.unwrap().stale);
    let page = state.t3_worklist(&preview.plan_sha256, 1, Some("A"))?;
    assert_eq!(
        page.entries[0].status,
        ai4math_store::T3WorklistStatus::Resolved
    );
    assert_eq!(
        state
            .events_after(seq, 32)?
            .iter()
            .filter(|event| event.event_type == "RECEIPT_RECORDED")
            .count(),
        1
    );
    let after = state.status()?.last_event_seq;
    recheck_one(root.path(), &repo, &preview.plan_sha256, "C", &exe, None)?;
    assert_eq!(state.status()?.last_event_seq, after);
    let second = edit_defs(root.path(), &repo, "def x : Nat := 3\n", "review-two")?;
    assert!(recheck(root.path(), &repo, &preview.plan_sha256, &exe, None).is_err());
    let next = inspect(root.path(), &repo, &first, &second)?;
    apply(
        root.path(),
        &repo,
        "Proof.lean",
        &first,
        &second,
        &next.plan_sha256,
    )?;
    recheck(root.path(), &repo, &next.plan_sha256, &exe, None)?;
    let c = state.claim("C")?.unwrap();
    assert_eq!(c.local_trust, Trust::L3);
    assert!(!c.stale);
    let seq = state.status()?.last_event_seq;
    apply(
        root.path(),
        &repo,
        "Proof.lean",
        &first,
        &second,
        &next.plan_sha256,
    )?;
    recheck(root.path(), &repo, &next.plan_sha256, &exe, None)?;
    assert_eq!(state.status()?.last_event_seq, seq);
    Ok(())
}

#[cfg(target_os = "macos")]
fn edit_from_main(
    root: &Path,
    repo: &Path,
    name: &str,
    changes: &[(&str, &str)],
) -> Result<String> {
    git(root, &["clone", repo.to_str().unwrap(), name]);
    let work = root.join(name);
    for (path, source) in changes {
        fs::write(work.join(path), source)?;
    }
    git(&work, &["add", "."]);
    git(
        &work,
        &["commit", "-m", "operator-reviewed definition change"],
    );
    git(&work, &["push", "origin", "HEAD:refs/heads/main"]);
    Ok(git(&work, &["rev-parse", "HEAD"]))
}

#[test]
#[cfg(target_os = "macos")]
#[ignore = "requires native Seatbelt and AI4MATH_TEST_LEAN"]
fn native_reviewed_spec_t3_restores_only_the_same_reviewed_proposition() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path();
    let (repo, _) = project(root)?;
    let approved = edit_from_main(
        root,
        &repo,
        "reviewed-input",
        &[
            ("TrustedSpec.lean", "def accepted : Prop := 2 = 2\n"),
            (
                "Proof.lean",
                "import Defs\ntheorem target : x = x := {{PROOF_BODY}}\n",
            ),
            (
                "ai4math-modules.txt",
                "TrustedSpec.lean\nDefs.lean\nProof.lean\n",
            ),
        ],
    )?;
    reviewed_spec::bind(root, &repo, "C", "TrustedSpec.lean", "accepted", &approved)?;
    let proof = root.join("reviewed-body.txt");
    fs::write(&proof, "rfl")?;
    let exe = PathBuf::from(env::var_os("AI4MATH_TEST_LEAN").context("set native Lean")?);
    promote_managed(root, &repo, "C", "Proof.lean", "target", &exe, &proof, None)?;
    let promoted = git(&repo, &["rev-parse", "refs/heads/main"]);
    let ledger = StateStore::open_readonly(root.join(".runtime/state.sqlite"))?;
    let t1 = ledger
        .lean_spec_comparison(&promoted)?
        .context("missing original comparison")?;

    let equivalent = edit_defs(root, &repo, "def x : Nat := Nat.succ 1\n", "equivalent")?;
    let plan = inspect(root, &repo, &promoted, &equivalent)?;
    assert!(matches!(&plan.claims[0].old_proof,
        ProofDisposition::Ready{reviewed_spec_prefix_sha256:Some(prefix),..} if *prefix==t1.spec_prefix_sha256));
    apply(
        root,
        &repo,
        "Proof.lean",
        &promoted,
        &equivalent,
        &plan.plan_sha256,
    )?;
    assert!(ledger.claim("C")?.unwrap().stale);
    assert!(ledger.lean_spec_recheck("C", &plan.plan_sha256)?.is_none());
    recheck(root, &repo, &plan.plan_sha256, &exe, None)?;
    let restored = ledger.claim("C")?.unwrap();
    assert_eq!(restored.local_trust, Trust::L5);
    assert!(!restored.stale);
    let t3 = ledger
        .lean_spec_recheck("C", &plan.plan_sha256)?
        .context("missing retained T3 comparison")?;
    assert_eq!(t3.reviewed_commit, equivalent);
    assert_eq!(t3.promoted_commit, promoted);
    assert_eq!(t3.spec_prefix_sha256, t1.spec_prefix_sha256);
    assert_ne!(t3.comparison_receipt_sha256, t1.receipt_sha256);
    let before_replay = ledger.status()?.last_event_seq;
    recheck(root, &repo, &plan.plan_sha256, &exe, None)?;
    reviewed_spec::read_recheck(root, "C", &plan.plan_sha256)?;
    assert_eq!(ledger.status()?.last_event_seq, before_replay);
    assert_eq!(git(&repo, &["rev-parse", "refs/heads/main"]), equivalent);

    // rfl still proves x = x, but at the new snapshot it is 3 = 3, not 2 = 2.
    let different = edit_defs(root, &repo, "def x : Nat := 3\n", "different")?;
    let different_plan = inspect(root, &repo, &equivalent, &different)?;
    apply(
        root,
        &repo,
        "Proof.lean",
        &equivalent,
        &different,
        &different_plan.plan_sha256,
    )?;
    let seq = ledger.status()?.last_event_seq;
    recheck(root, &repo, &different_plan.plan_sha256, &exe, None)?;
    assert_eq!(ledger.status()?.last_event_seq, seq);
    assert!(ledger.claim("C")?.unwrap().stale);
    assert!(
        ledger
            .lean_spec_recheck("C", &different_plan.plan_sha256)?
            .is_none()
    );

    // Even byte changes preserving the proposition cannot implicitly reapprove the spec.
    let changed = edit_from_main(
        root,
        &repo,
        "changed-spec",
        &[
            (
                "TrustedSpec.lean",
                "def accepted : Prop := Nat.succ 1 = 2\n",
            ),
            ("Defs.lean", "def x : Nat := 2\n"),
        ],
    )?;
    let changed_plan = inspect(root, &repo, &different, &changed)?;
    assert_eq!(
        changed_plan.claims[0].old_proof,
        ProofDisposition::ReviewedSpecChangedOrMissing
    );
    apply(
        root,
        &repo,
        "Proof.lean",
        &different,
        &changed,
        &changed_plan.plan_sha256,
    )?;
    let seq = ledger.status()?.last_event_seq;
    recheck(root, &repo, &changed_plan.plan_sha256, &exe, None)?;
    assert_eq!(ledger.status()?.last_event_seq, seq);
    assert!(ledger.claim("C")?.unwrap().stale);
    assert!(
        ledger
            .lean_spec_recheck("C", &changed_plan.plan_sha256)?
            .is_none()
    );
    assert_eq!(git(&repo, &["rev-parse", "refs/heads/main"]), changed);
    Ok(())
}
