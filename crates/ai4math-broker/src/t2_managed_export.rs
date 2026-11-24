//! Advisory export of one exact theorem from a fresh isolated source build.
use super::{
    Broker, Engine, LEAN_EXPORT_MAX_BYTES, LeanBuildAudit, LeanExportCheck, TIMEOUT, Toolchain,
    breaker_probe::identifier, check_lean_export, digest, path_for, sandbox,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    borrow::Cow,
    fs,
    io::{self, Read},
    path::Path,
};

const DECLARATION_MAX_BYTES: usize = 256;
const NATIVE_BINARY_MAX_BYTES: u64 = 64 * 1024 * 1024;
const EXPORT_LINE_MAX_BYTES: usize = 1024 * 1024;
const ORIGIN_FILE: &str = "Ai4mathManagedLeanOrigin.lean";
const ORIGIN_SEAL: &str = "ai4math:managed-lean-origin:v1\n";
// Compiled from a clean room before the objects are added to Lean's search path.
// The declaration and module are argv data, never interpolated Lean syntax.
const ORIGIN_SOURCE: &str = r#"import Lean

open Lean

def main (args : List String) : IO Unit := do
  let [objectsText, moduleText, declarationText] := args
    | throw <| IO.userError "invalid managed origin arguments"
  let objects := System.FilePath.mk objectsText
  let mod := Name.mkSimple moduleText
  let name := (declarationText.splitOn ".").foldl Name.str .anonymous
  let invalid := IO.userError s!"'{name}' is not a public theorem of '{mod}'"
  let builtinPath ← Lean.searchPathRef.get
  Lean.searchPathRef.set (builtinPath ++ [objects])
  let env ← Lean.importModules #[{ module := mod }] {} (loadExts := false)
  let some idx := env.getModuleIdxFor? name | throw invalid
  unless !isPrivateName name && env.header.moduleNames[idx.toNat]? == some mod do
    throw invalid
  let some data := env.header.moduleData[idx.toNat]? | throw invalid
  let some info@(.thmInfo _) := data.constants.find? (·.name == name) | throw invalid
  unless env.find? name == some info do
    throw invalid
  let (publicData, _) ← Lean.readModuleData (objects / (moduleText ++ ".olean"))
  let some publicInfo := publicData.constants.find? (·.name == name) | throw invalid
  unless publicInfo.toConstantVal == info.toConstantVal do
    throw invalid
  IO.println "ai4math:managed-lean-origin:v1"
"#;

/// An advisory observation of an isolated build and a pinned independent check
/// of one theorem from it. Not a promotion receipt or project attestation.
#[derive(Debug, Serialize)]
#[must_use]
pub struct ManagedLeanTheoremAudit {
    pub build: LeanBuildAudit,
    pub independent_check: LeanExportCheck,
    pub exporter_sha256: String,
}

fn validate_declaration(declaration: &str) -> Result<()> {
    ensure!(
        declaration.len() <= DECLARATION_MAX_BYTES
            && declaration.contains('.')
            && !declaration.starts_with("_root_.")
            && declaration.split('.').all(identifier),
        "expected an exact dotted ASCII Lean theorem name of at most {DECLARATION_MAX_BYTES} bytes"
    );
    Ok(())
}

fn validate_pin(pin: &str, label: &str) -> Result<()> {
    ensure!(
        pin.len() == 64
            && pin
                .bytes()
                .all(|b| b.is_ascii_digit() || matches!(b, b'a'..=b'f')),
        "expected {label} SHA-256 must be 64 lowercase hex digits"
    );
    Ok(())
}

fn verify_pin(path: &Path, pin: &str, label: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(path).with_context(|| format!("inspect native {label}"))?;
    ensure!(
        metadata.is_file(),
        "native {label} must be a regular file, not a symlink"
    );
    ensure!(
        metadata.len() <= NATIVE_BINARY_MAX_BYTES,
        "native {label} exceeds the 64 MiB size limit"
    );
    let canonical =
        Toolchain::native_executable(path).with_context(|| format!("check native {label}"))?;
    ensure!(
        canonical.as_os_str() == path.as_os_str(),
        "native {label} path must be canonical"
    );
    let mut hasher = Sha256::new();
    let len = io::copy(
        &mut fs::File::open(path)?.take(NATIVE_BINARY_MAX_BYTES + 1),
        &mut hasher,
    )?;
    ensure!(
        len <= NATIVE_BINARY_MAX_BYTES,
        "native {label} exceeds the 64 MiB size limit"
    );
    ensure!(
        format!("{:x}", hasher.finalize()) == pin,
        "{label} SHA-256 mismatch"
    );
    Ok(())
}

