#![cfg(unix)]

use ai4math_broker::LEAN_EXPORT_MAX_BYTES;
use sha2::{Digest, Sha256};
use std::{
    fs,
    os::unix::fs::symlink,
    path::Path,
    process::{Command, Output},
};

fn run(root: &Path, file: &str, export_pin: &str, checker_pin: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ai4math"))
        .current_dir(root)
        .args([
            "t2-check-export",
            file,
            export_pin,
            "/no-fixture-checker/nanoda_bin",
            checker_pin,
        ])
        .output()
        .unwrap()
}

fn rejects(out: Output, expected: &str) {
    assert!(!out.status.success() && out.stdout.is_empty());
    let diagnostic = String::from_utf8_lossy(&out.stderr);
    assert!(
        diagnostic.contains(expected),
        "expected {expected:?}: {diagnostic}"
    );
}

#[test]
fn export_file_must_be_local_regular_and_bounded_before_parsing() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path();
    fs::write(p.join("input"), b"test").unwrap();
    fs::create_dir(p.join("directory")).unwrap();
    symlink(p.join("input"), p.join("alias")).unwrap();
    fs::File::create(p.join("too-big"))
        .unwrap()
        .set_len(LEAN_EXPORT_MAX_BYTES as u64 + 1)
        .unwrap();
    let pin = "a".repeat(64);
    for path in ["directory", "alias"] {
        rejects(
            run(p, path, &pin, &pin),
            "must be a regular, non-symlink file",
        );
    }
    rejects(
        run(p, "too-big", &pin, &pin),
        "exceeds the 16 MiB input limit",
    );
    assert!(!p.join(".runtime").exists());
    assert_eq!(fs::read(p.join("input")).unwrap(), b"test");
}

#[test]
fn pinned_non_export_cannot_reach_checker_or_emit_partial_summary() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path();
    let input = b"{\"not_an_export\":true}\n";
    fs::write(p.join("input"), input).unwrap();
    let pin = format!("{:x}", Sha256::digest(input));
    let wrong = "f".repeat(64);
    let before = fs::read(p.join("input")).unwrap();
    let out = run(p, "input", &wrong, &wrong);
    assert!(!out.status.success() && out.stdout.is_empty());
    assert!(String::from_utf8_lossy(&out.stderr).contains("SHA-256"));
    let out = run(p, "input", &pin, &wrong);
    assert!(!out.status.success() && out.stdout.is_empty());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("metadata") || err.contains("header"), "{err}");
    assert!(!err.contains("resolve native compiler"), "{err}");
    assert_eq!(fs::read(p.join("input")).unwrap(), before);
    assert!(!p.join(".runtime").exists());
}
