use super::{Change, NewClaim, RecheckOutcome, StateStore, Trust, VerifiedReceipt};
use ai4math_broker::{Broker, ReceiptTier, RecheckAssessment, Toolchain};
use ai4math_core::{Engine, FrozenTarget, ProofBodySubmission};
use anyhow::Result;

const BASE: &str = "1111111111111111111111111111111111111111";
const HEAD: &str = "2222222222222222222222222222222222222222";

fn change(commit: &str) -> Change<'_> {
    Change {
        actor: "native-integration",
        git_commit: Some(commit),
    }
}

#[test]
#[cfg(target_os = "macos")]
#[ignore = "requires a native host (no outer Seatbelt) and AI4MATH_TEST_LEAN"]
fn native_old_proof_recheck_updates_sqlite_atomically() -> Result<()> {
    let path = std::env::var_os("AI4MATH_TEST_LEAN").expect("set native Lean binary");
    let toolchain = Toolchain::lean(path)?;
    let broker = Broker::new();
    let mut target = FrozenTarget {
        engine: Engine::Lean4,
        target_id: "target".into(),
        file_path: "Proof.lean".into(),
        prefix: "import Spec\ntheorem target : x = 2 := ".into(),
        suffix: "\n".into(),
    };
    let old_proof = ProofBodySubmission {
        target_id: "target".into(),
        proof_body: "by\n  rfl".into(),
    };
    let old_deps = [("Spec.lean", "def x : Nat := 2\n")];
    let new_deps = [("Spec.lean", "def x : Nat := Nat.succ 1\n")];
    let tmp = tempfile::tempdir()?;
    let database = tmp.path().join("state.sqlite");
    let mut state = StateStore::open(&database)?;
    state.create_claim(
        NewClaim {
            id: "C",
            title: "C",
            informal_statement: "x = 2",
            engine: "lean4",
            formal_target: Some("Proof.lean#target"),
            module_path: Some("Proof.lean"),
            critical: true,
        },
        change(BASE),
    )?;
    let t1 = broker.verify_with_dependencies(&toolchain, &target, &old_proof, BASE, &old_deps)?;
    state.record_verified("C", &VerifiedReceipt::from_broker(&t1)?, change(BASE))?;
    state.add_module_dependency(
        "Proof.lean",
        "Spec.lean",
        "lean4",
        "leaf_proof",
        change(BASE),
    )?;
    state.mark_t3(&[], &["Spec.lean"], change(HEAD))?;
    assert!(state.claim("C")?.unwrap().stale);

    // A genuine legacy verification with the correct tier and snapshot is not a T3 assessment.
    let legacy =
        broker.recheck_with_dependencies(&toolchain, &target, &old_proof, HEAD, &new_deps)?;
    assert_eq!(legacy.receipt().tier(), ReceiptTier::T3TaintRebuild);
    assert!(!legacy.is_assessed_t3());
    let denied = state
        .recheck_stale(HEAD, "native-integration", |_| {
            Ok(RecheckOutcome::Verified(Box::new(
                VerifiedReceipt::from_broker(&legacy)?,
            )))
        })
        .unwrap_err();
    assert!(denied.to_string().contains("broker-assessed T3 provenance"));
    assert!(state.claim("C")?.unwrap().stale);

    let assess = |target: &FrozenTarget| {
        let result = broker
            .assess_recheck_with_dependencies(&toolchain, target, &old_proof, HEAD, &new_deps);
        let RecheckAssessment::Verified(candidate) = result else {
            anyhow::bail!("expected successful T3 assessment: {result:?}");
        };
        Ok(candidate)
    };

    // A valid theorem with the same name in the wrong file cannot clear this claim.
    target.file_path = "Other.lean".into();
    let mismatch = state
        .recheck_stale(HEAD, "native-integration", |_| {
            let wrong = assess(&target)?;
            Ok(RecheckOutcome::Verified(Box::new(
                VerifiedReceipt::from_broker(&wrong)?,
            )))
        })
        .unwrap_err();
    assert!(mismatch.to_string().contains("module does not match"));
    assert!(state.claim("C")?.unwrap().stale);

    target.file_path = "Proof.lean".into();
    let report = state.recheck_stale(HEAD, "native-integration", |_| {
        let result = assess(&target)?;
        assert_eq!(result.receipt().tier(), ReceiptTier::T3TaintRebuild);
        assert_ne!(
            result.receipt().receipt_sha256(),
            t1.receipt().receipt_sha256()
        );
        Ok(RecheckOutcome::Verified(Box::new(
            VerifiedReceipt::from_broker(&result)?,
        )))
    })?;
    assert_eq!(report.verified, ["C"]);
    assert!(report.needs_reproof.is_empty());
    drop(state);

    let state = StateStore::open(database)?;
    let persisted = state.claim("C")?.unwrap();
    assert_eq!(persisted.local_trust, Trust::L5);
    assert!(!persisted.stale);
    assert!(
        state
            .events_after(0, 100)?
            .iter()
            .any(|e| e.event_type == "RECEIPT_RECORDED"
                && e.git_commit.as_deref() == Some(HEAD)
                && e.payload["tier"] == "T3_TAINT_REBUILD")
    );
    Ok(())
}
