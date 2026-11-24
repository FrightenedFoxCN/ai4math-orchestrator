//! End-to-end tests through the native Lean broker sandbox.
#![cfg(target_os = "macos")]

use ai4math_broker::{
    Broker, ReceiptTier, RecheckAssessment, Toolchain, VerifiedCandidate,
    lean_breaker_sources_sha256,
};
use ai4math_core::{Engine, FrozenTarget, ProofBodySubmission, render_proof_body};
use std::{fs, path::Path, process::Command};

#[test]
#[ignore = "set AI4MATH_TEST_LEAN to the native Lean executable and run explicitly"]
fn native_reviewed_spec_comparison_checks_exact_kernel_type_and_origin() {
    let lean = std::env::var_os("AI4MATH_TEST_LEAN")
        .or_else(|| std::env::var_os("AI4MATH_TEST_NATIVE_LEAN"))
        .expect("native Lean path");
    let tool = Toolchain::lean(lean).unwrap();
    let broker = Broker::new();
    let commit = "a".repeat(40);
    let support = ("Support.lean", "def seed : Nat := 2\n");
    let spec = (
        "Spec.lean",
        r#"import Support
def Contract.reviewed : Prop := seed = 2
def Contract.distinct : Prop := True
def Contract.notMonomorphic (n : Nat) : Prop := n = 2
axiom specAxiom : Prop
def Contract.notAxiomFree : Prop := specAxiom
private def Contract.notPublic : Prop := seed = 2
#eval IO.println "ai4math:sealed-lean-spec-comparison:v1"
"#,
    );
    let later = ("Later.lean", "import Spec\n");
    let deps = [support, spec, later];
    let verify = |header: &str, deps: &[(&str, &str)]| {
        let target = FrozenTarget {
            engine: Engine::Lean4,
            target_id: "Candidate.checked".into(),
            file_path: "Target.lean".into(),
            prefix: format!("import Support\n\n{header}"),
            suffix: "\n".into(),
        };
        let proof = ProofBodySubmission {
            target_id: target.target_id.clone(),
            proof_body: "by rfl".into(),
        };
        broker
            .verify_with_dependencies(&tool, &target, &proof, &commit, deps)
            .expect("the candidate should itself be provable")
    };
    let header = "theorem Candidate.checked : (fun n : Nat => n = 2) seed := ";
    let candidate = verify(header, &deps);
    let compare = |candidate: &VerifiedCandidate, file, decl| {
        broker.compare_lean_spec(&tool, candidate, file, decl, &deps)
    };
    let result = compare(&candidate, "Spec.lean", "Contract.reviewed").unwrap();
    assert_eq!(result.spec_file(), "Spec.lean");
    assert_eq!(result.spec_decl(), "Contract.reviewed");
    assert_eq!(result.base_commit(), commit);
    assert_eq!(
        result.spec_prefix_sha256(),
        lean_breaker_sources_sha256(&deps[..2])
    );
    assert_eq!(result.candidate_file(), candidate.receipt().file_path());
    assert_eq!(result.candidate_decl(), candidate.receipt().target_id());
    assert_eq!(
        result.candidate_source_sha256(),
        candidate.receipt().patch_sha256()
    );
    assert_eq!(
        result.candidate_receipt_sha256(),
        candidate.receipt().receipt_sha256()
    );
    assert_eq!(
        candidate.receipt().dependencies_sha256(),
        lean_breaker_sources_sha256(&deps)
    );

    let same = verify(header, &deps);
    let again = compare(&same, "Spec.lean", "Contract.reviewed").unwrap();
    assert_eq!(
        again.candidate_receipt_sha256(),
        same.receipt().receipt_sha256()
    );
    assert_eq!(again.receipt_sha256(), result.receipt_sha256());

    for (file, decl, diagnostic) in [
        // Provability and the build-stage success marker don't authorize the
        // comparison: True and the equality are not definitionally equal.
        ("Spec.lean", "Contract.distinct", "not definitionally equal"),
        (
            "Later.lean",
            "Contract.reviewed",
            "not a public declaration",
        ),
        (
            "Spec.lean",
            "Contract.notPublic",
            "not a public declaration",
        ),
        (
            "Spec.lean",
            "Contract.notMonomorphic",
            "monomorphic definition of type Prop",
        ),
        ("Spec.lean", "Contract.notAxiomFree", "non-kernel axiom"),
        (
            "Target.lean",
            "Candidate.checked",
            "distinct from the candidate",
        ),
        (
            "Absent.lean",
            "Contract.reviewed",
            "ordered candidate dependencies",
        ),
    ] {
        let error = compare(&candidate, file, decl).unwrap_err();
        assert!(
            format!("{error:#}").contains(diagnostic),
            "{file}/{decl}: {error:#}"
        );
    }
    let reordered = [support, later, spec];
    let changed = [support, spec, (later.0, "import Spec\n-- changed\n")];
    for deps in [&reordered, &changed] {
        let error = broker
            .compare_lean_spec(&tool, &candidate, "Spec.lean", "Contract.reviewed", deps)
            .unwrap_err();
        assert!(format!("{error:#}").contains("differ from the verified candidate"));
    }

    // Elaborated assignment can insert this implicit term argument; the raw
    // native check must compare the forall type of the bare constant instead.
    let with_implicit = verify("theorem Candidate.checked {h : True} : seed = 2 := ", &deps);
    let error = compare(&with_implicit, "Spec.lean", "Contract.reviewed").unwrap_err();
    assert!(
        format!("{error:#}").contains("not definitionally equal"),
        "{error:#}"
    );
}

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgSign=false",
        ])
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[test]
#[ignore = "set AI4MATH_TEST_LEAN to the native Lean executable and run explicitly"]
fn native_reviewed_spec_comparison_binds_assessed_t3_to_new_baseline() {
    let lean = std::env::var_os("AI4MATH_TEST_LEAN")
        .or_else(|| std::env::var_os("AI4MATH_TEST_NATIVE_LEAN"))
        .expect("native Lean path");
    let tool = Toolchain::lean(lean).unwrap();
    let broker = Broker::new();
    let target = FrozenTarget {
        engine: Engine::Lean4,
        target_id: "Candidate.checked".into(),
        file_path: "Target.lean".into(),
        prefix: "import Later\n\ntheorem Candidate.checked : current = 2 := ".into(),
        suffix: "\n".into(),
    };
    let proof = ProofBodySubmission {
        target_id: target.target_id.clone(),
        proof_body: "by rfl".into(),
    };
    let rendered = render_proof_body(&target, &proof).unwrap();
    let repo = tempfile::tempdir().unwrap();
    let root = repo.path();
    git(root, &["init", "-q"]);
    git(root, &["config", "user.name", "Native fixture"]);
    git(root, &["config", "user.email", "fixture@example.invalid"]);
    git(root, &["config", "core.autocrlf", "false"]);
    for (path, source) in [
        ("Support.lean", "def seed : Nat := 2\n"),
        (
            "Spec.lean",
            "import Support\n\ndef Contract.reviewed : Prop := seed = 2\n",
        ),
        (
            "Later.lean",
            "import Support\n\ndef current : Nat := seed\n",
        ),
        ("Target.lean", rendered.as_str()),
    ] {
        fs::write(root.join(path), source).unwrap();
    }
    git(
        root,
        &[
            "add",
            "--",
            "Support.lean",
            "Spec.lean",
            "Later.lean",
            "Target.lean",
        ],
    );
    git(
        root,
        &["commit", "-qm", "Commit original theorem and dependencies"],
    );
    let original_commit = git(root, &["rev-parse", "HEAD"]).trim().to_owned();
    fs::write(
        root.join("Later.lean"),
        "import Support\n\ndef current : Nat := 1 + 1\n",
    )
    .unwrap();
    git(root, &["add", "--", "Later.lean"]);
    git(root, &["commit", "-qm", "Change only the later dependency"]);
    let rebuilt_commit = git(root, &["rev-parse", "HEAD"]).trim().to_owned();
    assert_ne!(original_commit, rebuilt_commit);

    let blob = |commit: &str, path: &str| git(root, &["show", &format!("{commit}:{path}")]);
    let read_deps = |commit: &str| {
        ["Support.lean", "Spec.lean", "Later.lean"].map(|path| (path, blob(commit, path)))
    };
    let original_deps = read_deps(&original_commit);
    let rebuilt_deps = read_deps(&rebuilt_commit);
    assert_eq!(original_deps[..2], rebuilt_deps[..2]);
    assert_ne!(original_deps[2], rebuilt_deps[2]);
    let original_deps = original_deps
        .each_ref()
        .map(|(path, text)| (*path, text.as_str()));
    let rebuilt_deps = rebuilt_deps
        .each_ref()
        .map(|(path, text)| (*path, text.as_str()));
    let committed = blob(&original_commit, "Target.lean");
    assert_eq!(committed, blob(&rebuilt_commit, "Target.lean"));

    let original = broker
        .verify_with_dependencies(&tool, &target, &proof, &original_commit, &original_deps)
        .expect("old theorem should pass T1");
    let recheck = broker.assess_recheck_with_dependencies(
        &tool,
        &target,
        &proof,
        &rebuilt_commit,
        &rebuilt_deps,
    );
    let RecheckAssessment::Verified(rebuilt) = recheck else {
        panic!("committed theorem should survive T3: {recheck:?}");
    };
    let compare = |candidate: &VerifiedCandidate, deps: &[(&str, &str)]| {
        broker.compare_lean_spec(&tool, candidate, "Spec.lean", "Contract.reviewed", deps)
    };
    let previous = compare(&original, &original_deps).expect("old T1 comparison");
    let fresh = compare(&rebuilt, &rebuilt_deps).expect("new assessed T3 comparison");
    assert_eq!(original.receipt().tier(), ReceiptTier::T1Promotion);
    assert_eq!(rebuilt.receipt().tier(), ReceiptTier::T3TaintRebuild);
    assert_eq!(previous.base_commit(), original_commit);
    assert_eq!(fresh.base_commit(), rebuilt_commit);
    let prefix = lean_breaker_sources_sha256(&original_deps[..2]);
    for (candidate, comparison, deps) in [
        (&original, &previous, &original_deps),
        (&rebuilt, &fresh, &rebuilt_deps),
    ] {
        let receipt = candidate.receipt();
        assert_eq!(candidate.source(), committed);
        assert_eq!(
            receipt.dependencies_sha256(),
            lean_breaker_sources_sha256(deps)
        );
        assert_eq!(comparison.spec_prefix_sha256(), prefix);
        assert_eq!(comparison.candidate_file(), receipt.file_path());
        assert_eq!(comparison.candidate_decl(), receipt.target_id());
        assert_eq!(comparison.candidate_source_sha256(), receipt.patch_sha256());
        assert_eq!(
            comparison.candidate_receipt_sha256(),
            receipt.receipt_sha256()
        );
    }
    assert_ne!(previous.receipt_sha256(), fresh.receipt_sha256());
    assert_ne!(
        previous.candidate_receipt_sha256(),
        fresh.candidate_receipt_sha256()
    );
    for (candidate, stale_deps) in [(&original, &rebuilt_deps), (&rebuilt, &original_deps)] {
        let error = compare(candidate, stale_deps).unwrap_err();
        assert!(
            format!("{error:#}").contains("comparison dependencies differ"),
            "{error:#}"
        );
    }
}