fn confirm_origin(tool: &Toolchain, objects: &Path, module: &str, declaration: &str) -> Result<()> {
    let temp = tempfile::tempdir().context("create managed Lean origin room")?;
    let room = temp.path().canonicalize()?;
    let writable = room.join("output");
    fs::create_dir(&writable)?;
    fs::write(room.join(ORIGIN_FILE), ORIGIN_SOURCE)?;
    let args = [
        "--run".to_owned(),
        ORIGIN_FILE.to_owned(),
        objects
            .to_str()
            .context("non-UTF8 Lean object path")?
            .to_owned(),
        module.to_owned(),
        declaration.to_owned(),
    ];
    let log = sandbox::run_grouped(
        sandbox::Invocation {
            executable: &tool.executable,
            args: &args,
            room: &room,
            writable: &writable,
            reads: &tool.reads,
            env: &[],
            timeout: TIMEOUT,
        },
        &[objects.to_owned()],
        &[],
        tool.sandbox_group,
    )?;
    ensure!(log == ORIGIN_SEAL, "missing exact managed Lean origin seal");
    temp.close().context("discard managed Lean origin room")?;
    Ok(())
}

// Only interest in name-table and theorem records here. The existing independent
// checker separately validates the strict 3.1.0 metadata/theorem schema and checks
// expression/declaration references and the axioms in its native process.
#[derive(Deserialize)]
struct ExportRecord<'a> {
    #[serde(rename = "in")]
    name_index: Option<u32>,
    #[serde(rename = "str", borrow)]
    string: Option<ExportNamePart<'a>>,
    thm: Option<ExportTheoremName>,
}

#[derive(Deserialize)]
struct ExportNamePart<'a> {
    pre: u32,
    #[serde(rename = "str", borrow)]
    string: Cow<'a, str>,
}

#[derive(Deserialize)]
struct ExportTheoremName {
    name: u32,
}

fn require_exported_theorem(bytes: &[u8], declaration: &str) -> Result<()> {
    ensure!(
        bytes.len() <= LEAN_EXPORT_MAX_BYTES,
        "Lean export exceeds the 16 MiB limit"
    );
    let parts: Vec<_> = declaration.split('.').collect();
    // Index zero is anonymous. Track just the length of a matching prefix per
    // name-table index; do not flatten Lean Name components or rebuild trees.
    let mut prefixes = vec![Some(0)];
    let mut found = false;
    let lines = bytes
        .strip_suffix(b"\n")
        .unwrap_or(bytes)
        .split(|&b| b == b'\n');
    for (index, line) in lines.enumerate().skip(1) {
        let line_number = index + 1;
        ensure!(
            line.len() <= EXPORT_LINE_MAX_BYTES,
            "Lean export line {line_number} exceeds the 1 MiB limit"
        );
        let row: ExportRecord<'_> = serde_json::from_slice(line)
            .with_context(|| format!("read managed Lean export record on line {line_number}"))?;
        if let Some(id) = row.name_index {
            ensure!(
                id as usize == prefixes.len(),
                "unexpected Lean export name index on line {line_number}"
            );
            let matched = if let Some(part) = row.string {
                let prefix = *prefixes
                    .get(part.pre as usize)
                    .context("missing prefix of exported Lean name")?;
                prefix
                    .filter(|&n| parts.get(n).is_some_and(|&s| s == part.string))
                    .map(|n| n + 1)
            } else {
                // Numeric name components cannot match a dotted ASCII identifier.
                None
            };
            prefixes.push(matched);
        }
        if let Some(theorem) = row.thm
            && prefixes.get(theorem.name as usize) == Some(&Some(parts.len()))
        {
            ensure!(!found, "duplicate selected theorem in managed Lean export");
            found = true;
        }
    }
    ensure!(
        found,
        "managed Lean export does not contain selected theorem {declaration:?}"
    );
    Ok(())
}

