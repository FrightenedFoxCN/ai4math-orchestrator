//! Exercise the reconstruction and independent audit through the native sandbox.
#![cfg(target_os = "macos")]

use ai4math_broker::{AgdaMode, Broker, HammerPremises, MAX_HAMMER_DEPENDENCIES, Toolchain};
use ai4math_core::{Engine, FrozenTarget, ProofBodySubmission, render_proof_body};
use sha2::{Digest, Sha256};

fn native() -> std::ffi::OsString {
    std::env::var_os("AI4MATH_TEST_LEAN")
        .or_else(|| std::env::var_os("AI4MATH_TEST_NATIVE_LEAN"))
        .expect("native Lean path")
}

fn premises(names: &[&str]) -> HammerPremises {
    HammerPremises::parse(
        &serde_json::to_vec(&serde_json::json!({
            "version": 1, "premises": names
        }))
        .unwrap(),
    )
    .unwrap()
}

fn target(prefix: &str) -> FrozenTarget {
    FrozenTarget {
        engine: Engine::Lean4,
        target_id: "Goal.associate".into(),
        file_path: "Target.lean".into(),
        prefix: prefix.into(),
        suffix: "\n".into(),
    }
}

#[test]
#[ignore = "set AI4MATH_TEST_LEAN to the native Lean executable and run explicitly"]
fn native_lean_hammer_uses_exact_names_and_needs_multiple_premises() {
    let tool = Toolchain::lean(native()).unwrap();
    let broker = Broker::new();
    let commit = "a".repeat(40);
    let deps = [(
        "Hints.lean",
        r#"import Lean
theorem Hints.step (n m k : Nat) : (n + m + k = n + (m + k)) → n + (m + k) = n + m + k := Eq.symm
theorem Hints.seed' (n m k : Nat) : n + m + k = n + (m + k) := Nat.add_assoc n m k
theorem Hints.choiceSeed (n m k : Nat) : n + m + k = n + (m + k) :=
  Classical.choice (Nonempty.intro (Nat.add_assoc n m k))
theorem Shadow.Hints.seed' (n : Nat) : n = n := rfl
namespace Spoof
export Hints (seed')
end Spoof
"#,
    )];
    let goal = target(
        "import Hints\nopen Shadow\ntheorem Goal.associate (n m k : Nat) : n + (m + k) = n + m + k := ",
    );
    let reconstruct = |names: &[&str]| {
        broker.reconstruct_lean_hammer(&tool, &goal, &premises(names), &commit, &deps)
    };
    let candidate = reconstruct(&["Hints.step", "Hints.seed'", "Nat.zero_le"]).unwrap();
    assert_eq!(candidate.target_id, goal.target_id);
    assert_eq!(candidate.file_path, goal.file_path);
    assert_eq!(candidate.base_commit, commit);
    assert!(candidate.observed_axioms.is_empty());
    assert_eq!(
        candidate.proof_body,
        "by\n  solve_by_elim (maxDepth := 4) -constructor -intro -symm -exfalso -suggestions only [_root_.Hints.step, _root_.Hints.seed', _root_.Nat.zero_le]"
    );
    let source = render_proof_body(
        &goal,
        &ProofBodySubmission {
            target_id: candidate.target_id.clone(),
            proof_body: candidate.proof_body.clone(),
        },
    )
    .unwrap();
    assert_eq!(
        candidate.source_sha256,
        format!("{:x}", Sha256::digest(source.as_bytes()))
    );
    assert_eq!(
        candidate.dependencies_sha256,
        ai4math_broker::lean_breaker_sources_sha256(&deps)
    );
    let output = serde_json::to_value(&candidate).unwrap();
    assert!(output.get("receipt").is_none());
    assert!(output.get("receipt_sha256").is_none());
    assert!(output.get("tier").is_none());
    assert_eq!(
        reconstruct(&["Hints.step", "Hints.choiceSeed"])
            .unwrap()
            .observed_axioms,
        ["Classical.choice"]
    );

    for wrong in [
        &["Hints.step"][..],
        &["Hints.seed'"][..],
        &["Hints.step", "Shadow.Hints.seed'"][..],
        &["Hints.step", "Hints.seed'", "Hints.unknownUnused"][..],
        &["Hints.step", "Spoof.seed'"][..],
    ] {
        assert!(
            reconstruct(wrong).is_err(),
            "{wrong:?} must not reconstruct"
        );
    }
}

#[test]
#[ignore = "set AI4MATH_TEST_LEAN to the native Lean executable and run explicitly"]
fn native_lean_hammer_refuses_bypass_assumptions_and_mismatched_inputs() {
    let tool = Toolchain::lean(native()).unwrap();
    let broker = Broker::new();
    let commit = "a".repeat(40);
    let trial = |statement, hints, dependency| {
        let goal = target(&format!(
            "import Hints\ntheorem Goal.associate : {statement} := "
        ));
        broker.reconstruct_lean_hammer(
            &tool,
            &goal,
            &premises(&[hints]),
            &commit,
            &[("Hints.lean", dependency)],
        )
    };
    for (goal, hint, dependency) in [
        (
            "False",
            "Bypass.carry",
            "import Lean\naxiom Bypass.falsehood : False\ntheorem Bypass.carry : False := Bypass.falsehood\n",
        ),
        (
            "False",
            "Bypass.hole",
            "import Lean\ntheorem Bypass.hole : False := by sorry\n",
        ),
        (
            "1 + 1 = 2",
            "Bypass.native",
            "import Lean\ntheorem Bypass.native : (1 : Nat) + 1 = 2 := by native_decide\n",
        ),
    ] {
        let error = trial(goal, hint, dependency).unwrap_err();
        assert!(
            format!("{error:#}").contains("non-kernel axiom")
                || format!("{error:#}").contains("non-L5 Lean hammer axiom"),
            "{error:#}"
        );
    }

    let goal = target("import Lean\ntheorem Goal.associate : True := ");
    let hints = premises(&["Nat.zero_le"]);
    let mismatch = Toolchain::agda(native(), AgdaMode::WithoutK).unwrap();
    assert!(
        broker
            .reconstruct_lean_hammer(&mismatch, &goal, &hints, &commit, &[])
            .is_err()
    );
    assert!(
        broker
            .reconstruct_lean_hammer(&tool, &goal, &hints, "HEAD", &[])
            .is_err()
    );
    assert!(
        broker
            .reconstruct_lean_hammer(&tool, &goal, &hints, &commit, &[("Init.lean", "")])
            .is_err()
    );
    let deps = [("Unused.lean", ""); MAX_HAMMER_DEPENDENCIES + 1];
    let error = broker
        .reconstruct_lean_hammer(&tool, &goal, &hints, &commit, &deps)
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("too many Lean hammer dependencies")
    );
}
