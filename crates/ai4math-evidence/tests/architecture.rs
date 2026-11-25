#[test]
fn store_production_dependencies_do_not_include_broker() {
    let manifest = include_str!("../../ai4math-store/Cargo.toml");
    assert!(
        !manifest.contains("ai4math-broker"),
        "ai4math-store must not depend on the native broker in any dependency section"
    );
}

#[test]
fn evidence_crate_has_no_execution_or_storage_dependencies() {
    let manifest = include_str!("../Cargo.toml");
    for forbidden in [
        "ai4math-broker",
        "ai4math-store",
        "rusqlite",
        "libc",
        "fs2",
        "tempfile",
    ] {
        assert!(
            !manifest.contains(forbidden),
            "ai4math-evidence must remain IO-free; found {forbidden}"
        );
    }
}
