use ai4math_core::{
    Engine, FrozenTarget, PreFlightScanResult, ProofBodyError, ProofBodySubmission,
    render_proof_body, render_proof_body_allow_oracle, render_proof_body_allow_sorry, scan_source,
    validate_source_path,
};

fn lean_target() -> FrozenTarget {
    FrozenTarget {
        engine: Engine::Lean4,
        target_id: "target".into(),
        file_path: "Fixture.lean".into(),
        prefix: "import Lean

theorem target (n : Nat) : n = n := "
            .into(),
        suffix: "\r\n\n".into(),
    }
}

fn agda_target() -> FrozenTarget {
    FrozenTarget {
        engine: Engine::Agda,
        target_id: "target".into(),
        file_path: "Fixture.agda".into(),
        prefix: "module Fixture where
  open import Agda.Builtin.Equality
  target : 0 ≡ 0
  target = "
            .into(),
        suffix: "
"
        .into(),
    }
}

fn submit(target: &FrozenTarget, body: &str) -> Result<String, ProofBodyError> {
    render_proof_body(
        target,
        &ProofBodySubmission {
            target_id: target.target_id.clone(),
            proof_body: body.into(),
        },
    )
}

#[test]
fn path_check_rejects_non_source_and_noncanonical_paths() {
    assert!(validate_source_path(Engine::Lean4, "Library/Sub/Identity.lean").is_ok());
    assert!(validate_source_path(Engine::Agda, "Fixture.agda").is_ok());
    assert!(validate_source_path(Engine::Rocq, "Fixture.v").is_ok());
    for path in [
        "/Main.lean",
        "./Main.lean",
        "../Main.lean",
        "A/../Main.lean",
        "A//Main.lean",
        "A\\Main.lean",
        "A/.git/Main.lean",
        ".runtime/Main.lean",
        "docs/ADR/Main.lean",
        "lakefile.lean",
        "A/LAKEFILE.lean",
        "Main.agda",
        "Main.lean/",
    ] {
        assert!(
            validate_source_path(Engine::Lean4, path).is_err(),
            "{path:?}"
        );
    }
}

#[test]
fn digest_binds_exact_source_bytes_and_reports_no_authorization() {
    let result = scan_source(Engine::Lean4, "Fixture.lean", "abc");
    assert!(result.passed);
    assert!(!result.authorized_scope_checked);
    assert_eq!(
        result.patch_sha256,
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert_ne!(
        result.patch_sha256,
        scan_source(
            Engine::Lean4,
            "Fixture.lean",
            "abc
"
        )
        .patch_sha256
    );
    let json = serde_json::to_value(&result).unwrap();
    assert_eq!(json["engine"], "lean4");
    let mut oracle_report = json.clone();
    oracle_report["engine"] = serde_json::json!("python");
    assert!(serde_json::from_value::<PreFlightScanResult>(oracle_report).is_ok());
    assert_eq!(
        serde_json::from_value::<PreFlightScanResult>(json).unwrap(),
        result
    );
}

#[test]
fn scanner_skips_comments_and_ordinary_literals_and_reports_visible_tokens() {
    let benign = "-- sorry\r\n/- outer /- admit -/ -/
 def message := \"axiom\"
 def sorryCount := 0
";
    assert!(scan_source(Engine::Lean4, "Fixture.lean", benign).passed);
    assert!(
        scan_source(
            Engine::Agda,
            "Fixture.agda",
            "{- postulate {- nested -} -}
"
        )
        .passed
    );
    assert!(
        scan_source(
            Engine::Rocq,
            "Fixture.v",
            "(* Admitted. (* nested *) *)
"
        )
        .passed
    );
    let result = scan_source(
        Engine::Lean4,
        "Fixture.lean",
        "-- comment
/- nested
/- comment -/ -/
 sorry
",
    );
    assert!(!result.passed);
    assert_eq!(result.violations.len(), 1);
    let json = serde_json::to_value(&result.violations[0]).unwrap();
    assert_eq!(json["rule_id"], "LEAN_TRUST");
    assert_eq!(json["severity"], "DENY");
    assert_eq!(json["line"], 4);
    assert_eq!(json["matched_construct"], "sorry");
}

#[test]
fn scanner_rejects_incomplete_lexical_constructs_and_unsupported_language_features() {
    let inputs = [
        (
            Engine::Lean4,
            "Fixture.lean",
            "/- unfinished comment",
            "SOURCE_LEX",
        ),
        (
            Engine::Lean4,
            "Fixture.lean",
            "\"unfinished string",
            "SOURCE_LEX",
        ),
        (Engine::Lean4, "Fixture.lean", "«identifier»", "SOURCE_LEX"),
        (
            Engine::Agda,
            "Fixture.agda",
            "{-# OPTIONS --safe #-}",
            "AGDA_PRAGMA",
        ),
        (Engine::Agda, "Fixture.agda", "postulate", "AGDA_TRUST"),
        (Engine::Rocq, "Fixture.v", "Admitted.", "ROCQ_TRUST"),
    ];
    for (engine, path, source, rule) in inputs {
        let result = scan_source(engine, path, source);
        assert!(!result.passed, "{engine}: {source:?}");
        assert_eq!(result.violations[0].rule_id, rule);
    }
}

#[test]
fn renders_lean_tactic_and_term_without_changing_frozen_spans() {
    let target = lean_target();
    let source = submit(&target, "    by\r\n      rfl\r\n").unwrap();
    assert_eq!(
        source,
        format!(
            "{} (
  by
    rfl
  ){}",
            target.prefix, target.suffix
        )
    );
    let candidate = submit(&target, "Eq.refl n -- reflexivity").unwrap();
    assert!(candidate.starts_with(&target.prefix));
    assert!(candidate.ends_with(&target.suffix));
    assert!(candidate.contains(
        "-- reflexivity
  )"
    ));
}

