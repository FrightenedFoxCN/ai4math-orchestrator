#![cfg(target_os = "macos")]

use ai4math_broker::{Broker, RecheckAssessment, Toolchain};
use ai4math_core::{Engine, FrozenTarget, InfraRFC, InfraStatus, ProofBodySubmission};
use ai4math_evidence::ReceiptTier;
use ai4math_store::{
    Change, InfraRfcStatus, NewClaim, PromotionIntent, RecheckOutcome, SedimentReplayScope,
    StateStore, Trust, VerifiedReceipt,
};
use anyhow::{Context, Result};

#[test]
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
    let base = "1111111111111111111111111111111111111111";
    let promoted = "2222222222222222222222222222222222222222";
    let head = "3333333333333333333333333333333333333333";
    let repo = "/managed/project";
    let change = |commit| Change {
        actor: "native-integration",
        git_commit: Some(commit),
    };
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
        change(base),
    )?;
    state.bind_task_repository(repo, base)?;
    let t1 = broker.verify_with_dependencies(&toolchain, &target, &old_proof, base, &old_deps)?;
    let regression_sources = [old_deps[0], (target.file_path.as_str(), t1.source())];
    let artifacts = broker.regress(&toolchain, &t1, &regression_sources)?;
    let receipt = VerifiedReceipt::from_promotion(&t1, &artifacts)?;
    let intent = PromotionIntent {
        repo: repo.into(),
        claim_id: "C".into(),
        base_commit: base.into(),
        new_commit: promoted.into(),
        file_path: "Proof.lean".into(),
        target_decl: "target".into(),
        proof_body: old_proof.proof_body.clone(),
    };
    state.prepare_promotion(&intent, &receipt)?;
    state.finish_promotion(&intent, &receipt)?;
    state.add_module_dependency(
        "Proof.lean",
        "Spec.lean",
        "lean4",
        "leaf_proof",
        change(base),
    )?;
    state.mark_t3(&[], &["Spec.lean"], change(head))?;
    assert!(state.claim("C")?.unwrap().stale);

    // A genuine legacy verification with the correct tier and snapshot is not a T3 assessment.
    let legacy =
        broker.recheck_with_dependencies(&toolchain, &target, &old_proof, head, &new_deps)?;
    assert_eq!(legacy.receipt().tier(), ReceiptTier::T3TaintRebuild);
    assert!(!legacy.is_assessed_t3());
    let denied = state
        .recheck_stale(head, "native-integration", |_| {
            Ok(RecheckOutcome::Verified(Box::new(
                VerifiedReceipt::from_broker(&legacy)?,
            )))
        })
        .unwrap_err();
    assert!(denied.to_string().contains("broker-assessed T3 provenance"));
    assert!(state.claim("C")?.unwrap().stale);

    let assess = |target: &FrozenTarget| {
        let result = broker
            .assess_recheck_with_dependencies(&toolchain, target, &old_proof, head, &new_deps);
        let RecheckAssessment::Verified(candidate) = result else {
            anyhow::bail!("expected successful T3 assessment: {result:?}");
        };
        Ok(candidate)
    };

    // A valid theorem with the same name in the wrong file cannot clear this claim.
    target.file_path = "Other.lean".into();
    let mismatch = state
        .recheck_stale(head, "native-integration", |_| {
            let wrong = assess(&target)?;
            Ok(RecheckOutcome::Verified(Box::new(
                VerifiedReceipt::from_broker(&wrong)?,
            )))
        })
        .unwrap_err();
    assert!(mismatch.to_string().contains("module does not match"));
    assert!(state.claim("C")?.unwrap().stale);

    target.file_path = "Proof.lean".into();
    let report = state.recheck_stale(head, "native-integration", |_| {
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
                && e.git_commit.as_deref() == Some(head)
                && e.payload["tier"] == "T3_TAINT_REBUILD")
    );
    Ok(())
}

const SEDIMENT_REPO: &str = "/managed/project";
const SEDIMENT_CLAIM: &str = "claim:target";
const SEDIMENT_FILE: &str = "Proof.lean";
const SEDIMENT_BASE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const SEDIMENT_PROMOTED: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const SEDIMENT_HEAD: &str = "cccccccccccccccccccccccccccccccccccccccc";
const SEDIMENT_ARCHIVE: &str =
    "SED-1111111111111111111111111111111111111111111111111111111111111111";

fn sediment_target_and_proof() -> (FrozenTarget, ProofBodySubmission) {
    (
        FrozenTarget {
            engine: Engine::Lean4,
            prefix: "theorem target : True := ".into(),
            suffix: "\n".into(),
            target_id: "target".into(),
            file_path: SEDIMENT_FILE.into(),
        },
        ProofBodySubmission {
            target_id: "target".into(),
            proof_body: "by trivial".into(),
        },
    )
}

