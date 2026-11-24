use super::*;
use ai4math_core::Engine;
use ai4math_store::Change;
use std::process::{Command, Stdio};

const SOURCE: &str = "prelude\nnamespace N\ntheorem x {P : Prop} : P → P := fun h => h\ntheorem y {P : Prop} : P → P := fun h => h\nend N\n";
const POLICY: &[u8] = br#"[{"target_module":"A.lean","declaration":"N.x"},{"target_module":"A.lean","declaration":"N.y"}]"#;

fn git(repo: &Path, args: &[&str]) -> Result<String> {
    let run = Command::new("/usr/bin/git")
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .current_dir(repo)
        .args([
            "-c",
            "user.name=Readback Fixture",
            "-c",
            "user.email=fixture@localhost",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args(args)
        .stdin(Stdio::null())
        .output()?;
    ensure!(
        run.status.success(),
        "fixture Git failed: {}",
        String::from_utf8_lossy(&run.stderr)
    );
    Ok(String::from_utf8(run.stdout)?.trim().to_owned())
}

#[test]
fn retained_policy_sources_and_ledger_are_checked_without_authority() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    let work = root.join("work");
    fs::create_dir(&work)?;
    git(
        &work,
        &["init", "--quiet", "--initial-branch=main", "--template="],
    )?;
    fs::write(work.join("A.lean"), SOURCE)?;
    fs::write(work.join("ai4math-t2-release.json"), POLICY)?;
    git(&work, &["add", "A.lean", "ai4math-t2-release.json"])?;
    git(
        &work,
        &["commit", "--quiet", "-m", "readback evidence fixture"],
    )?;
    git(
        &root,
        &[
            "clone",
            "--quiet",
            "--bare",
            "--no-hardlinks",
            "work",
            "review.git",
        ],
    )?;
    let repo = root.join("review.git");
    let head = git(&repo, &["rev-parse", "refs/heads/main"])?;
    crate::init(&root, Engine::Lean4)?;
    let db = private_state_path(&root)?;
    let mut state = StateStore::open(&db)?;
    state.bind_task_repository(repo.to_str().context("fixture repository path")?, &head)?;
    let seq = state.status()?.last_event_seq;
    let make_record = || LocalReleaseEvidence {
        schema: LOCAL_EVIDENCE_SCHEMA.into(),
        reviewed_commit: head.clone(),
        ledger_event_seq: seq,
        policy_sha256: format!("{:x}", Sha256::digest(POLICY)),
        trust_debt_findings: 0,
        source_set_sha256: ordered_source_set_sha256(&[("A.lean", SOURCE)]),
        compiler_sha256: "1".repeat(64),
        exporter_sha256: "2".repeat(64),
        checker_sha256: "3".repeat(64),
        checks: ["N.x", "N.y"]
            .into_iter()
            .map(|name| LocalReleaseCheck {
                target_module: "A.lean".into(),
                declaration: name.into(),
                export_sha256: "4".repeat(64),
            })
            .collect(),
    };
    // These fixture records are not proof receipts: this test intentionally writes
    // them without executing any tools to exercise structural, read-only behavior.
    let oid = {
        let locked = PromotionRepo::try_lock(&repo)?;
        let (_, oid) =
            locked.create_release_tag("structure", &head, &serde_json::to_vec(&make_record())?)?;
        for (label, change) in [
            (
                "wrong-policy",
                (|e: &mut LocalReleaseEvidence| e.policy_sha256 = "5".repeat(64))
                    as fn(&mut LocalReleaseEvidence),
            ),
            ("wrong-sources", |e| e.source_set_sha256 = "6".repeat(64)),
            ("wrong-order", |e| e.checks.swap(0, 1)),
            ("wrong-name", |e| e.checks[1].declaration = "N.z".into()),
            ("future-ledger", |e| e.ledger_event_seq += 10),
        ] {
            let mut e = make_record();
            change(&mut e);
            locked.create_release_tag(label, &head, &serde_json::to_vec(&e)?)?;
        }
        oid
    };
    let refs = git(&repo, &["show-ref"])?;
    {
        // The shared validator runs inside the caller's existing Git lock.
        let locked = PromotionRepo::try_lock_existing(&repo)?;
        let baseline =
            validated_release_baseline(&locked, "structure", Some(&oid), seq, Some(SOURCE.len()))?;
        assert_eq!(baseline.tag_ref, "refs/tags/ai4math-release/structure");
        assert_eq!(baseline.tag_oid, oid);
        assert_eq!(baseline.reviewed_commit, head);
        assert_eq!(baseline.ledger_event_seq(), seq);
        assert_eq!(baseline.compiler_sha256(), "1".repeat(64));
        for label in ["structure", "wrong-sources"] {
            let err = validated_release_baseline(&locked, label, None, seq, Some(SOURCE.len() - 1))
                .err()
                .context("accepted evidence without enough source bytes")?;
            assert!(format!("{err:#}").contains("combined managed heartbeat sources exceed"));
        }
        let err = validated_release_baseline(&locked, "structure", Some(&head), seq, None)
            .err()
            .context("accepted a nonmatching tag pin")?;
        assert!(format!("{err:#}").contains("release tag OID mismatch"));
    }
    let read = inspect_release(&root, &repo, "structure", Some(&oid))?;
    assert_eq!(
        serde_json::to_value(read)?,
        json!({
            "schema": "ai4math.t2.release-inspection.v1",
            "advisory_only": true,
            "tag_ref": "refs/tags/ai4math-release/structure",
            "tag_oid": oid,
            "reviewed_commit": head,
            "ledger_event_seq": seq,
            "current_main": head,
            "current_ledger_event_seq": seq,
            "heads_match": true,
            "ledger_matches": true,
            "evidence": make_record(),
        })
    );
    for (label, diagnostic) in [
        ("wrong-policy", "policy SHA-256 mismatch"),
        ("wrong-sources", "ordered source set SHA-256 mismatch"),
        ("wrong-order", "policy order and scope"),
        ("wrong-name", "policy order and scope"),
        ("future-ledger", "future ledger generation"),
    ] {
        let err = inspect_release(&root, &repo, label, None)
            .err()
            .context("accepted inconsistent marker")?;
        assert!(format!("{err:#}").contains(diagnostic), "{label}: {err:#}");
    }
    let next = state.append_event(
        Change {
            actor: "inspection-fixture",
            git_commit: None,
        },
        "NOTE_CREATED",
        "note",
        "time-shift",
        json!({}),
    )?;
    let read = inspect_release(&root, &repo, "structure", None)?;
    assert!(read.advisory_only && read.heads_match && !read.ledger_matches);
    assert_eq!(
        (read.ledger_event_seq, read.current_ledger_event_seq),
        (seq, next)
    );
    assert_eq!(state.status()?.last_event_seq, next);
    assert_eq!(git(&repo, &["show-ref"])?, refs);
    Ok(())
}
