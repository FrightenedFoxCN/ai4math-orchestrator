//! Advisory export of exact theorems from a fresh isolated source build.
use super::{
    Broker, Engine, LEAN_EXPORT_MAX_BYTES, LeanBuildAudit, LeanExportCheck, TIMEOUT, Toolchain,
    breaker_probe::identifier, digest, path_for, sandbox, t2_nanoda::check_lean_export_timed,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    borrow::Cow,
    collections::HashSet,
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

/// Maximum number of requested declarations per managed theorem audit.
pub const MAX_MANAGED_T2_THEOREMS: usize = 8;
const INDEPENDENT_TIMEOUT: Duration = Duration::from_secs(120);
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

/// Advisory observations from one isolated build, not promotion receipts.
#[derive(Debug, Serialize)]
#[must_use]
pub struct ManagedLeanBatchAudit {
    pub build: LeanBuildAudit,
    pub exporter_sha256: String,
    /// Checks in the caller's target order, not build order.
    pub checks: Vec<ManagedLeanBatchCheck>,
}

/// Independent check of the exact public theorem in its owning module.
#[derive(Debug, Serialize)]
pub struct ManagedLeanBatchCheck {
    /// Source path, such as `Fixture.lean`.
    pub target_module: String,
    pub declaration: String,
    pub independent_check: LeanExportCheck,
}

#[derive(Clone, Copy)]
struct ManagedTarget<'a> {
    path: &'a str,
    module: &'a str,
    declaration: &'a str,
}

fn validate_targets<'a>(
    modules: &[(&str, &str)],
    targets: &[(&'a str, &'a str)],
) -> Result<Vec<ManagedTarget<'a>>> {
    ensure!(
        (1..=MAX_MANAGED_T2_THEOREMS).contains(&targets.len()),
        "expected 1 to {MAX_MANAGED_T2_THEOREMS} managed Lean theorem targets"
    );
    let mut declarations = HashSet::with_capacity(targets.len());
    targets
        .iter()
        .map(|&(path, declaration)| {
            validate_declaration(declaration)?;
            let module = path_for(Engine::Lean4, path)?;
            ensure!(
                modules.iter().any(|&(source_path, _)| source_path == path),
                "managed theorem target module must be in the supplied source set"
            );
            ensure!(
                declarations.insert(declaration),
                "duplicate managed Lean theorem declaration {declaration:?}"
            );
            Ok(ManagedTarget {
                path,
                module,
                declaration,
            })
        })
        .collect()
}

/// Validate managed Lean target paths, exact names, count and uniqueness without compiling.
pub fn validate_managed_lean_targets(
    modules: &[(&str, &str)],
    targets: &[(&str, &str)],
) -> Result<()> {
    validate_targets(modules, targets).map(|_| ())
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

fn phase_timeout(deadline: Instant) -> Result<Duration> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    ensure!(
        !remaining.is_zero(),
        "managed Lean independent check wall-clock budget exhausted"
    );
    Ok(TIMEOUT.min(remaining))
}

fn within_check_budget<T>(
    remaining: &mut Duration,
    check: impl FnOnce(Instant) -> Result<T>,
) -> Result<T> {
    let start = Instant::now();
    let deadline = start + *remaining;
    phase_timeout(deadline)?;
    let checked = check(deadline);
    *remaining = remaining.saturating_sub(start.elapsed());
    let checked = checked?;
    phase_timeout(deadline)?;
    Ok(checked)
}

fn confirm_origin(
    tool: &Toolchain,
    objects: &Path,
    module: &str,
    declaration: &str,
    deadline: Instant,
) -> Result<()> {
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
            timeout: phase_timeout(deadline)?,
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

struct ManagedNative<'a> {
    tool: &'a Toolchain,
    sysroot: &'a Path,
    exporter: PathBuf,
    exporter_pin: &'a str,
    checker: PathBuf,
    checker_pin: &'a str,
}

impl<'a> ManagedNative<'a> {
    fn new(tool: &'a Toolchain, exporter_pin: &'a str, checker_pin: &'a str) -> Result<Self> {
        validate_pin(exporter_pin, "Lean exporter")?;
        validate_pin(checker_pin, "Nanoda checker")?;
        let bin = tool
            .executable
            .parent()
            .filter(|path| path.file_name().is_some_and(|name| name == "bin"))
            .context("managed theorem audit requires the native Lean toolchain bin directory")?;
        let sysroot = bin.parent().context("find native Lean sysroot")?;
        let exporter = bin.join("leanexport");
        let checker = bin.join("nanoda_bin");
        verify_pin(&tool.executable, &tool.binary_sha256, "Lean compiler")?;
        verify_pin(&exporter, exporter_pin, "Lean exporter")?;
        verify_pin(&checker, checker_pin, "Nanoda checker")?;
        Ok(Self {
            tool,
            sysroot,
            exporter,
            exporter_pin,
            checker,
            checker_pin,
        })
    }

