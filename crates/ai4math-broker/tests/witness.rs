//! Native, benign end-to-end checks through the broker's real sandbox.
#![cfg(target_os = "macos")]

use ai4math_broker::{Broker, Toolchain};
use ai4math_core::ProofBodySubmission;

#[test]
#[ignore = "set AI4MATH_TEST_LEAN to the native Lean executable and run explicitly"]
fn native_lean_witness_checks_exact_proposition_and_ordered_sources() {
    let lean = std::env::var_os("AI4MATH_TEST_LEAN")
        .or_else(|| std::env::var_os("AI4MATH_TEST_NATIVE_LEAN"))
        .expect("native Lean path");
    let tool = Toolchain::lean(lean).unwrap();
    let broker = Broker::new();
    let commit = "a".repeat(40);
    const PROOF: &str = "by\n  change ¬ (current = 0)\n  decide";
    let proof = ProofBodySubmission {
        target_id: "witness".into(),
        proof_body: PROOF.into(),
    };
    let source = "import Support\n\ndef target : Prop := current = 0\n";
    let support = ("Support.lean", "def current : Nat := 1\n");
    let other = ("Other.lean", "def other : Prop := False\n");
    let verify = |name: &str, source: &str, proof: &ProofBodySubmission, deps: &[(&str, &str)]| {
        broker.verify_lean_witness_with_dependencies(
            &tool,
            "Target.lean",
            name,
            source,
            proof,
            &commit,
            deps,
        )
    };
    let witness = verify("target", source, &proof, &[support, other]).unwrap();
    assert!(witness.observed_axioms().is_empty());
    assert!(witness.matches(
        &commit,
        "Target.lean",
        "target",
        source.as_bytes(),
        PROOF.as_bytes()
    ));
    assert!(!witness.matches(
        &commit,
        "Target.lean",
        "target",
        source.as_bytes(),
        format!("{PROOF}\n").as_bytes()
    ));
    assert!(
        witness
            .source()
            .contains("theorem witness : ai4mathSealedRefutation")
    );
    assert_eq!(
        witness.witness_sha256(),
        verify("target", source, &proof, &[support, other])
            .unwrap()
            .witness_sha256()
    );
    assert_ne!(
        witness.witness_sha256(),
        verify("target", source, &proof, &[other, support])
            .unwrap()
            .witness_sha256()
    );
    assert_ne!(
        witness.witness_sha256(),
        verify("target", &format!("{source}\n"), &proof, &[support, other])
            .unwrap()
            .witness_sha256()
    );

    let classical = ProofBodySubmission {
        target_id: "witness".into(),
        proof_body:
            "by\n  change ¬ (current = 0)\n  exact Classical.choice (Nonempty.intro (by decide))"
                .into(),
    };
    assert_eq!(
        verify("target", source, &classical, &[support])
            .unwrap()
            .observed_axioms(),
        &["Classical.choice"],
    );

    let changed = ("Support.lean", "def current : Nat := 0\n");
    assert!(
        verify("target", source, &proof, &[changed]).is_err(),
        "true proposition"
    );
    assert!(
        verify("target", source, &proof, &[]).is_err(),
        "missing dependency"
    );
    let identity = ProofBodySubmission {
        target_id: "witness".into(),
        proof_body: "fun h => h".into(),
    };
    let wrong_home = verify("other", "import Other\n", &identity, &[other]).unwrap_err();
    assert!(format!("{wrong_home:#}").contains("not declared by the committed target module"));
    for (source, diagnostic) in [
        ("def target : Nat := 0\n", "closed, monomorphic definition"),
        (
            "theorem target : True := True.intro\n",
            "public definition of Prop",
        ),
        (
            "opaque target : Prop := False\n",
            "public definition of Prop",
        ),
        ("unsafe def target : Prop := False\n", "safe definition"),
        (
            "def target (x : Nat) : Prop := x = 1\n",
            "closed, monomorphic definition",
        ),
        (
            "universe u\ndef target : Prop := Nonempty (Type u)\n",
            "closed, monomorphic definition",
        ),
    ] {
        let error = verify("target", source, &identity, &[]).unwrap_err();
        assert!(
            format!("{error:#}").contains(diagnostic),
            "unsupported: {source}: {error:#}"
        );
    }
    let proof = ProofBodySubmission {
        target_id: "target".into(),
        ..proof
    };
    assert!(
        verify("target", source, &proof, &[support]).is_err(),
        "unreserved RHS target"
    );
}
