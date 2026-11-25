//! Shared admission and independent kernel-replay policy for native Lean.
use super::{
    AXIOMS, TIMEOUT, Toolchain, digest, path_for, sandbox, stage_source, validate_modules,
};
use anyhow::{Context, Result, ensure};
use std::{fs, path::Path};

pub(super) const CHECKER_FILE: &str = "Ai4mathSealedLeanKernel.lean";
const CHECKER: &str = include_str!("lean_kernel_audit.lean");
const SEAL: &str = "ai4math:sealed-lean-kernel-audit:v1 ";
const CHECKER_HASH_DOMAIN: &str = "ai4math:lean-kernel-policy:checker-source:v1";
// Update this pinned source identity and its unit golden together with CHECKER.
const CHECKER_SHA256: &str = "890637e92eeb329fa34ed1aa60b6e515ceb3f48c9f32678368c63465c72978b3";

/// Common policy for every broker entrypoint that admits Lean source modules or
/// authorizes a candidate after native verification.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct LeanKernelPolicy;

impl LeanKernelPolicy {
    pub(super) fn validate_modules(
        tool: &Toolchain,
        modules: &[(&str, &str)],
        target: Option<&str>,
    ) -> Result<()> {
        ensure!(
            tool.engine == super::Engine::Lean4,
            "Lean policy requires native Lean"
        );
        validate_modules(super::Engine::Lean4, modules, target)?;
        reject_installation_shadows(tool, modules)?;
        if let Some(target) = target {
            reject_installation_shadow(tool, target)?;
        }
        Ok(())
    }

    /// Replay the exact target theorem and its used dependency closure through a
    /// stock Lean kernel environment. Any loader, policy, or replay failure is
    /// fatal; there is deliberately no fallback to the ordinary compiler check.
    pub(super) fn replay(
        tool: &Toolchain,
        objects: &Path,
        module: &str,
        target: &str,
    ) -> Result<Vec<String>> {
        ensure!(
            objects.join(format!("{module}.olean")).is_file(),
            "missing Lean replay object for target module {module:?}"
        );
        let objects = objects
            .canonicalize()
            .context("resolve Lean replay objects")?;
        let objects_text = objects
            .to_str()
            .context("non-UTF-8 Lean replay object path")?;
        run_checker(
            tool,
            &objects,
            vec![
                "--run".into(),
                CHECKER_FILE.into(),
                "target".into(),
                objects_text.into(),
                module.into(),
                target.into(),
            ],
        )
    }

    /// Replay every public declaration owned by the supplied modules. This is
    /// the fail-closed policy for local checks and downstream regression modules
    /// that have no single caller-selected theorem root.
    pub(super) fn replay_modules(
        tool: &Toolchain,
        objects: &Path,
        modules: &[&str],
    ) -> Result<Vec<String>> {
        ensure!(!modules.is_empty(), "empty Lean module replay set");
        for &module in modules {
            path_for(super::Engine::Lean4, &format!("{module}.lean"))?;
            ensure!(
                objects.join(format!("{module}.olean")).is_file(),
                "missing Lean replay object for module {module:?}"
            );
        }
        let objects = objects
            .canonicalize()
            .context("resolve Lean replay objects")?;
        let objects_text = objects
            .to_str()
            .context("non-UTF-8 Lean replay object path")?;
        let mut args = vec![
            "--run".into(),
            CHECKER_FILE.into(),
            "modules".into(),
            objects_text.into(),
            modules.len().to_string(),
        ];
        args.extend(modules.iter().map(|module| (*module).to_owned()));
        run_checker(tool, &objects, args)
    }

    /// Require the independent replay to agree with the ordinary compiler's
    /// axiom observation while preserving the receipt's original ordering.
    pub(super) fn replay_matching_report(
        tool: &Toolchain,
        objects: &Path,
        module: &str,
        target: &str,
        reported: &[String],
    ) -> Result<()> {
        let replayed = Self::replay(tool, objects, module, target)?;
        let mut expected = reported.to_vec();
        expected.sort_unstable();
        expected.dedup();
        ensure!(
            replayed == expected,
            "independent Lean kernel replay axioms differ from the compiler report: \
             replayed {replayed:?}, reported {reported:?}"
        );
        Ok(())
    }
}

