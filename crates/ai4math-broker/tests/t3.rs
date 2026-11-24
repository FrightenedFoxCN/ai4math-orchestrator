//! Safe end-to-end checks with an operator-selected native Lean installation.
#![cfg(target_os = "macos")]

use ai4math_broker::{Broker, ReceiptTier, RecheckAssessment, Toolchain};
use ai4math_core::{Engine, FrozenTarget, ProofBodySubmission};

#[test]
#[ignore = "set AI4MATH_TEST_LEAN to the native Lean executable and run explicitly"]
fn native_t3_distinguishes_mismatch_from_missing_checks() {
    let lean = std::env::var_os("AI4MATH_TEST_LEAN")
        .or_else(|| std::env::var_os("AI4MATH_TEST_NATIVE_LEAN"))
        .expect("native Lean path");
    let tool = Toolchain::lean(lean).unwrap();
    let broker = Broker::new();
    let target = FrozenTarget {
        engine: Engine::Lean4,
        target_id: "checked".into(),
        file_path: "Target.lean".into(),
        prefix: "import Support\n\ntheorem checked : current = 0 := ".into(),
        suffix: "\n".into(),
    };
    let proof = ProofBodySubmission {
        target_id: target.target_id.clone(),
        proof_body: "by rfl".into(),
    };
    let commit = "a".repeat(40);
    let original = [("Support.lean", "def current : Nat := 0\n")];
    let result =
        broker.assess_recheck_with_dependencies(&tool, &target, &proof, &commit, &original);
    let RecheckAssessment::Verified(candidate) = result else {
        panic!("unchanged proof must verify: {result:?}");
    };
    assert_eq!(candidate.receipt().tier(), ReceiptTier::T3TaintRebuild);
    assert!(
        candidate
            .receipt()
            .matches(&commit, "Target.lean", candidate.source().as_bytes())
    );
    assert_eq!(
        broker
            .recheck_with_dependencies(&tool, &target, &proof, &commit, &original)
            .unwrap()
            .receipt()
            .tier(),
        ReceiptTier::T3TaintRebuild
    );
    let changed = [("Support.lean", "def current : Nat := 1\n")];
    let result = broker.assess_recheck_with_dependencies(&tool, &target, &proof, &commit, &changed);
    assert!(
        matches!(&result, RecheckAssessment::Rejected { diagnostics } if diagnostics.contains("not definitionally equal")),
        "{result:?}"
    );

    let result = broker.assess_recheck_with_dependencies(&tool, &target, &proof, &commit, &[]);
    assert!(
        matches!(&result, RecheckAssessment::Unavailable { diagnostics } if diagnostics.contains("unknown module prefix")),
        "{result:?}"
    );
    let broken = [("Support.lean", "def current : Nat := True.intro\n")];
    let result = broker.assess_recheck_with_dependencies(&tool, &target, &proof, &commit, &broken);
    assert!(
        matches!(&result, RecheckAssessment::Unavailable { diagnostics } if diagnostics.contains("build verification dependencies")),
        "{result:?}"
    );
    let inadmissible = ProofBodySubmission {
        proof_body: "(".into(),
        ..proof
    };
    let result =
        broker.assess_recheck_with_dependencies(&tool, &target, &inadmissible, &commit, &original);
    assert!(
        matches!(&result, RecheckAssessment::Unavailable { diagnostics } if diagnostics.contains("invalid proof body")),
        "{result:?}"
    );
}