impl Broker {
    /// Build ordered sources, confirm the exact public declaration belongs to
    /// `target_module` (a source path such as `Fixture.lean`), export it, and
    /// run the existing independent checker. Pins must come from an operator
    /// who trusts the native installation, not from the submitted source set.
    pub fn audit_lean_theorem(
        &self,
        tool: &Toolchain,
        modules: &[(&str, &str)],
        target_module: &str,
        declaration: &str,
        exporter_sha256: &str,
        checker_sha256: &str,
    ) -> Result<ManagedLeanTheoremAudit> {
        ensure!(
            tool.engine == Engine::Lean4,
            "managed theorem audit requires native Lean"
        );
        validate_declaration(declaration)?;
        let module = path_for(Engine::Lean4, target_module)?;
        ensure!(
            modules.iter().any(|(path, _)| *path == target_module),
            "managed theorem target module must be in the supplied source set"
        );
        validate_pin(exporter_sha256, "Lean exporter")?;
        validate_pin(checker_sha256, "Nanoda checker")?;
        let bin = tool
            .executable
            .parent()
            .filter(|path| path.file_name().is_some_and(|name| name == "bin"))
            .context("managed theorem audit requires the native Lean toolchain bin directory")?;
        let sysroot = bin.parent().context("find native Lean sysroot")?;
        let exporter = bin.join("leanexport");
        let checker = bin.join("nanoda_bin");
        verify_pin(&tool.executable, &tool.binary_sha256, "Lean compiler")?;
        verify_pin(&exporter, exporter_sha256, "Lean exporter")?;
        verify_pin(&checker, checker_sha256, "Nanoda checker")?;
        let (build, independent_check) = self.audit_lean_build_with(
            tool,
            modules,
            Some((module, |objects: &Path| {
                confirm_origin(tool, objects, module, declaration)
                    .context("Lean declaration is not a public theorem owned by target module")?;
                let temp = tempfile::tempdir().context("create managed Lean export room")?;
                let room = temp.path().canonicalize()?;
                let writable = room.join("output");
                fs::create_dir(&writable)?;
                verify_pin(&exporter, exporter_sha256, "Lean exporter")?;
                let result = sandbox::run_capture_bytes(
                    sandbox::Invocation {
                        executable: &exporter,
                        args: &[module.to_owned(), "--".to_owned(), declaration.to_owned()],
                        room: &room,
                        writable: &writable,
                        reads: &tool.reads,
                        env: &[("LEAN_SYSROOT", sysroot), ("LEAN_PATH", objects)],
                        timeout: TIMEOUT,
                    },
                    &[objects.to_owned()],
                    &[],
                    tool.sandbox_group,
                    LEAN_EXPORT_MAX_BYTES,
                );
                verify_pin(&exporter, exporter_sha256, "Lean exporter")
                    .context("recheck Lean exporter after sandbox invocation")?;
                let bytes = result.context("export managed Lean theorem")?;
                temp.close().context("discard managed Lean export room")?;
                require_exported_theorem(&bytes, declaration)?;
                check_lean_export(&bytes, &digest(&bytes), &checker, checker_sha256)
            })),
        )?;
        verify_pin(&tool.executable, &tool.binary_sha256, "Lean compiler")
            .context("recheck Lean compiler after managed theorem audit")?;
        Ok(ManagedLeanTheoremAudit {
            build,
            independent_check: independent_check.context("missing managed Lean theorem check")?,
            exporter_sha256: exporter_sha256.to_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_bounded_dotted_ascii_names() {
        assert!(validate_declaration("A_B.x2'").is_ok());
        assert!(validate_declaration(&format!("A.{}", "a".repeat(254))).is_ok());
        for bad in [
            "",
            "x",
            "_root_.x",
            "A._",
            "A..x",
            "A.x.",
            ".A.x",
            "--all.x",
            "A.-x",
            "A.2",
            "A.λ",
            "A.x y",
            "A.x\n",
            "A.\"x",
            "A.x/*",
            &format!("A.{}", "a".repeat(255)),
        ] {
            assert!(validate_declaration(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn resolves_exact_structured_names_to_exported_theorems() -> Result<()> {
        const RECORDS: &str = r#"{}
{"in":1,"str":{"pre":0,"str":"Wanted"}}
{"in":2,"str":{"pre":1,"str":"example"}}
{"in":3,"str":{"pre":0,"str":"Wanted.example"}}
{"in":4,"num":{"pre":1,"num":1}}
{"in":5,"str":{"pre":4,"str":"example"}}
{"ie":0,"sort":0}"#;
        let theorem = |n| {
            format!(r#"{{"thm":{{"name":{n},"type":4,"value":6,"levelParams":[],"all":[{n}]}}}}"#)
        };
        let root = format!("{RECORDS}\n{}", theorem(2));
        require_exported_theorem(root.as_bytes(), "Wanted.example")?;
        for n in [1, 3, 5] {
            assert!(
                require_exported_theorem(
                    format!("{RECORDS}\n{}", theorem(n)).as_bytes(),
                    "Wanted.example"
                )
                .is_err()
            );
        }
        for trailing in [
            String::new(),
            format!("\n{}\n{}", theorem(2), theorem(2)),
            format!(
                r#"
{{"in":2,"str":{{"pre":0,"str":"wrong"}}}}
{}"#,
                theorem(2)
            ),
            r#"
{"def":{"name":2}}"#
                .to_owned(),
        ] {
            assert!(
                require_exported_theorem(
                    format!("{RECORDS}{trailing}").as_bytes(),
                    "Wanted.example"
                )
                .is_err()
            );
        }
        Ok(())
    }
}