#[test]
fn renders_an_indented_agda_expression_after_a_frozen_signature() {
    let target = agda_target();
    let source = submit(&target, "refl").unwrap();
    assert_eq!(
        source,
        format!(
            "{} (
    refl
    ){}",
            target.prefix, target.suffix
        )
    );
}

#[test]
fn rejects_targets_without_a_matching_trailing_trusted_declaration() {
    let mut target = lean_target();
    let wrong_id = ProofBodySubmission {
        target_id: "other".into(),
        proof_body: "rfl".into(),
    };
    assert_eq!(
        render_proof_body(&target, &wrong_id),
        Err(ProofBodyError::TargetMismatch)
    );
    target.prefix = "theorem targetLonger : True := ".into();
    assert!(matches!(
        submit(&target, "True.intro"),
        Err(ProofBodyError::InvalidTemplate(_))
    ));
    target.prefix = "theorem target : True := ".into();
    target.suffix = "
-- Only a trailing whitespace suffix is supported.
"
    .into();
    assert!(matches!(
        submit(&target, "True.intro"),
        Err(ProofBodyError::InvalidTemplate(_))
    ));
    let mut agda = agda_target();
    agda.prefix = "module Fixture where
  target = "
        .into();
    assert!(matches!(
        submit(&agda, "refl"),
        Err(ProofBodyError::InvalidTemplate(_))
    ));
    agda.prefix = "module Other where
  target : 0 ≡ 0
  target = "
        .into();
    assert!(matches!(
        submit(&agda, "refl"),
        Err(ProofBodyError::InvalidTemplate(_))
    ));
}

#[test]
fn denies_out_of_profile_tactic_constructs_without_running_them() {
    // Bare construct names only: admission rejects these before any compiler invocation.
    for (target, fragment, rule) in [
        (lean_target(), "by run_tac", "LEAN_EXEC"),
        (lean_target(), "by_elab", "LEAN_EXEC"),
        (lean_target(), "eval%", "LEAN_EXEC"),
        (agda_target(), "tactic", "AGDA_REFLECTION"),
        (agda_target(), "unquote", "AGDA_REFLECTION"),
    ] {
        assert!(
            matches!(submit(&target, fragment), Err(ProofBodyError::PreFlight(e))
            if e.rule_id == rule),
            "{fragment:?}"
        );
    }
    assert!(
        submit(
            &lean_target(),
            "by
  exact Eq.refl n -- run_tac"
        )
        .is_ok()
    );
    assert!(submit(&agda_target(), "refl -- tactic").is_ok());
}

#[test]
fn rejects_unsupported_or_missing_proof_syntax() {
    let target = lean_target();
    for fragment in [
        "",
        "-- comment",
        "(rfl",
        "{[rfl)]}",
        "\"text\"",
        "{{PROOF_BODY}}",
        "by	 rfl",
    ] {
        assert!(
            matches!(
                submit(&target, fragment),
                Err(ProofBodyError::InvalidBody { .. })
            ),
            "{fragment:?}"
        );
    }
    assert!(matches!(
        submit(&target, "by sorry"),
        Err(ProofBodyError::PreFlight(_))
    ));
    assert!(matches!(
        submit(&target, &" ".repeat(256 * 1024 + 1)),
        Err(ProofBodyError::InvalidBody { .. })
    ));
}