    fn check(
        &self,
        objects: &Path,
        target: ManagedTarget<'_>,
        deadline: Instant,
    ) -> Result<LeanExportCheck> {
        let ManagedTarget {
            module,
            declaration,
            ..
        } = target;
        confirm_origin(self.tool, objects, module, declaration, deadline)
            .context("Lean declaration is not a public theorem owned by target module")?;
        let temp = tempfile::tempdir().context("create managed Lean export room")?;
        let room = temp.path().canonicalize()?;
        let writable = room.join("output");
        fs::create_dir(&writable)?;
        verify_pin(&self.exporter, self.exporter_pin, "Lean exporter")?;
        let result = sandbox::run_capture_bytes(
            sandbox::Invocation {
                executable: &self.exporter,
                args: &[module.to_owned(), "--".to_owned(), declaration.to_owned()],
                room: &room,
                writable: &writable,
                reads: &self.tool.reads,
                env: &[("LEAN_SYSROOT", self.sysroot), ("LEAN_PATH", objects)],
                timeout: phase_timeout(deadline)?,
            },
            &[objects.to_owned()],
            &[],
            self.tool.sandbox_group,
            LEAN_EXPORT_MAX_BYTES,
        );
        verify_pin(&self.exporter, self.exporter_pin, "Lean exporter")
            .context("recheck Lean exporter after sandbox invocation")?;
        let bytes = result.context("export managed Lean theorem")?;
        temp.close().context("discard managed Lean export room")?;
        require_exported_theorem(&bytes, declaration)?;
        check_lean_export_timed(
            &bytes,
            &digest(&bytes),
            &self.checker,
            self.checker_pin,
            self.tool.sandbox_group,
            || phase_timeout(deadline),
        )
    }
}

