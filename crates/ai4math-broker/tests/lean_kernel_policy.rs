//! Native Lean admission and independent kernel-replay regressions.
#![cfg(target_os = "macos")]

use ai4math_broker::{Broker, ReceiptTier, RecheckAssessment, Toolchain};
use ai4math_core::{Engine, FrozenTarget, ProofBodySubmission};

fn native() -> Toolchain {
    let lean = std::env::var_os("AI4MATH_TEST_LEAN")
        .or_else(|| std::env::var_os("AI4MATH_TEST_NATIVE_LEAN"))
        .expect("native Lean path");
    Toolchain::lean(lean).unwrap()
}

fn make_target(prefix: &str) -> FrozenTarget {
    FrozenTarget {
        engine: Engine::Lean4,
        target_id: "checked".into(),
        file_path: "Target.lean".into(),
        prefix: prefix.into(),
        suffix: "\n".into(),
    }
}

fn make_proof(target: &FrozenTarget, body: &str) -> ProofBodySubmission {
    ProofBodySubmission {
        target_id: target.target_id.clone(),
        proof_body: body.into(),
    }
}

#[test]
#[ignore = "set AI4MATH_TEST_LEAN to the native Lean executable and run explicitly"]
fn native_lean_t1_t3_regression_and_check_modules_accept_normal_closure() {
    let tool = native();
    let broker = Broker::new();
    let commit = "a".repeat(40);
    let dependencies = [("Support.lean", "def current : Nat := 0\n")];
    let target = make_target("import Support\ntheorem checked : current = 0 := ");
    let proof = make_proof(&target, "by rfl");

    let candidate = broker
        .verify_with_dependencies(&tool, &target, &proof, &commit, &dependencies)
        .unwrap();
    assert_eq!(candidate.receipt().tier(), ReceiptTier::T1Promotion);
    assert!(candidate.receipt().observed_axioms().is_empty());
    assert!(
        candidate
            .receipt()
            .matches(&commit, "Target.lean", candidate.source().as_bytes())
    );
    assert!(!candidate.is_assessed_t3());

    let legacy = broker
        .recheck_with_dependencies(&tool, &target, &proof, &commit, &dependencies)
        .unwrap();
    assert_eq!(legacy.receipt().tier(), ReceiptTier::T3TaintRebuild);
    assert!(!legacy.is_assessed_t3());
    let assessed =
        broker.assess_recheck_with_dependencies(&tool, &target, &proof, &commit, &dependencies);
    let RecheckAssessment::Verified(assessed) = assessed else {
        panic!("normal assessed T3 replay must verify: {assessed:?}");
    };
    assert!(assessed.is_assessed_t3());

    let mut modules = dependencies.to_vec();
    modules.push(("Target.lean", candidate.source()));
    modules.push((
        "Later.lean",
        "import Support\ntheorem later : current = 0 := rfl\n",
    ));
    broker.check_modules(&tool, &modules).unwrap();
    broker.regress(&tool, &candidate, &modules).unwrap();

    let classical_target = make_target("theorem checked : True := ");
    let classical = broker
        .verify_with_dependencies(
            &tool,
            &classical_target,
            &make_proof(
                &classical_target,
                "Classical.choice (Nonempty.intro True.intro)",
            ),
            &commit,
            &[],
        )
        .unwrap();
    assert_eq!(classical.receipt().observed_axioms(), &["Classical.choice"]);
}

#[test]
#[ignore = "set AI4MATH_TEST_LEAN to the native Lean executable and run explicitly"]
fn native_lean_public_entrypoints_reject_installation_shadows() {
    let tool = native();
    let broker = Broker::new();
    let commit = "a".repeat(40);
    let normal = make_target("theorem checked : True := ");
    let proof = make_proof(&normal, "True.intro");
    let candidate = broker
        .verify_with_dependencies(&tool, &normal, &proof, &commit, &[])
        .unwrap();

    let shadow_target = FrozenTarget {
        file_path: "Init.lean".into(),
        ..normal.clone()
    };
    let error = broker
        .verify_with_dependencies(&tool, &shadow_target, &proof, &commit, &[])
        .unwrap_err();
    assert!(format!("{error:#}").contains("shadows the trusted Lean installation"));

    for dependencies in [&[("Lean.lean", "")][..], &[("Std.lean", "")][..]] {
        let error = broker
            .verify_with_dependencies(&tool, &normal, &proof, &commit, dependencies)
            .unwrap_err();
        assert!(format!("{error:#}").contains("shadows the trusted Lean installation"));
        let error = broker
            .recheck_with_dependencies(&tool, &normal, &proof, &commit, dependencies)
            .unwrap_err();
        assert!(format!("{error:#}").contains("shadows the trusted Lean installation"));
        let assessment =
            broker.assess_recheck_with_dependencies(&tool, &normal, &proof, &commit, dependencies);
        assert!(
            matches!(&assessment, RecheckAssessment::Unavailable { diagnostics }
                if diagnostics.contains("shadows the trusted Lean installation")),
            "{assessment:?}"
        );
    }

    let error = broker
        .check_modules(&tool, &[("Init.lean", "")])
        .unwrap_err();
    assert!(format!("{error:#}").contains("shadows the trusted Lean installation"));
    let error = broker
        .regress(
            &tool,
            &candidate,
            &[("Lean.lean", ""), ("Target.lean", candidate.source())],
        )
        .err()
        .expect("installation shadow must fail regression");
    assert!(format!("{error:#}").contains("shadows the trusted Lean installation"));
    let error = broker.probe(&tool, &shadow_target, &proof).unwrap_err();
    assert!(format!("{error:#}").contains("shadows the trusted Lean installation"));
}