#[test]
fn formal_sketch_render_admits_only_explicit_sorry_holes() {
    let target = lean_target();
    let submission = ProofBodySubmission {
        target_id: "target".into(),
        proof_body: "by sorry".into(),
    };
    assert!(render_proof_body_allow_sorry(&target, &submission).is_ok());
    assert!(matches!(
        render_proof_body(&target, &submission),
        Err(ProofBodyError::PreFlight(_))
    ));
    for body in ["axiom bypass : True", "by native_decide", "by run_tac"] {
        let submission = ProofBodySubmission {
            target_id: "target".into(),
            proof_body: body.into(),
        };
        assert!(
            matches!(
                render_proof_body_allow_sorry(&target, &submission),
                Err(ProofBodyError::PreFlight(_))
            ),
            "{body}"
        );
    }
}

#[test]
fn oracle_backed_render_admits_only_compiler_trust_constructs() {
    let target = lean_target();
    let oracle = ProofBodySubmission {
        target_id: "target".into(),
        proof_body: "by native_decide".into(),
    };
    assert!(render_proof_body_allow_oracle(&target, &oracle).is_ok());
    assert!(matches!(
        render_proof_body(&target, &oracle),
        Err(ProofBodyError::PreFlight(_))
    ));
    for body in ["by sorry", "axiom bypass : True", "by run_tac"] {
        let submission = ProofBodySubmission {
            target_id: "target".into(),
            proof_body: body.into(),
        };
        assert!(
            matches!(
                render_proof_body_allow_oracle(&target, &submission),
                Err(ProofBodyError::PreFlight(_))
            ),
            "{body}"
        );
    }
}

fn rocq_target() -> FrozenTarget {
    FrozenTarget {
        engine: Engine::Rocq,
        target_id: "target".into(),
        file_path: "Fixture.v".into(),
        prefix: "Require Import Earlier.\nTheorem target (n : nat) : Earlier.identity n = n.\nProof.\n  exact (".into(),
        suffix: ").\nQed.\r\n".into(),
    }
}

#[test]
fn renders_rocq_term_in_exactly_the_frozen_wrapper() {
    let target = rocq_target();
    let body = "(* nested (* harmless *) text *)\n@eq_refl nat n";
    let candidate = submit(&target, body).unwrap();
    assert_eq!(
        candidate,
        format!("{}\n{body}\n{}", target.prefix, target.suffix)
    );
    assert!(scan_source(Engine::Rocq, &target.file_path, &candidate).passed);
    let trivial = FrozenTarget {
        prefix: "Theorem target : True.\nProof.\n  exact (".into(),
        ..target
    };
    assert!(submit(&trivial, "I").is_ok());
    assert_eq!(
        render_proof_body(
            &trivial,
            &ProofBodySubmission {
                target_id: "Target".into(),
                proof_body: "I".into()
            }
        ),
        Err(ProofBodyError::TargetMismatch),
    );
}

#[test]
fn rocq_root_theorem_id_wrapper_and_immutable_tail_are_strict() {
    let target = rocq_target();
    for (prefix, suffix, id) in [
        (
            "Theorem targetLonger : True. Proof. exact (",
            "). Qed.\n",
            "target",
        ),
        (
            "Theorem target : True. Proof. exact (",
            "). Qed.\n",
            "Fixture.target",
        ),
        (
            "Theorem target : True. Proof. exact (",
            "). Qed.\n",
            "Target",
        ),
        (
            "Theorem target.Nested : True. Proof. exact (",
            "). Qed.\n",
            "target",
        ),
        ("Lemma target : True. Proof. exact (", "). Qed.\n", "target"),
        ("Theorem target. Proof. exact (", "). Qed.\n", "target"),
        (
            "Theorem target : True := I. Proof. exact (",
            "). Qed.\n",
            "target",
        ),
        (
            "Theorem target : True. Proof using Type. exact (",
            "). Qed.\n",
            "target",
        ),
        ("Theorem target : True. Proof. exact ", ". Qed.\n", "target"),
        (
            "Theorem target : True. Proof. exact (",
            "). Defined.\n",
            "target",
        ),
        (
            "Theorem target : True. Proof. exact (",
            "). Qed. Definition trailing := I.\n",
            "target",
        ),
        (
            "Theorem target : True. Proof. exact (",
            "). Qed. (* {{PROOF_BODY}} *)\n",
            "target",
        ),
        (
            "Module Wrapped. Theorem target : True. Proof. exact (",
            "). Qed.\n",
            "target",
        ),
        (
            "Section Wrap. Theorem target : True. Proof. exact (",
            "). Qed.\n",
            "target",
        ),
    ] {
        let attempted = FrozenTarget {
            target_id: id.into(),
            prefix: prefix.into(),
            suffix: suffix.into(),
            ..target.clone()
        };
        assert!(submit(&attempted, "I").is_err(), "{attempted:?}");
    }
}