fn run_checker(tool: &Toolchain, objects: &Path, args: Vec<String>) -> Result<Vec<String>> {
    ensure!(
        tool.engine == super::Engine::Lean4,
        "Lean replay requires native Lean"
    );
    ensure!(
        checker_source_sha256() == CHECKER_SHA256,
        "Lean kernel checker source/hash policy mismatch"
    );
    let objects = objects
        .canonicalize()
        .context("resolve independent Lean replay objects")?;
    ensure!(objects.is_dir(), "Lean replay objects must be a directory");

    let audit = tempfile::tempdir().context("create independent Lean replay room")?;
    let writable = audit.path().join("scratch");
    fs::create_dir(&writable)?;
    stage_source(
        super::Engine::Lean4,
        audit.path(),
        &writable,
        CHECKER_FILE,
        CHECKER,
    )?;
    // Candidate objects are data only. No LEAN_PATH is present while the sealed
    // checker starts, so candidate parsers, extensions, and initializers cannot
    // authorize its own elaboration.
    let log = sandbox::run_with_data(
        sandbox::Invocation {
            executable: &tool.executable,
            args: &args,
            room: audit.path(),
            writable: &writable,
            reads: &tool.reads,
            env: &[],
            timeout: TIMEOUT,
        },
        std::slice::from_ref(&objects),
        &[],
    )
    .context("independent Lean kernel replay failed")?;
    audit_report(&log)
}

pub(super) fn checker_source_sha256() -> String {
    digest(
        &serde_json::to_vec(&(CHECKER_HASH_DOMAIN, CHECKER_FILE, CHECKER))
            .expect("Lean checker source identity is serializable"),
    )
}

fn audit_report(log: &str) -> Result<Vec<String>> {
    let json = log
        .strip_prefix(SEAL)
        .and_then(|line| line.strip_suffix('\n'))
        .filter(|line| !line.contains('\n'))
        .context("missing or inexact sealed Lean kernel replay audit")?;
    let axioms: Vec<String> =
        serde_json::from_str(json).context("invalid sealed Lean kernel axiom list")?;
    ensure!(
        axioms.iter().all(|a| AXIOMS.contains(&a.as_str()))
            && axioms.windows(2).all(|pair| pair[0] < pair[1]),
        "non-L5 or ambiguous independently replayed Lean axioms: {axioms:?}"
    );
    Ok(axioms)
}

pub(super) fn reject_installation_shadows(
    tool: &Toolchain,
    sources: &[(&str, &str)],
) -> Result<()> {
    for &(path, _) in sources {
        reject_installation_shadow(tool, path)?;
    }
    Ok(())
}

fn reject_installation_shadow(tool: &Toolchain, path: &str) -> Result<()> {
    let module = path_for(super::Engine::Lean4, path)?;
    ensure!(
        !["init", "lean", "std"].contains(&module.to_ascii_lowercase().as_str())
            && !tool.reads.iter().any(|root| {
                root.join(module).is_dir() || root.join(format!("{module}.olean")).is_file()
            }),
        "module shadows the trusted Lean installation: {path:?}"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AgdaMode, Engine};

    fn fixture_tool(install: &Path) -> Toolchain {
        Toolchain {
            engine: Engine::Lean4,
            executable: install.join("unused"),
            reads: vec![install.to_owned()],
            agda_mode: AgdaMode::default(),
            agda_data: None,
            rocq_install: None,
            binary_sha256: String::new(),
            sandbox_group: sandbox::ProcessGroup::OwnSession,
        }
    }

    #[test]
    fn installation_shadow_policy_covers_reserved_installed_and_target_modules() -> Result<()> {
        let install = tempfile::tempdir()?;
        fs::create_dir(install.path().join("InstalledDir"))?;
        fs::write(install.path().join("InstalledObject.olean"), b"fixture")?;
        let tool = fixture_tool(install.path());
        LeanKernelPolicy::validate_modules(&tool, &[("Candidate.lean", "")], Some("Target.lean"))?;
        for path in [
            "Init.lean",
            "LeAn.lean",
            "STD.lean",
            "InstalledDir.lean",
            "InstalledObject.lean",
        ] {
            let error = LeanKernelPolicy::validate_modules(&tool, &[(path, "")], None).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("shadows the trusted Lean installation"),
                "{path}: {error:#}"
            );
        }
        for target in ["Init.lean", "InstalledObject.lean"] {
            let error = LeanKernelPolicy::validate_modules(&tool, &[], Some(target)).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("shadows the trusted Lean installation")
            );
        }
        Ok(())
    }

    #[test]
    fn checker_source_identity_and_sealed_report_are_pinned() -> Result<()> {
        assert_eq!(checker_source_sha256(), CHECKER_SHA256);
        assert_eq!(audit_report(&format!("{SEAL}[]\n"))?, Vec::<String>::new());
        assert_eq!(
            audit_report(&format!("{SEAL}[\"Classical.choice\",\"propext\"]\n"))?,
            ["Classical.choice", "propext"]
        );
        for invalid in [
            String::new(),
            format!("{SEAL}[]"),
            format!("warning\n{SEAL}[]\n"),
            format!("{SEAL}[\"propext\",\"propext\"]\n"),
            format!("{SEAL}[\"propext\",\"Classical.choice\"]\n"),
            format!("{SEAL}[\"sorryAx\"]\n"),
        ] {
            assert!(audit_report(&invalid).is_err(), "accepted {invalid:?}");
        }
        Ok(())
    }
}
