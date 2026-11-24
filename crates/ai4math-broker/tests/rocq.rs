//! Native broker tests need a macOS host on which Seatbelt profiles can run.
#![cfg(target_os = "macos")]

use ai4math_broker::{Broker, ReceiptTier, RecheckAssessment, Toolchain, clone_golden};
use ai4math_core::{Engine, FrozenTarget, ProofBodySubmission};
use anyhow::{Context, Result};
use std::{env, fs};

const BASE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn toolchain() -> Result<Toolchain> {
    let driver = env::var_os("AI4MATH_TEST_ROCQ").context("native Rocq 9.3 driver path")?;
    let data = env::var_os("AI4MATH_TEST_ROCQ_DATA").context("trusted Rocq data path")?;
    let mut toolchain = Toolchain::rocq(driver, data)?;
    if let Some(reads) = env::var_os("AI4MATH_TEST_ROCQ_READS") {
        for read in env::split_paths(&reads) {
            toolchain = toolchain.with_runtime_read(read)?;
        }
    }
    Ok(toolchain)
}

fn target(imports: &str) -> FrozenTarget {
    FrozenTarget {
        engine: Engine::Rocq,
        target_id: "target".into(),
        file_path: "Fixture.v".into(),
        prefix: format!("{imports}Theorem target : True.\nProof.\n  exact ("),
        suffix: ").\nQed.\n".into(),
    }
}

fn proof(term: &str) -> ProofBodySubmission {
    ProofBodySubmission {
        target_id: "target".into(),
        proof_body: term.into(),
    }
}

#[test]
#[ignore = "requires native Rocq 9.3 and Seatbelt; set AI4MATH_TEST_ROCQ, AI4MATH_TEST_ROCQ_DATA and AI4MATH_TEST_ROCQ_READS"]
fn closed_promotion_regression_and_golden_vo() -> Result<()> {
    let toolchain = toolchain()?;
    let broker = Broker::new();
    let target = target("Require Import Earlier.\n");
    let earlier = ("Earlier.v", "Definition closed : True := I.\n");
    let proof = proof("Earlier.closed");
    let candidate =
        broker.verify_with_dependencies(&toolchain, &target, &proof, BASE, &[earlier])?;
    assert_eq!(candidate.receipt().tier(), ReceiptTier::T1Promotion);
    assert_eq!(candidate.receipt().engine(), Engine::Rocq);
    assert!(candidate.receipt().observed_axioms().is_empty());
    assert!(
        candidate
            .receipt()
            .matches(BASE, "Fixture.v", candidate.source().as_bytes())
    );
    assert!(!candidate.source().contains("Print Assumptions"));
    assert!(broker.verify(&toolchain, &target, &proof, BASE).is_err());
    let verified = ("Fixture.v", candidate.source());
    let later = (
        "Later.v",
        "Require Import Fixture.\nDefinition followup : True := Fixture.target.\n",
    );
    let artifacts = broker.regress(&toolchain, &candidate, &[earlier, verified, later])?;
    assert!(artifacts.matches(&candidate));
    assert!(
        broker
            .regress(&toolchain, &candidate, &[verified, earlier, later])
            .is_err()
    );
    assert!(
        broker
            .regress(
                &toolchain,
                &candidate,
                &[
                    earlier,
                    verified,
                    ("Later.v", "Definition wrong : True := 0.\n")
                ]
            )
            .is_err()
    );

    let tmp = tempfile::tempdir()?;
    let golden = tmp.path().join("golden");
    let clone = tmp.path().join("clone");
    artifacts.publish(&golden, BASE)?;
    assert_eq!(clone_golden(&golden, BASE, &clone)?, 3);
    for name in ["Earlier.vo", "Fixture.vo", "Later.vo"] {
        assert!(fs::metadata(clone.join(name))?.len() > 0);
    }
    assert!(!clone.join("Ai4mathRocqAudit.vo").exists());
    Ok(())
}

#[test]
#[ignore = "requires native Rocq 9.3 and Seatbelt; set AI4MATH_TEST_ROCQ, AI4MATH_TEST_ROCQ_DATA and AI4MATH_TEST_ROCQ_READS"]
fn invalid_term_and_cross_engine_cannot_issue_receipts() -> Result<()> {
    let toolchain = toolchain()?;
    let broker = Broker::new();
    let target = target("");
    let wrong = proof("0");
    let err = broker
        .verify(&toolchain, &target, &wrong, BASE)
        .unwrap_err();
    assert!(format!("{err:#}").contains("has type"), "{err:#}");
    let assessment =
        broker.assess_recheck_with_dependencies(&toolchain, &target, &wrong, BASE, &[]);
    assert!(
        matches!(assessment, RecheckAssessment::Unavailable { .. }),
        "{assessment:?}"
    );
    let lean = FrozenTarget {
        engine: Engine::Lean4,
        file_path: "Other.lean".into(),
        ..target
    };
    assert!(
        broker
            .verify(&toolchain, &lean, &proof("I"), BASE)
            .unwrap_err()
            .to_string()
            .contains("engine mismatch")
    );
    Ok(())
}

#[test]
#[ignore = "requires native Rocq 9.3 with Stdlib and Seatbelt; set AI4MATH_TEST_ROCQ, AI4MATH_TEST_ROCQ_DATA and AI4MATH_TEST_ROCQ_READS"]
fn kernel_audit_rejects_indirect_imported_axiom_and_reserved_audit() -> Result<()> {
    let toolchain = toolchain()?;
    let broker = Broker::new();
    let classical = target("From Stdlib Require Import Logic.Classical_Prop.\n");
    let proof_using_axiom = proof("or_ind (fun p => p) (fun _ => I) (classic True)");
    let err = broker
        .verify(&toolchain, &classical, &proof_using_axiom, BASE)
        .unwrap_err();
    let log = format!("{err:#}");
    assert!(
        log.contains("assumptions audit") && log.contains("Classical_Prop.classic"),
        "{log}"
    );
    assert!(
        broker
            .verify(&toolchain, &classical, &proof("I"), BASE)
            .is_ok()
    );

    let plain = target("");
    let err = broker
        .verify_with_dependencies(
            &toolchain,
            &plain,
            &proof("I"),
            BASE,
            &[("Ai4MathRocqAudit.v", "Definition closed : True := I.\n")],
        )
        .unwrap_err();
    assert!(
        err.to_string().contains("reserved Rocq audit module"),
        "{err:#}"
    );
    let forged_source = "Definition closed : True := I.\nPrint Assumptions closed.\n";
    let err = broker
        .verify_with_dependencies(
            &toolchain,
            &plain,
            &proof("I"),
            BASE,
            &[("Forgery.v", forged_source)],
        )
        .unwrap_err();
    assert!(err.to_string().contains("Rocq preflight"), "{err:#}");
    Ok(())
}