#[test]
fn rocq_term_cannot_escape_or_invoke_tactics_or_literals() {
    let target = rocq_target();
    for body in [
        "",
        "(* only a comment *)",
        "(* unterminated",
        "I *)",
        "(I",
        "((I])",
        "I). Qed. Theorem target : True. Proof. exact (I",
        "I. Qed",
        "I.(projection)",
        "Earlier . identity",
        "Earlier.(* no *)identity",
        "(I); ltac:(idtac)",
        "ltac (* still a tactic *) : (exact I)",
        "ltac2:(())",
        "Native.native_cast_no_check I",
        "vm_compute",
        "unsafe",
        "admit",
        "?goal",
        "\"Closed under the global context\"",
        "#[local] I",
        "I (* \"*)\" *)",
        "I\t",
        "Ⅰ",
        "{{PROOF_BODY}}",
    ] {
        assert!(
            matches!(
                submit(&target, body),
                Err(ProofBodyError::InvalidBody { .. })
            ),
            "{body:?}"
        );
    }
}

#[test]
fn rocq_preflight_admits_ordinary_root_dependencies() {
    let source = "From Corelib.Init Require Import Logic Datatypes.\n\
        (* Proof. Qed. (* these are inside comments *) *)\n\
        Definition identity (n : nat) : nat := n.\n\
        Definition three := identity 3.\n\
        Definition either (P Q : Prop) : Prop := P /\\ Q \\/ P.\n\
        Lemma identity_correct (n : nat) : identity n = n.\n\
        Proof. exact (@eq_refl nat n). Qed.\n";
    let report = scan_source(Engine::Rocq, "Earlier.v", source);
    assert!(report.passed, "{:?}", report.violations);
    for invalid in ["Nested/Earlier.v", "earlier.v", "Earlier-Path.v"] {
        assert!(validate_source_path(Engine::Rocq, invalid).is_err());
    }
}

#[test]
fn rocq_preflight_denies_every_non_profile_vernacular_form() {
    for source in [
        "Unset Guard Checking.",
        "Set Type in Type.",
        "Unset Positivity Checking.",
        "Unset Universe Checking.",
        "Local Unset Implicit Arguments.",
        "Set Warnings \"-all\".",
        "#[bypass_check(guard)] Definition x : True := I.",
        "#[universes(polymorphic)] Definition x := I.",
        "Axiom fake : False.",
        "Parameter fake : False.",
        "Variable fake : False.",
        "Theorem x : True. Admitted.",
        "Theorem x : True. Proof. Abort.",
        "Goal True. Proof. exact (I). Qed.",
        "Theorem x : True. Proof. exact I. Qed.",
        "Theorem x : True. Proof. exact (I); idtac. Qed.",
        "Theorem x : True. Proof. exact (I) (I). Qed.",
        "Theorem x : True. Proof. exact (I). Defined.",
        "Theorem x : True. Proof. exact (I).",
        "Definition x := I",
        "Definition x : := I.",
        "Definition x := .",
        "Definition x := (I.",
        "Definition x := I.(* comment *)",
        "Definition x := ltac:(exact I).",
        "Definition x := \"literal\".",
        "Definition x := I. (* \"*)\" *)",
        "Print Assumptions target.",
        "Check target.",
        "Eval compute in I.",
        "Redirect \"audit\" Print Assumptions target.",
        "Ltac fake := idtac \"Closed under the global context\".",
        "Declare ML Module \"arbitrary\".",
        "Load \"Other.v\".",
        "Notation \"x\" := I.",
        "Hint Extern 1 => idtac : core.",
        "Time Definition x := I.",
        "Fail Definition x := I.",
        "Require Import.",
        "From Corelib Require Import.",
        "Require Import \"Earlier\".",
    ] {
        let report = scan_source(Engine::Rocq, "Fixture.v", source);
        assert!(!report.passed, "{source:?} passed");
        assert!(report.violations[0].line.is_some(), "{source:?}");
    }
}

#[test]
fn caps_source_expansion_before_allocating_the_rendered_candidate() {
    let mut target = agda_target();
    let indent = " ".repeat(64);
    target.prefix = format!("module Fixture where\n{indent}target : Set\n{indent}target = ");
    let body = "value\n".repeat(32_000);
    assert!(matches!(
        submit(&target, &body),
        Err(ProofBodyError::InvalidBody {
            detail: "rendered source exceeds the Phase 1 size limit",
            ..
        })
    ));
}