fn sediment_scope(expected_event_seq: i64) -> SedimentReplayScope<'static> {
    SedimentReplayScope {
        repo: SEDIMENT_REPO,
        archive_id: SEDIMENT_ARCHIVE,
        claim_id: SEDIMENT_CLAIM,
        promoted_commit: SEDIMENT_PROMOTED,
        reviewed_commit: SEDIMENT_HEAD,
        expected_event_seq,
    }
}

fn install_historical_promotion(
    store: &mut StateStore,
    broker: &Broker,
    tool: &Toolchain,
) -> Result<()> {
    store.create_claim(
        NewClaim {
            id: SEDIMENT_CLAIM,
            title: "Target",
            informal_statement: "True",
            engine: "lean4",
            formal_target: Some("Proof.lean#target"),
            module_path: Some(SEDIMENT_FILE),
            critical: false,
        },
        Change {
            actor: "fixture",
            git_commit: Some(SEDIMENT_BASE),
        },
    )?;
    let intent = PromotionIntent {
        repo: SEDIMENT_REPO.into(),
        claim_id: SEDIMENT_CLAIM.into(),
        base_commit: SEDIMENT_BASE.into(),
        new_commit: SEDIMENT_PROMOTED.into(),
        file_path: SEDIMENT_FILE.into(),
        target_decl: "target".into(),
        proof_body: "by trivial".into(),
    };
    let (target, proof) = sediment_target_and_proof();
    let candidate = broker.verify_with_dependencies(tool, &target, &proof, SEDIMENT_BASE, &[])?;
    let artifacts = broker.regress(tool, &candidate, &[(SEDIMENT_FILE, candidate.source())])?;
    let receipt = VerifiedReceipt::from_promotion(&candidate, &artifacts)?;
    store.prepare_promotion(&intent, &receipt)?;
    store.finish_promotion(&intent, &receipt)?;
    Ok(())
}

#[test]
#[ignore = "requires a native host (no outer Seatbelt) and AI4MATH_TEST_LEAN"]
fn native_retention_rejects_legacy_t3_and_unrelated_bindings() -> Result<()> {
    let lean = std::env::var_os("AI4MATH_TEST_LEAN").context("set native Lean binary")?;
    let tool = Toolchain::lean(lean)?;
    let broker = Broker::new();
    let (target, proof) = sediment_target_and_proof();
    let mut store = StateStore::open(":memory:")?;
    install_historical_promotion(&mut store, &broker, &tool)?;
    let before = store.status()?.last_event_seq;
    let legacy = broker.recheck_with_dependencies(&tool, &target, &proof, SEDIMENT_HEAD, &[])?;
    assert_eq!(legacy.receipt().tier(), ReceiptTier::T3TaintRebuild);
    assert!(!legacy.is_assessed_t3());
    let error = store
        .retain_sediment_replay(sediment_scope(before), &legacy, None)
        .unwrap_err();
    assert!(error.to_string().contains("natively assessed Lean T3"));

    let assessment =
        broker.assess_recheck_with_dependencies(&tool, &target, &proof, SEDIMENT_HEAD, &[]);
    let RecheckAssessment::Verified(candidate) = assessment else {
        anyhow::bail!("expected successful native assessment: {assessment:?}");
    };
    for (invalid, message) in [
        (
            SedimentReplayScope {
                repo: "/other",
                ..sediment_scope(before)
            },
            "bound managed repository",
        ),
        (
            SedimentReplayScope {
                promoted_commit: SEDIMENT_BASE,
                ..sediment_scope(before)
            },
            "exact historical",
        ),
        (
            SedimentReplayScope {
                reviewed_commit: SEDIMENT_PROMOTED,
                ..sediment_scope(before)
            },
            "does not match the reviewed commit",
        ),
        (sediment_scope(before - 1), "ledger event sequence changed"),
    ] {
        let error = store
            .retain_sediment_replay(invalid, &candidate, None)
            .unwrap_err();
        assert!(error.to_string().contains(message), "{error:#}");
        assert_eq!(store.status()?.last_event_seq, before);
    }
    let assessment = broker.assess_recheck_with_dependencies(
        &tool,
        &target,
        &proof,
        SEDIMENT_HEAD,
        &[("Extra.lean", "def unused : Nat := 1\n")],
    );
    let RecheckAssessment::Verified(changed_prefix) = assessment else {
        anyhow::bail!("expected successful native assessment: {assessment:?}");
    };
    let error = store
        .retain_sediment_replay(sediment_scope(before), &changed_prefix, None)
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("managed T1 proof and dependency prefix")
    );
    assert_eq!(store.status()?.last_event_seq, before);
    let retained = store.retain_sediment_replay(sediment_scope(before), &candidate, None)?;
    assert_eq!(
        store.retain_sediment_replay(sediment_scope(before), &candidate, None)?,
        retained
    );
    assert_eq!(store.status()?.last_event_seq, retained.event_seq);
    Ok(())
}