fn observe_module<'a, T>(
    checks: &mut [(ManagedTarget<'a>, Option<T>)],
    module: &str,
    mut observe: impl FnMut(ManagedTarget<'a>) -> Result<T>,
) -> Result<()> {
    // Filter without sorting: observations of the same module run in input
    // order and all results occupy their original target slots.
    for (target, check) in checks
        .iter_mut()
        .filter(|(target, _)| target.module == module)
    {
        *check = Some(observe(*target)?);
    }
    Ok(())
}

enum OnCheckFailure {
    Stop,
    // The legacy single audit gave downstream build failures precedence over
    // independent-check errors, even though it checked at the target prefix.
    FinishBuild,
}

impl Broker {
    /// Build ordered sources, confirm the exact public declaration belongs to
    /// the source path, export it, and run the pinned independent checker.
    pub fn audit_lean_theorem(
        &self,
        tool: &Toolchain,
        modules: &[(&str, &str)],
        target_module: &str,
        declaration: &str,
        exporter_sha256: &str,
        checker_sha256: &str,
    ) -> Result<ManagedLeanTheoremAudit> {
        let audit = self.audit_managed_lean_theorems(
            tool,
            modules,
            &[(target_module, declaration)],
            exporter_sha256,
            checker_sha256,
            OnCheckFailure::FinishBuild,
        )?;
        let check = audit
            .checks
            .into_iter()
            .next()
            .context("missing managed Lean theorem check")?;
        Ok(ManagedLeanTheoremAudit {
            build: audit.build,
            independent_check: check.independent_check,
            exporter_sha256: audit.exporter_sha256,
        })
    }

    /// Build ordered sources once and check each exact public theorem when its
    /// owning module arrives. Return the checks in target order only after the
    /// entire audit succeeds. Pins must come from an operator who trusts the
    /// native installation, not from the submitted source set.
    pub fn audit_lean_theorems(
        &self,
        tool: &Toolchain,
        modules: &[(&str, &str)],
        targets: &[(&str, &str)],
        exporter_sha256: &str,
        checker_sha256: &str,
    ) -> Result<ManagedLeanBatchAudit> {
        self.audit_managed_lean_theorems(
            tool,
            modules,
            targets,
            exporter_sha256,
            checker_sha256,
            OnCheckFailure::Stop,
        )
    }

    fn audit_managed_lean_theorems(
        &self,
        tool: &Toolchain,
        modules: &[(&str, &str)],
        targets: &[(&str, &str)],
        exporter_sha256: &str,
        checker_sha256: &str,
        on_check_failure: OnCheckFailure,
    ) -> Result<ManagedLeanBatchAudit> {
        ensure!(
            tool.engine == Engine::Lean4,
            "managed theorem audit requires native Lean"
        );
        let mut checks: Vec<_> = validate_targets(modules, targets)?
            .into_iter()
            .map(|target| (target, None))
            .collect();
        let native = ManagedNative::new(tool, exporter_sha256, checker_sha256)?;
        let mut remaining = INDEPENDENT_TIMEOUT;
        let mut deferred_error = None;
        let build = self.audit_lean_build_with(
            tool,
            modules,
            Some(|module: &str, objects: &Path| {
                if deferred_error.is_some() {
                    return Ok(());
                }
                let result = observe_module(&mut checks, module, |target| {
                    let result = within_check_budget(&mut remaining, |deadline| {
                        native.check(objects, target, deadline)
                    });
                    match on_check_failure {
                        OnCheckFailure::Stop => result.with_context(|| {
                            format!(
                                "check managed Lean theorem {:?} in {:?}",
                                target.declaration, target.path
                            )
                        }),
                        OnCheckFailure::FinishBuild => result,
                    }
                });
                match on_check_failure {
                    OnCheckFailure::Stop => result,
                    OnCheckFailure::FinishBuild => {
                        deferred_error = result.err();
                        Ok(())
                    }
                }
            }),
        )?;
        if let Some(error) = deferred_error {
            return Err(error);
        }
        verify_pin(&tool.executable, &tool.binary_sha256, "Lean compiler")
            .context("recheck Lean compiler after managed theorem audit")?;
        let checks = checks
            .into_iter()
            .map(|(target, check)| {
                Ok(ManagedLeanBatchCheck {
                    target_module: target.path.to_owned(),
                    declaration: target.declaration.to_owned(),
                    independent_check: check.context("missing managed Lean theorem check")?,
                })
            })
            .collect::<Result<_>>()?;
        Ok(ManagedLeanBatchAudit {
            build,
            exporter_sha256: exporter_sha256.to_owned(),
            checks,
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
    fn validates_batch_count_known_paths_and_global_declaration_uniqueness() -> Result<()> {
        let modules = [("First.lean", ""), ("Second.lean", "")];
        let first = ("First.lean", "P.one");
        let second = ("Second.lean", "Q.two");
        let declarations: Vec<_> = (0..=MAX_MANAGED_T2_THEOREMS)
            .map(|index| format!("P.target{index}"))
            .collect();
        let many: Vec<_> = declarations
            .iter()
            .map(|declaration| ("First.lean", declaration.as_str()))
            .collect();
        assert!(validate_targets(&modules, &many).is_err());
        assert_eq!(
            validate_targets(&modules, &many[..MAX_MANAGED_T2_THEOREMS])?.len(),
            MAX_MANAGED_T2_THEOREMS
        );
        for bad in [
            &[][..],
            &[first, first],
            &[first, (second.0, first.1)],
            &[first, ("Third.lean", second.1)],
            &[first, ("SECond.lean", second.1)],
            &[first, ("../Second.lean", second.1)],
            &[first, (second.0, "unqualified")],
        ] {
            assert!(validate_targets(&modules, bad).is_err(), "{bad:?}");
        }
        let targets = validate_targets(&modules, &[second, first])?;
        assert_eq!(targets[0].module, "Second");
        assert_eq!(targets[1].declaration, "P.one");
        Ok(())
    }

    #[test]
    fn observes_targets_on_arrival_and_retains_original_slots() -> Result<()> {
        let modules = [
            ("First.lean", ""),
            ("Untargeted.lean", ""),
            ("Last.lean", ""),
        ];
        let selected = [
            ("Last.lean", "Example.last"),
            ("First.lean", "Example.first"),
            ("First.lean", "Example.another"),
        ];
        let mut checks: Vec<_> = validate_targets(&modules, &selected)?
            .into_iter()
            .map(|target| (target, None))
            .collect();
        let mut observed = vec![];
        for (path, _) in modules {
            observe_module(&mut checks, path_for(Engine::Lean4, path)?, |target| {
                observed.push(target.declaration);
                Ok(observed.len())
            })?;
        }
        assert_eq!(
            observed,
            ["Example.first", "Example.another", "Example.last"]
        );
        assert_eq!(
            checks.iter().map(|(_, check)| *check).collect::<Vec<_>>(),
            [Some(3), Some(1), Some(2)]
        );

        for (_, check) in &mut checks {
            *check = None;
        }
        let error = observe_module(&mut checks, "First", |target| {
            anyhow::bail!("failed at {}", target.declaration)
        })
        .unwrap_err();
        assert_eq!(error.to_string(), "failed at Example.first");
        assert!(checks.iter().all(|(_, check)| check.is_none()));
        Ok(())
    }

    #[test]
    fn independent_phase_budget_uses_remaining_time_and_fails_closed() -> Result<()> {
        let start = Instant::now();
        assert_eq!(phase_timeout(start + INDEPENDENT_TIMEOUT)?, TIMEOUT);
        let short = Duration::from_secs(10);
        assert!(phase_timeout(start + short)? <= short);
        assert!(phase_timeout(start).is_err());
        let mut exhausted = Duration::ZERO;
        assert!(
            within_check_budget(&mut exhausted, |_| -> Result<()> {
                panic!("must not check after exhaustion");
            })
            .is_err()
        );
        Ok(())
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
