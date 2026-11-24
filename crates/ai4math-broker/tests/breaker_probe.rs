//! Benign end-to-end checks through the native broker sandbox.
#![cfg(target_os = "macos")]

use ai4math_broker::{Broker, Toolchain, lean_breaker_sources_sha256};

#[test]
#[ignore = "set AI4MATH_TEST_LEAN to the native Lean executable and run explicitly"]
fn native_lean_breaker_probes_seal_only_direct_kernel_checked_cases() {
    let lean = std::env::var_os("AI4MATH_TEST_LEAN")
        .or_else(|| std::env::var_os("AI4MATH_TEST_NATIVE_LEAN"))
        .expect("native Lean path");
    let tool = Toolchain::lean(lean).unwrap();
    let broker = Broker::new();
    let commit = "a".repeat(40);
    let verify = |cases: &[&str], sources: &[(&str, &str)]| {
        broker.verify_lean_breaker_probes(&tool, &commit, "Target.lean", "watched", cases, sources)
    };
    let support = ("Support.lean", "def expected : Nat := 2\n");
    let target = (
        "Target.lean",
        "import Support\n\
         def watched (n : Nat) := n + 1\n\
         theorem first : watched 1 = expected := rfl\n\
         namespace Cases\n\
         theorem second : watched 0 = 1 := rfl\n\
         end Cases\n",
    );
    let cases = ["first", "Cases.second"];
    let sources = [support, target];
    let verified = verify(&cases, &sources).unwrap();
    assert_eq!(verified.base_commit(), commit);
    assert_eq!(verified.file_path(), "Target.lean");
    assert_eq!(verified.definition(), "watched");
    assert_eq!(verified.cases(), &cases);
    assert_eq!(
        verified.sources_sha256(),
        lean_breaker_sources_sha256(&sources)
    );
    assert_eq!(
        verified.receipt_sha256(),
        verify(&cases, &sources).unwrap().receipt_sha256()
    );
    assert_ne!(
        verified.receipt_sha256(),
        verify(&[cases[1], cases[0]], &sources)
            .unwrap()
            .receipt_sha256()
    );
    let edited = (support.0, "def expected : Nat := 2\n-- different bytes\n");
    assert_ne!(
        verified.receipt_sha256(),
        verify(&cases, &[edited, target]).unwrap().receipt_sha256()
    );

    for (invalid, source, why) in [
        (
            "invalid",
            "def alias := watched\ntheorem invalid : alias 1 = expected := rfl\n",
            "does not directly reference",
        ),
        (
            "invalid",
            "theorem invalid : True := by have _ : watched 1 = expected := rfl; trivial\n",
            "does not directly reference",
        ),
        (
            "invalid",
            "theorem invalid (watched : Nat) : watched = watched := rfl\n",
            "does not directly reference",
        ),
        (
            "Other.invalid",
            "namespace Other\ndef watched := expected\ntheorem invalid : watched = expected := rfl\nend Other\n",
            "does not directly reference",
        ),
        (
            "invalid",
            "def invalid : watched 1 = expected := rfl\n",
            "must be a theorem",
        ),
        (
            "invalid",
            "axiom assumption : watched 1 = expected\ntheorem invalid : watched 1 = expected := assumption\n",
            "non-kernel axiom",
        ),
        (
            "invalid",
            "theorem invalid : watched 1 = expected := by sorry\n",
            "non-kernel axiom",
        ),
    ] {
        let amended = format!("{}{source}", target.1);
        let err = verify(&[cases[0], invalid], &[support, (target.0, &amended)]).unwrap_err();
        assert!(format!("{err:#}").contains(why), "{source}: {err:#}");
    }
    for (dep, source) in [
        ("def watched := 2\n", "theorem first : watched = 2 := rfl\n"),
        ("theorem first : True := trivial\n", "def watched := 2\n"),
        (
            "def expected := 2\n",
            "private def watched := expected\ntheorem first : watched = expected := rfl\n",
        ),
    ] {
        let source = format!("import Support\n{source}");
        let err = verify(&["first"], &[(support.0, dep), (target.0, &source)]).unwrap_err();
        assert!(
            format!("{err:#}").contains("not a public declaration"),
            "{err:#}"
        );
    }
    let module = (
        "Target.lean",
        "module\n\
         @[expose] public def watched := 2\n\
         public theorem classical : watched = 2 := Classical.choice (Nonempty.intro rfl)\n",
    );
    assert!(verify(&["classical"], &[module]).is_ok());
    assert!(verify(&[], &sources).is_err());
    assert!(verify(&["first", "first"], &sources).is_err());
    assert!(verify(&["first"], &[target, support]).is_err());
    assert!(verify(&["first"], &[("Init.lean", ""), target]).is_err());
}