const INFRA_REPO: &str = "/managed/rfc.git";
const INFRA_BASE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const INFRA_SMOKE_COMMIT: &str = "cccccccccccccccccccccccccccccccccccccccc";
const INFRA_MODULE: &str = "Basis.lean";

fn infra_draft() -> InfraRFC {
    InfraRFC {
        rfc_id: "RFC-native".into(),
        requested_by_claim: "Consumer".into(),
        engine: Engine::Lean4,
        motivation: "Need a reusable boundary API.".into(),
        proposed_definitions: vec!["Basis.bound".into()],
        required_api_lemmas: vec!["Basis.bound_spec".into()],
        consumer_usage_sketch: "exact Basis.bound_spec\n-- sketch, not executed".into(),
        smoke_test_criteria: vec!["Check an actual boundary case".into()],
        status: InfraStatus::Draft,
    }
}

fn enroll_infra_consumer(store: &mut StateStore) -> Result<()> {
    let change = Change {
        actor: "fixture",
        git_commit: Some(INFRA_BASE),
    };
    store.bind_task_repository(INFRA_REPO, INFRA_BASE)?;
    store.create_claim(
        NewClaim {
            id: "Consumer",
            title: "Consumer",
            informal_statement: "uses the new basis",
            engine: "lean4",
            formal_target: Some("Consumer.lean#N.theorem"),
            module_path: Some("Consumer.lean"),
            critical: false,
        },
        change,
    )?;
    Ok(())
}

#[test]
#[ignore = "requires a native host (no outer Seatbelt) and AI4MATH_TEST_LEAN"]
fn native_broker_smoke_and_completion_end_to_end() -> Result<()> {
    let lean = std::env::var_os("AI4MATH_TEST_LEAN").expect("set native Lean binary");
    let tool = Toolchain::lean(lean)?;
    let broker = Broker::new();
    let tmp = tempfile::tempdir()?;
    let mut store = StateStore::open(tmp.path().join("native.sqlite"))?;
    enroll_infra_consumer(&mut store)?;
    let draft = infra_draft();
    let submitted =
        store.submit_infra_rfc(INFRA_REPO, INFRA_BASE, INFRA_MODULE, &draft, || Ok(()))?;
    store.approve_infra_rfc(
        INFRA_REPO,
        INFRA_BASE,
        &draft.rfc_id,
        &submitted.draft_sha256,
        || Ok(()),
    )?;
    store.start_infra_rfc_smoke(
        INFRA_REPO,
        INFRA_BASE,
        &draft.rfc_id,
        &submitted.draft_sha256,
        || Ok(()),
    )?;

    let src = "namespace Basis\ndef bound (n : Nat) : Nat := n + 1\ntheorem bound_smoke : bound 0 = 1 := rfl\ntheorem bound_spec (n : Nat) : bound n = n + 1 := rfl\nend Basis\n";
    let smoke = broker.verify_lean_infra_smoke(
        &tool,
        INFRA_SMOKE_COMMIT,
        INFRA_MODULE,
        &["Basis.bound"],
        &["Basis.bound_smoke"],
        &[(INFRA_MODULE, src)],
    )?;
    let frozen = store.freeze_infra_rfc(
        INFRA_REPO,
        &draft.rfc_id,
        &submitted.draft_sha256,
        &smoke,
        || Ok(()),
    )?;
    assert_eq!(frozen.status, InfraRfcStatus::FrozenProving);
    assert_eq!(frozen.smoke_commit.as_deref(), Some(INFRA_SMOKE_COMMIT));
    assert_eq!(
        frozen.smoke_receipt_sha256.as_deref(),
        Some(smoke.receipt_sha256())
    );

    let completion = broker.verify_lean_infra_completion(
        &tool,
        INFRA_SMOKE_COMMIT,
        INFRA_MODULE,
        &["Basis.bound"],
        &["Basis.bound_spec"],
        &[(INFRA_MODULE, src)],
    )?;
    let completed = store.complete_infra_rfc(
        INFRA_REPO,
        &draft.rfc_id,
        &submitted.draft_sha256,
        &completion,
        || Ok(()),
    )?;
    assert_eq!(completed.status, InfraRfcStatus::Completed);
    assert_eq!(
        completed.completion_commit.as_deref(),
        Some(INFRA_SMOKE_COMMIT)
    );
    assert_eq!(
        completed.completion_receipt_sha256.as_deref(),
        Some(completion.receipt_sha256())
    );
    Ok(())
}
