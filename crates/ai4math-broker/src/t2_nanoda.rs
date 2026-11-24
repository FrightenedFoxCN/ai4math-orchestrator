//! Advisory check of an exact Lean NDJSON export by the native Nanoda checker.
use super::{AXIOMS, TIMEOUT, Toolchain, digest, sandbox};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{self, Read},
    path::Path,
    time::Duration,
};

/// Maximum input size, exclusive of the broker-generated configuration.
pub const LEAN_EXPORT_MAX_BYTES: usize = 16 * 1024 * 1024;
const MAX_LINE_BYTES: usize = 1024 * 1024;
const MAX_CHECKER_BYTES: u64 = 64 * 1024 * 1024;
const EXPORT_FORMAT: &str = "3.1.0";
const EXPORT_FILE: &str = "export.ndjson";
const CONFIG_FILE: &str = "config.json";

/// A successful advisory observation, not a proof receipt, project check, or
/// attestation of what source produced the export. The Lean commit is the value
/// declared in the export's header, not a separately verified provenance claim.
#[derive(Debug, Serialize)]
#[must_use]
pub struct LeanExportCheck {
    pub schema: &'static str,
    pub advisory_only: bool,
    pub checker: &'static str,
    pub accepted: bool,
    pub export_sha256: String,
    pub checker_sha256: String,
    pub export_format: &'static str,
    pub lean_git_commit: String,
    pub theorem_count: usize,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExportHeader {
    meta: ExportMeta,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExportMeta {
    exporter: ExporterMeta,
    format: FormatMeta,
    lean: LeanMeta,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExporterMeta {
    name: String,
    version: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FormatMeta {
    version: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LeanMeta {
    githash: String,
    version: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TheoremRecord {
    #[serde(rename = "thm")]
    _theorem: Theorem,
}

// Validate the official theorem shape, including required indexes, not merely
// the presence of a string or JSON key. Nanoda validates all reference values.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Theorem {
    #[serde(rename = "name")]
    _name: u32,
    #[serde(rename = "type")]
    _type: u32,
    #[serde(rename = "value")]
    _value: u32,
    #[serde(rename = "levelParams")]
    _level_params: Vec<u32>,
    #[serde(rename = "all")]
    _all: Vec<u32>,
}

fn lower_hex(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn check_line_size(line: &[u8], line_number: usize) -> Result<()> {
    ensure!(
        line.len() <= MAX_LINE_BYTES,
        "Lean export line {line_number} exceeds the 1 MiB limit"
    );
    Ok(())
}

fn inspect_export(bytes: &[u8], expected_sha256: &str) -> Result<(String, usize)> {
    ensure!(
        bytes.len() <= LEAN_EXPORT_MAX_BYTES,
        "Lean export exceeds the 16 MiB limit"
    );
    ensure!(
        lower_hex(expected_sha256, 64),
        "expected Lean export SHA-256 must be 64 lowercase hex digits"
    );
    ensure!(
        digest(bytes) == expected_sha256,
        "Lean export SHA-256 mismatch"
    );

    // Allow a final delimiter or EOF after the final object, but not blank records.
    let mut lines = bytes
        .strip_suffix(b"\n")
        .unwrap_or(bytes)
        .split(|b| *b == b'\n');
    let header = lines.next().context("missing Lean export header")?;
    check_line_size(header, 1)?;
    let shape: Value =
        serde_json::from_slice(header).context("invalid Lean export metadata on line 1")?;
    ensure!(
        shape
            .get("meta")
            .and_then(Value::as_object)
            .is_some_and(|meta| ["exporter", "format", "lean"]
                .iter()
                .all(|key| meta.get(*key).is_some_and(Value::is_object))),
        "invalid Lean export metadata objects on line 1"
    );
    let ExportHeader { meta } =
        serde_json::from_slice(header).context("invalid Lean export metadata on line 1")?;
    ensure!(
        meta.format.version == EXPORT_FORMAT,
        "unsupported Lean export format; expected {EXPORT_FORMAT}"
    );
    ensure!(
        meta.exporter.name == "lean4export" && !meta.exporter.version.is_empty(),
        "Lean export metadata must identify lean4export and its version"
    );
    ensure!(
        lower_hex(&meta.lean.githash, 40) && meta.lean.githash.bytes().any(|b| b != b'0'),
        "Lean git hash must be 40 lowercase hex digits and nonzero"
    );
    ensure!(
        !meta.lean.version.is_empty(),
        "missing Lean version in export metadata"
    );

    let mut theorem_count = 0;
    for (index, line) in lines.enumerate() {
        let line_number = index + 2;
        check_line_size(line, line_number)?;
        let object: Map<String, Value> = serde_json::from_slice(line)
            .with_context(|| format!("invalid Lean export NDJSON object on line {line_number}"))?;
        ensure!(
            !object.contains_key("meta"),
            "unexpected Lean export metadata on line {line_number}"
        );
        if let Some(theorem) = object.get("thm") {
            ensure!(
                theorem.is_object(),
                "invalid Lean theorem record on line {line_number}"
            );
            // Reparse the original line as a strict typed record to reject mixed
            // tags, duplicate fields and duplicate theorem keys before counting.
            serde_json::from_slice::<TheoremRecord>(line)
                .with_context(|| format!("invalid Lean theorem record on line {line_number}"))?;
            theorem_count += 1;
        }
    }
    ensure!(theorem_count > 0, "Lean export contains no theorem records");
    Ok((meta.lean.githash, theorem_count))
}

fn bounded_file_digest(path: &Path, max_bytes: u64) -> Result<String> {
    let file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let len = io::copy(&mut file.take(max_bytes + 1), &mut hasher)?;
    ensure!(len <= max_bytes, "hash input exceeds {max_bytes} bytes");
    Ok(format!("{:x}", hasher.finalize()))
}

fn verify_checker_pin(path: &Path, expected_sha256: &str) -> Result<()> {
    ensure!(
        path.is_absolute(),
        "Nanoda executable path must be absolute"
    );
    ensure!(
        path.file_name().is_some_and(|name| name == "nanoda_bin"),
        "Nanoda executable basename must be nanoda_bin"
    );
    let metadata = fs::symlink_metadata(path).context("inspect native Nanoda executable")?;
    ensure!(
        metadata.is_file(),
        "Nanoda executable must be a regular file, not a symlink"
    );
    ensure!(
        metadata.len() <= MAX_CHECKER_BYTES,
        "Nanoda executable exceeds the 64 MiB size limit"
    );
    let canonical = Toolchain::native_executable(path).context("check native Nanoda executable")?;
    ensure!(
        canonical.as_os_str() == path.as_os_str(),
        "Nanoda executable path must be canonical"
    );
    ensure!(
        bounded_file_digest(path, MAX_CHECKER_BYTES).context("hash native Nanoda executable")?
            == expected_sha256,
        "Nanoda checker SHA-256 mismatch"
    );
    Ok(())
}

/// Run an operator-pinned native Nanoda in the broker sandbox against an exact
/// pinned export. The operator must vet the executable and trust its installation;
/// a digest supplied by an untrusted client is not an executable trust policy.
/// No caller-supplied checker options, source files, or project paths are exposed.
pub fn check_lean_export(
    bytes: &[u8],
    expected_export_sha256: &str,
    native_nanoda: &Path,
    expected_checker_sha256: &str,
) -> Result<LeanExportCheck> {
    check_lean_export_timed(
        bytes,
        expected_export_sha256,
        native_nanoda,
        expected_checker_sha256,
        || Ok(TIMEOUT),
    )
}

/// Query the remaining managed time at invocation, after validation, staging,
/// and the pin check, rather than giving Nanoda a stale timeout.
pub(super) fn check_lean_export_timed(
    bytes: &[u8],
    expected_export_sha256: &str,
    native_nanoda: &Path,
    expected_checker_sha256: &str,
    timeout: impl FnOnce() -> Result<Duration>,
) -> Result<LeanExportCheck> {
    ensure!(
        lower_hex(expected_checker_sha256, 64),
        "expected Nanoda checker SHA-256 must be 64 lowercase hex digits"
    );
    let (lean_git_commit, theorem_count) = inspect_export(bytes, expected_export_sha256)?;
    let temp = tempfile::tempdir().context("create Nanoda check room")?;
    let room = temp.path().canonicalize()?;
    let output = room.join("output");
    fs::create_dir(&output).context("create Nanoda scratch directory")?;
    let export = room.join(EXPORT_FILE);
    // The room is private to the broker, and only output is writable to Nanoda.
    fs::write(&export, bytes).context("write broker-staged Lean export")?;
    let config = json!({
        "use_stdin": false,
        "export_file_path": EXPORT_FILE,
        "permitted_axioms": AXIOMS,
        "unpermitted_axiom_hard_error": true,
        "unsafe_permit_all_axioms": false,
        "num_threads": 1,
        "nat_extension": true,
        "string_extension": true,
        "pp_declars": [],
        "pp_to_stdout": false,
        "print_success_message": false,
        "print_axioms": false,
    });
    fs::write(room.join(CONFIG_FILE), serde_json::to_vec(&config)?)
        .context("write broker-owned Nanoda configuration")?;
    verify_checker_pin(native_nanoda, expected_checker_sha256)?;
    let result = sandbox::run(sandbox::Invocation {
        executable: native_nanoda,
        args: &[CONFIG_FILE.to_owned()],
        room: &room,
        writable: &output,
        reads: &[],
        env: &[],
        timeout: TIMEOUT.min(timeout()?),
    });
    verify_checker_pin(native_nanoda, expected_checker_sha256)
        .context("recheck Nanoda checker after sandbox invocation")?;
    ensure!(
        bounded_file_digest(&export, LEAN_EXPORT_MAX_BYTES as u64)
            .context("rehash broker-staged Lean export")?
            == expected_export_sha256,
        "broker-staged Lean export SHA-256 mismatch after sandbox invocation"
    );
    result.context("Nanoda rejected the Lean export or could not complete the sandboxed check")?;
    temp.close().context("discard Nanoda check room")?;
    Ok(LeanExportCheck {
        schema: "ai4math.t2.lean-export-check.v1",
        advisory_only: true,
        checker: "nanoda",
        accepted: true,
        export_sha256: expected_export_sha256.to_owned(),
        checker_sha256: expected_checker_sha256.to_owned(),
        export_format: EXPORT_FORMAT,
        lean_git_commit,
        theorem_count,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // These are emitted by native Lean 4.35's leanexport for a small identity
    // theorem. Unit tests exercise preflight, not resolution of record indexes.
    const COMMIT: &str = "eb6cafb7290e8bd5f2937522d8d804f58f02bafc";
    const HEADER: &str = r#"{"meta":{"exporter":{"name":"lean4export","version":"3.1.0"},"format":{"version":"3.1.0"},"lean":{"githash":"eb6cafb7290e8bd5f2937522d8d804f58f02bafc","version":"4.35.0"}}}"#;
    const THEOREM: &str = r#"{"thm":{"all":[1],"levelParams":[],"name":1,"type":4,"value":6}}"#;
    const OTHER: &str = r#"{"ie":0,"sort":0}"#;

    fn inspect(bytes: &[u8]) -> Result<(String, usize)> {
        inspect_export(bytes, &digest(bytes))
    }

    #[test]
    fn recognizes_official_metadata_and_counts_theorem_records() -> Result<()> {
        for delimiter in ["\n", "\r\n"] {
            for suffix in ["", delimiter] {
                let export = format!(
                    "{HEADER}{delimiter}{OTHER}{delimiter}{THEOREM}{delimiter}{THEOREM}{suffix}"
                );
                assert_eq!(inspect(export.as_bytes())?, (COMMIT.to_owned(), 2));
            }
        }
        Ok(())
    }

    #[test]
    fn rejects_wrong_metadata_or_no_actual_theorem() {
        for header in [
            HEADER.replacen("3.1.0", "3.0.0", 2),
            HEADER.replace("lean4export", "other"),
            HEADER.replace(COMMIT, &COMMIT.to_uppercase()),
            HEADER.replace(COMMIT, &"0".repeat(40)),
            HEADER.replace(COMMIT, "abc"),
            "{}".to_owned(),
            format!(r#"[{}]"#, &HEADER[8..HEADER.len() - 1]),
            HEADER.replace(r#"{"version":"3.1.0"}"#, r#"["3.1.0"]"#),
        ] {
            assert!(inspect(format!("{header}\n{THEOREM}").as_bytes()).is_err());
        }
        for body in [
            "",
            OTHER,
            HEADER,
            r#"{"thm":null}"#,
            r#"{"thm":{}}"#,
            r#"{"thm":[1,4,6,[],[1]]}"#,
            r#"{"thm":{"all":[1],"levelParams":[],"name":1,"type":4,"value":"6"}}"#,
            r#"{"nested":{"thm":true}}"#,
            r#"{"thm":{"all":[1],"levelParams":[],"name":1,"type":4,"value":6},"ie":0}"#,
            r#"{"thm":{"all":[1],"levelParams":[],"name":1,"type":4,"value":6},"thm":null}"#,
        ] {
            assert!(inspect(format!("{HEADER}\n{body}").as_bytes()).is_err());
        }
        for bad_line in [&b"[]"[..], b"\xff", b""] {
            assert!(
                inspect(
                    &[
                        HEADER.as_bytes(),
                        b"\n",
                        bad_line,
                        b"\n",
                        THEOREM.as_bytes()
                    ]
                    .concat()
                )
                .is_err()
            );
        }
    }

    #[test]
    fn caps_header_and_record_lines_and_total_input() -> Result<()> {
        let padded = |line: &str, len: usize| [line, &" ".repeat(len - line.len())].concat();
        let export = format!(
            "{}\n{}\n{THEOREM}",
            padded(HEADER, MAX_LINE_BYTES),
            padded(OTHER, MAX_LINE_BYTES),
        );
        assert_eq!(inspect(export.as_bytes())?.1, 1);
        for (line, export) in [
            (
                1,
                format!("{}\n{THEOREM}", padded(HEADER, MAX_LINE_BYTES + 1)),
            ),
            (
                2,
                format!("{HEADER}\n{}\n{THEOREM}", padded(OTHER, MAX_LINE_BYTES + 1)),
            ),
        ] {
            let err = inspect(export.as_bytes()).unwrap_err();
            assert!(
                err.to_string()
                    .contains(&format!("line {line} exceeds the 1 MiB limit"))
            );
        }
        let block = padded(OTHER, MAX_LINE_BYTES);
        let mut bytes = format!("{HEADER}\n{THEOREM}\n").into_bytes();
        while LEAN_EXPORT_MAX_BYTES - bytes.len() > MAX_LINE_BYTES {
            bytes.extend_from_slice(block.as_bytes());
            bytes.push(b'\n');
        }
        bytes.extend_from_slice(OTHER.as_bytes());
        bytes.resize(LEAN_EXPORT_MAX_BYTES, b' ');
        assert_eq!(inspect(&bytes)?.1, 1);
        bytes.push(b' ');
        let err = inspect_export(&bytes, &"0".repeat(64)).unwrap_err();
        assert!(err.to_string().contains("exceeds the 16 MiB limit"));
        Ok(())
    }

    #[test]
    fn requires_exact_lowercase_sha256_pins() {
        let export = format!("{HEADER}\n{THEOREM}");
        let bytes = export.as_bytes();
        let export_pin = digest(bytes);
        for pin in [
            export_pin.to_uppercase(),
            format!(" {export_pin}"),
            "0".repeat(63),
        ] {
            assert!(
                inspect_export(bytes, &pin)
                    .unwrap_err()
                    .to_string()
                    .contains("64 lowercase hex digits")
            );
            assert!(
                check_lean_export(bytes, &export_pin, Path::new("unused"), &pin)
                    .unwrap_err()
                    .to_string()
                    .contains("64 lowercase hex digits")
            );
        }
        assert!(
            inspect_export(bytes, &"0".repeat(64))
                .unwrap_err()
                .to_string()
                .contains("SHA-256 mismatch")
        );
    }

    #[test]
    fn rejects_noncanonical_paths_and_shims_without_launch() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let canonical = temp.path().canonicalize()?;
        let binary = canonical.join("nanoda_bin");
        let shim = b"#!/bin/sh\nexit 0\n";
        fs::write(&binary, shim)?;
        let pin = digest(shim);
        assert!(
            verify_checker_pin(Path::new("nanoda_bin"), &pin)
                .unwrap_err()
                .to_string()
                .contains("absolute")
        );
        assert!(
            verify_checker_pin(&canonical.join("nanoda"), &pin)
                .unwrap_err()
                .to_string()
                .contains("basename")
        );
        assert!(format!("{:#}", verify_checker_pin(&binary, &pin).unwrap_err()).contains("Mach-O"));
        // Fake magic for preflight only; never pass this file to Seatbelt.
        let fake = b"\xcf\xfa\xed\xfe";
        fs::write(&binary, fake)?;
        assert!(verify_checker_pin(&binary, &digest(fake)).is_ok());
        assert!(
            verify_checker_pin(&binary, &pin)
                .unwrap_err()
                .to_string()
                .contains("SHA-256 mismatch")
        );
        assert!(
            verify_checker_pin(&canonical.join("./nanoda_bin"), &digest(fake))
                .unwrap_err()
                .to_string()
                .contains("canonical")
        );
        Ok(())
    }
}