#[test]
#[ignore = "set AI4MATH_TEST_LEAN to the native Lean executable and run explicitly"]
fn native_lean_replay_rejects_kernel_bypasses_unsafe_partial_and_elaborator_declarations() {
    let tool = native();
    let broker = Broker::new();
    let commit = "a".repeat(40);
    let trial = |dependency: (&str, &str), statement: &str, body: &str| {
        let target = make_target(&format!("import Bad\ntheorem checked : {statement} := "));
        broker
            .verify_with_dependencies(
                &tool,
                &target,
                &make_proof(&target, body),
                &commit,
                &[dependency],
            )
            .unwrap_err()
    };

    let skipped = trial(
        (
            "Bad.lean",
            "import Lean\nset_option debug.skipKernelTC true in\n\
             theorem Bad.false : False := True.intro\n",
        ),
        "False",
        "Bad.false",
    );
    assert!(
        format!("{skipped:#}").contains("Type mismatch"),
        "{skipped:#}"
    );

    let unsafe_dependency = trial(
        (
            "Bad.lean",
            "import Lean\nunsafe def Bad.id (n : Nat) : Nat := n\n\
             set_option debug.skipKernelTC true in\n\
             def Bad.alias : Nat := Bad.id 0\n",
        ),
        "Bad.alias = 0",
        "by rfl",
    );
    assert!(
        format!("{unsafe_dependency:#}").contains("unsafe or partial dependency 'Bad.id'"),
        "{unsafe_dependency:#}"
    );

    let partial_dependency = trial(
        (
            "Bad.lean",
            "import Lean\npartial def Bad.loop (n : Nat) : Nat := Bad.loop n\n",
        ),
        "Bad.loop 0 = Bad.loop 0",
        "by rfl",
    );
    assert!(
        format!("{partial_dependency:#}").contains("unsafe or partial dependency 'Bad.loop'"),
        "{partial_dependency:#}"
    );

    let elaborated = trial(
        (
            "Bad.lean",
            r#"import Lean
open Lean Elab Command
set_option debug.skipKernelTC true
elab "ai4math_forge_bad" : command => do
  liftCoreM <| Lean.addDecl (.thmDecl {
    name := `Forged.bad
    levelParams := []
    type := mkConst ``False
    value := mkConst ``True.intro
  })
ai4math_forge_bad
"#,
        ),
        "False",
        "Forged.bad",
    );
    assert!(
        format!("{elaborated:#}").contains("while replaying declaration 'Forged.bad'"),
        "{elaborated:#}"
    );
}

#[test]
#[ignore = "set AI4MATH_TEST_LEAN to the native Lean executable and run explicitly"]
fn native_lean_module_replay_rejects_public_bypasses_in_checks_and_downstream_regression() {
    let tool = native();
    let broker = Broker::new();
    let commit = "a".repeat(40);
    let target = make_target("theorem checked : True := ");
    let candidate = broker
        .verify_with_dependencies(
            &tool,
            &target,
            &make_proof(&target, "True.intro"),
            &commit,
            &[],
        )
        .unwrap();

    for (source, diagnostic) in [
        (
            "import Lean\nset_option debug.skipKernelTC true in\n\
             theorem Bad.false : False := True.intro\n",
            "Type mismatch",
        ),
        (
            "import Lean\nunsafe def Bad.id (n : Nat) : Nat := n\n",
            "unsafe or partial public declaration 'Bad.id'",
        ),
        (
            "import Lean\npartial def Bad.loop (n : Nat) : Nat := Bad.loop n\n",
            "unsafe or partial public declaration 'Bad.loop'",
        ),
        (
            "import Lean\naxiom Bad.assumption : Prop\n",
            "non-kernel public axiom 'Bad.assumption'",
        ),
        (
            r#"import Lean
open Lean Elab Command
set_option debug.skipKernelTC true
elab "ai4math_forge_bad" : command => do
  liftCoreM <| Lean.addDecl (.thmDecl {
    name := `Forged.bad
    levelParams := []
    type := mkConst ``False
    value := mkConst ``True.intro
  })
ai4math_forge_bad
"#,
            "while replaying declaration 'Forged.bad'",
        ),
    ] {
        let checked = broker.check_modules(&tool, &[("Later.lean", source)]).err();
        assert!(
            checked.is_some_and(|error| format!("{error:#}").contains(diagnostic)),
            "check_modules accepted unsafe source: {source}"
        );
        let regressed = broker
            .regress(
                &tool,
                &candidate,
                &[("Target.lean", candidate.source()), ("Later.lean", source)],
            )
            .err();
        assert!(
            regressed.is_some_and(|error| format!("{error:#}").contains(diagnostic)),
            "regress accepted unsafe downstream source: {source}"
        );
    }
}
