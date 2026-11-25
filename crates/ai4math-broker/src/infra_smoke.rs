//! Native Lean smoke and completion audits for infrastructure declarations.
use super::{
    Broker, Engine, Lane, Pressure, Toolchain,
    breaker_probe::{
        TOTAL_TIMEOUT, build_committed_sources, command_timeout, identifier,
        reject_installation_shadows,
    },
    dependencies_digest, digest, path_for, sandbox, stage_source, validate_modules,
};
use anyhow::{Context, Result, ensure};
use std::{collections::HashSet, fs, time::Instant};

const CHECKER_FILE: &str = "Ai4mathSealedInfraSmoke.lean";
const CHECKER: &str = include_str!("infra_smoke_audit.lean");
const SMOKE_SEAL: &str = "ai4math:sealed-infra-smoke-audit:v1\n";
const COMPLETE_SEAL: &str = "ai4math:sealed-infra-complete-audit:v1\n";
const MAX_DECLARATIONS: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AuditMode {
    Smoke,
    Complete,
}

impl AuditMode {
    fn cli_arg(self) -> &'static str {
        match self {
            Self::Smoke => "smoke",
            Self::Complete => "complete",
        }
    }

    fn min_lemmas(self) -> usize {
        match self {
            Self::Smoke => 1,
            Self::Complete => 0,
        }
    }

    fn seal(self) -> &'static str {
        match self {
            Self::Smoke => SMOKE_SEAL,
            Self::Complete => COMPLETE_SEAL,
        }
    }

    fn receipt_domain(self) -> &'static str {
        match self {
            Self::Smoke => "ai4math:lean-infra-smoke:v1",
            Self::Complete => "ai4math:lean-infra-complete:v1",
        }
    }

    fn lemma_kind(self) -> &'static str {
        match self {
            Self::Smoke => "smoke lemmas",
            Self::Complete => "API lemmas",
        }
    }
}

/// Sealed, all-or-nothing evidence that an infrastructure module compiles with
/// its full ordered project slice and that its audited definitions and smoke
/// lemmas pass native kernel replay and axiom checks.
#[derive(Debug)]
#[must_use]
pub struct VerifiedInfraSmoke {
    commit: String,
    module_path: String,
    definitions: Vec<String>,
    smoke_lemmas: Vec<String>,
    sources_sha256: String,
    compiler_sha256: String,
    receipt_sha256: String,
}

impl VerifiedInfraSmoke {
    pub fn commit(&self) -> &str {
        &self.commit
    }
    pub fn module_path(&self) -> &str {
        &self.module_path
    }
    pub fn definitions(&self) -> &[String] {
        &self.definitions
    }
    pub fn smoke_lemmas(&self) -> &[String] {
        &self.smoke_lemmas
    }
    pub fn sources_sha256(&self) -> &str {
        &self.sources_sha256
    }
    pub fn compiler_sha256(&self) -> &str {
        &self.compiler_sha256
    }
    pub fn receipt_sha256(&self) -> &str {
        &self.receipt_sha256
    }
}

/// Sealed, all-or-nothing evidence that an infrastructure module and its
/// audited definitions and API lemmas compile cleanly with the full ordered
/// project slice and pass native kernel replay and axiom checks.
#[derive(Debug)]
#[must_use]
pub struct VerifiedInfraCompletion {
    commit: String,
    module_path: String,
    definitions: Vec<String>,
    api_lemmas: Vec<String>,
    sources_sha256: String,
    compiler_sha256: String,
    receipt_sha256: String,
}

impl VerifiedInfraCompletion {
    pub fn commit(&self) -> &str {
        &self.commit
    }
    pub fn module_path(&self) -> &str {
        &self.module_path
    }
    pub fn definitions(&self) -> &[String] {
        &self.definitions
    }
    pub fn api_lemmas(&self) -> &[String] {
        &self.api_lemmas
    }
    pub fn sources_sha256(&self) -> &str {
        &self.sources_sha256
    }
    pub fn compiler_sha256(&self) -> &str {
        &self.compiler_sha256
    }
    pub fn receipt_sha256(&self) -> &str {
        &self.receipt_sha256
    }
}

/// Digest of the exact source paths and UTF-8 bytes in verification order.
pub fn lean_infra_sources_sha256(sources: &[(&str, &str)]) -> String {
    dependencies_digest(sources)
}

fn valid_commit(commit: &str) -> bool {
    matches!(commit.len(), 40 | 64)
        && commit
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        && commit.bytes().any(|b| b != b'0')
}

fn valid_dotted_identifier(name: &str) -> bool {
    !name.is_empty() && name.split('.').all(identifier)
}

fn validate_identifier_list(names: &[&str], min_len: usize, kind: &str) -> Result<()> {
    ensure!(
        names.len() >= min_len && names.len() <= MAX_DECLARATIONS,
        "expected between {min_len} and {MAX_DECLARATIONS} {kind}"
    );
    let mut seen = HashSet::with_capacity(names.len());
    for &name in names {
        ensure!(
            valid_dotted_identifier(name),
            "expected valid dotted Lean identifier in {kind}: {name:?}"
        );
        ensure!(
            seen.insert(name),
            "duplicate Lean identifier in {kind}: {name:?}"
        );
    }
    Ok(())
}

struct InfraAuditRequest<'a> {
    commit: &'a str,
    module_path: &'a str,
    definitions: &'a [&'a str],
    lemmas: &'a [&'a str],
    sources: &'a [(&'a str, &'a str)],
    mode: AuditMode,
}

fn validate_inputs<'a>(tool: &Toolchain, req: &InfraAuditRequest<'a>) -> Result<&'a str> {
    ensure!(
        tool.engine == Engine::Lean4,
        "infra audit requires native Lean"
    );
    ensure!(
        valid_commit(req.commit),
        "expected a non-zero 40- or 64-character lowercase hex Git commit hash"
    );
    validate_identifier_list(req.definitions, 1, "definitions")?;
    validate_identifier_list(req.lemmas, req.mode.min_lemmas(), req.mode.lemma_kind())?;
    let module = path_for(Engine::Lean4, req.module_path)?;
    validate_modules(Engine::Lean4, req.sources, Some(CHECKER_FILE))?;
    reject_installation_shadows(tool, req.sources)?;
    ensure!(
        req.sources.iter().any(|&(path, _)| path == req.module_path),
        "target module {:?} must be present in sources",
        req.module_path
    );
    Ok(module)
}

fn require_seal(log: &str, mode: AuditMode) -> Result<()> {
    ensure!(
        log == mode.seal(),
        "missing or inexact sealed Lean infra {} audit: {log:?}",
        mode.cli_arg()
    );
    Ok(())
}

fn receipt_digest(
    mode: AuditMode,
    commit: &str,
    module_path: &str,
    definitions: &[&str],
    lemmas: &[&str],
    sources_sha256: &str,
    compiler_sha256: &str,
) -> Result<String> {
    Ok(digest(&serde_json::to_vec(&(
        mode.receipt_domain(),
        commit,
        module_path,
        definitions,
        lemmas,
        sources_sha256,
        CHECKER_FILE,
        digest(CHECKER.as_bytes()),
        compiler_sha256,
    ))?))
}

struct VerifiedInfraParts {
    sources_sha256: String,
    compiler_sha256: String,
    receipt_sha256: String,
}

impl Broker {
    fn run_lean_infra_audit(
        &self,
        tool: &Toolchain,
        req: &InfraAuditRequest<'_>,
    ) -> Result<VerifiedInfraParts> {
        let module = validate_inputs(tool, req)?;
        let _lease = self.resources.acquire(Lane::Heavy, Pressure::Red)?;
        let deadline = Instant::now() + TOTAL_TIMEOUT;

        let build = tempfile::tempdir()?;
        let build_path = build.path().canonicalize()?;
        build_committed_sources(tool, &build_path, req.sources, deadline, None)?;
        let objects = build_path.join("objects");

        let audit = tempfile::tempdir()?;
        let writable = audit.path().join("scratch");
        fs::create_dir(&writable)?;
        stage_source(
            Engine::Lean4,
            audit.path(),
            &writable,
            CHECKER_FILE,
            CHECKER,
        )?;

        let mut args = vec![
            "--run".into(),
            CHECKER_FILE.into(),
            objects.to_str().context("non-UTF-8 object path")?.into(),
            module.into(),
            req.mode.cli_arg().into(),
            req.definitions.len().to_string(),
        ];
        args.extend(req.definitions.iter().map(|name| (*name).to_owned()));
        args.push(req.lemmas.len().to_string());
        args.extend(req.lemmas.iter().map(|name| (*name).to_owned()));

        // No LEAN_PATH on startup: only installed Lean modules can supply
        // syntax or run import hooks while the broker checker is elaborated.
        let log = sandbox::run_with_data(
            sandbox::Invocation {
                executable: &tool.executable,
                args: &args,
                room: audit.path(),
                writable: &writable,
                reads: &tool.reads,
                env: &[],
                timeout: command_timeout(deadline)?,
            },
            std::slice::from_ref(&objects),
            &[],
        )
        .with_context(|| {
            format!(
                "kernel-check sealed Lean infra {} audit",
                req.mode.cli_arg()
            )
        })?;
        command_timeout(deadline)?;
        require_seal(&log, req.mode)?;

        let sources_sha256 = lean_infra_sources_sha256(req.sources);
        let compiler_sha256 = tool.binary_sha256.clone();
        let receipt_sha256 = receipt_digest(
            req.mode,
            req.commit,
            req.module_path,
            req.definitions,
            req.lemmas,
            &sources_sha256,
            &compiler_sha256,
        )?;
        Ok(VerifiedInfraParts {
            sources_sha256,
            compiler_sha256,
            receipt_sha256,
        })
    }

    /// Verify a Lean infrastructure module's definitions and smoke lemmas in a
    /// clean room after compiling the full ordered project slice.
    pub fn verify_lean_infra_smoke(
        &self,
        tool: &Toolchain,
        commit: &str,
        module_path: &str,
        definitions: &[&str],
        smoke_lemmas: &[&str],
        sources: &[(&str, &str)],
    ) -> Result<VerifiedInfraSmoke> {
        let parts = self.run_lean_infra_audit(
            tool,
            &InfraAuditRequest {
                commit,
                module_path,
                definitions,
                lemmas: smoke_lemmas,
                sources,
                mode: AuditMode::Smoke,
            },
        )?;
        Ok(VerifiedInfraSmoke {
            commit: commit.into(),
            module_path: module_path.into(),
            definitions: definitions.iter().map(|name| (*name).to_owned()).collect(),
            smoke_lemmas: smoke_lemmas.iter().map(|name| (*name).to_owned()).collect(),
            sources_sha256: parts.sources_sha256,
            compiler_sha256: parts.compiler_sha256,
            receipt_sha256: parts.receipt_sha256,
        })
    }

    /// Verify a Lean infrastructure module's definitions and API lemmas in a
    /// clean room after compiling the full ordered project slice.
    pub fn verify_lean_infra_completion(
        &self,
        tool: &Toolchain,
        commit: &str,
        module_path: &str,
        definitions: &[&str],
        api_lemmas: &[&str],
        sources: &[(&str, &str)],
    ) -> Result<VerifiedInfraCompletion> {
        let parts = self.run_lean_infra_audit(
            tool,
            &InfraAuditRequest {
                commit,
                module_path,
                definitions,
                lemmas: api_lemmas,
                sources,
                mode: AuditMode::Complete,
            },
        )?;
        Ok(VerifiedInfraCompletion {
            commit: commit.into(),
            module_path: module_path.into(),
            definitions: definitions.iter().map(|name| (*name).to_owned()).collect(),
            api_lemmas: api_lemmas.iter().map(|name| (*name).to_owned()).collect(),
            sources_sha256: parts.sources_sha256,
            compiler_sha256: parts.compiler_sha256,
            receipt_sha256: parts.receipt_sha256,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AgdaMode;
    use std::path::PathBuf;

    fn fake_toolchain(engine: Engine) -> Toolchain {
        Toolchain {
            engine,
            executable: PathBuf::from("/nonexistent/lean"),
            reads: Vec::new(),
            agda_mode: AgdaMode::default(),
            agda_data: None,
            rocq_install: None,
            binary_sha256: "b".repeat(64),
            sandbox_group: sandbox::ProcessGroup::OwnSession,
        }
    }

    #[test]
    fn validates_commit_hashes_strictly() {
        let valid40 = "0123456789abcdef".repeat(2) + "01234567";
        let valid64 = "0123456789abcdef".repeat(4);
        assert!(valid_commit(&valid40));
        assert!(valid_commit(&valid64));

        for bad in [
            "",
            &"0".repeat(40),
            &"0".repeat(64),
            &"a".repeat(39),
            &"a".repeat(41),
            &"a".repeat(63),
            &"a".repeat(65),
            &"A".repeat(40),
            &"A".repeat(64),
            &("a".repeat(39) + "g"),
            &("a".repeat(39) + "A"),
        ] {
            assert!(!valid_commit(bad), "commit should be rejected: {bad:?}");
        }
    }

    #[test]
    fn validates_dotted_identifiers_and_bounds() {
        for good in ["Foo", "Foo.bar", "Ns.Sub.helper'", "_helper", "A._b.c1'"] {
            assert!(valid_dotted_identifier(good), "should accept {good:?}");
        }
        for bad in [
            "", "_", "Foo._", "_.Foo", ".Foo", "Foo.", "Foo..Bar", "1Foo", "Foo.1Bar", "Foo bar",
            "Foo-Bar",
        ] {
            assert!(!valid_dotted_identifier(bad), "should reject {bad:?}");
        }

        let sixteen: Vec<String> = (0..16).map(|i| format!("Ns.decl_{i}")).collect();
        let sixteen_refs: Vec<&str> = sixteen.iter().map(String::as_str).collect();
        let seventeen: Vec<String> = (0..17).map(|i| format!("Ns.decl_{i}")).collect();
        let seventeen_refs: Vec<&str> = seventeen.iter().map(String::as_str).collect();

        assert!(validate_identifier_list(&[], 1, "definitions").is_err());
        assert!(validate_identifier_list(&[], 0, "API lemmas").is_ok());
        assert!(validate_identifier_list(&sixteen_refs, 1, "definitions").is_ok());
        assert!(validate_identifier_list(&seventeen_refs, 1, "definitions").is_err());
        assert!(validate_identifier_list(&seventeen_refs, 0, "API lemmas").is_err());
        assert!(validate_identifier_list(&["Foo.a", "Foo.a"], 1, "definitions").is_err());
    }

    fn check_inputs<'a>(
        tool: &Toolchain,
        commit: &'a str,
        module_path: &'a str,
        definitions: &'a [&'a str],
        lemmas: &'a [&'a str],
        sources: &'a [(&'a str, &'a str)],
        mode: AuditMode,
    ) -> Result<&'a str> {
        validate_inputs(
            tool,
            &InfraAuditRequest {
                commit,
                module_path,
                definitions,
                lemmas,
                sources,
                mode,
            },
        )
    }

    #[test]
    fn input_validation_rejects_invalid_requests_before_sandbox() -> Result<()> {
        let lean = fake_toolchain(Engine::Lean4);
        let agda = fake_toolchain(Engine::Agda);
        let rocq = fake_toolchain(Engine::Rocq);
        let commit40 = "a".repeat(40);
        let commit64 = "f".repeat(64);
        let sources = [
            ("Base.lean", "def base := 1\n"),
            ("Infra.lean", "import Base\ndef x := base\n"),
            ("Downstream.lean", "import Infra\ndef y := x\n"),
        ];

        // Valid smoke and completion inputs pass validation even when module_path is not last.
        assert_eq!(
            check_inputs(
                &lean,
                &commit40,
                "Infra.lean",
                &["Ns.MyStruct", "x"],
                &["smoke_x"],
                &sources,
                AuditMode::Smoke,
            )?,
            "Infra"
        );
        assert_eq!(
            check_inputs(
                &lean,
                &commit64,
                "Infra.lean",
                &["Ns.MyStruct"],
                &[],
                &sources,
                AuditMode::Complete,
            )?,
            "Infra"
        );

        // Engine mismatch.
        for wrong in [&agda, &rocq] {
            assert!(
                check_inputs(
                    wrong,
                    &commit40,
                    "Infra.lean",
                    &["x"],
                    &["smoke_x"],
                    &sources,
                    AuditMode::Smoke,
                )
                .is_err()
            );
        }

        // Bad commit hashes (all zeros, uppercase, wrong length).
        for bad_commit in [&"0".repeat(40), &"0".repeat(64), &"A".repeat(40), "abc"] {
            assert!(
                check_inputs(
                    &lean,
                    bad_commit,
                    "Infra.lean",
                    &["x"],
                    &["smoke_x"],
                    &sources,
                    AuditMode::Smoke,
                )
                .is_err()
            );
        }

        // Smoke requires at least 1 lemma; both modes require at least 1 definition.
        assert!(
            check_inputs(
                &lean,
                &commit40,
                "Infra.lean",
                &["x"],
                &[],
                &sources,
                AuditMode::Smoke,
            )
            .is_err()
        );
        assert!(
            check_inputs(
                &lean,
                &commit40,
                "Infra.lean",
                &[],
                &["smoke_x"],
                &sources,
                AuditMode::Smoke,
            )
            .is_err()
        );
        assert!(
            check_inputs(
                &lean,
                &commit40,
                "Infra.lean",
                &[],
                &[],
                &sources,
                AuditMode::Complete,
            )
            .is_err()
        );

        // Missing target module in sources.
        assert!(
            check_inputs(
                &lean,
                &commit40,
                "Missing.lean",
                &["x"],
                &["smoke_x"],
                &sources,
                AuditMode::Smoke,
            )
            .is_err()
        );

        // Invalid module path, reserved checker name, module collisions, and installation shadows.
        assert!(
            check_inputs(
                &lean,
                &commit40,
                "../Infra.lean",
                &["x"],
                &["smoke_x"],
                &sources,
                AuditMode::Smoke,
            )
            .is_err()
        );
        assert!(
            check_inputs(
                &lean,
                &commit40,
                CHECKER_FILE,
                &["x"],
                &["smoke_x"],
                &[(CHECKER_FILE, "def x := 1\n")],
                AuditMode::Smoke,
            )
            .is_err()
        );
        assert!(
            check_inputs(
                &lean,
                &commit40,
                "Infra.lean",
                &["x"],
                &["smoke_x"],
                &[("Infra.lean", ""), ("infra.lean", "")],
                AuditMode::Smoke,
            )
            .is_err()
        );
        assert!(
            check_inputs(
                &lean,
                &commit40,
                "Infra.lean",
                &["x"],
                &["smoke_x"],
                &[("Init.lean", ""), ("Infra.lean", "def x := 1\n")],
                AuditMode::Smoke,
            )
            .is_err()
        );

        // Broker entry points fail early on validation errors without invoking sandbox.
        let broker = Broker::new();
        assert!(
            broker
                .verify_lean_infra_smoke(&agda, &commit40, "Infra.lean", &["x"], &["s"], &sources)
                .is_err()
        );
        assert!(
            broker
                .verify_lean_infra_completion(
                    &lean,
                    &"0".repeat(40),
                    "Infra.lean",
                    &["x"],
                    &[],
                    &sources,
                )
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn seal_must_be_exact_and_mode_specific() {
        assert!(require_seal(SMOKE_SEAL, AuditMode::Smoke).is_ok());
        assert!(require_seal(COMPLETE_SEAL, AuditMode::Complete).is_ok());
        assert!(require_seal(SMOKE_SEAL, AuditMode::Complete).is_err());
        assert!(require_seal(COMPLETE_SEAL, AuditMode::Smoke).is_err());
        for bad in [
            "",
            SMOKE_SEAL.trim_end(),
            &format!("warning\n{SMOKE_SEAL}"),
            &format!("{SMOKE_SEAL}{SMOKE_SEAL}"),
        ] {
            assert!(require_seal(bad, AuditMode::Smoke).is_err());
        }
    }

    #[test]
    fn sources_and_receipt_digests_bind_all_audited_fields() -> Result<()> {
        let a = ("Base.lean", "def base := 1\n");
        let b = ("Infra.lean", "import Base\ndef x := base\n");
        let sources_hash = lean_infra_sources_sha256(&[a, b]);
        assert_eq!(sources_hash, dependencies_digest(&[a, b]));
        assert_ne!(sources_hash, lean_infra_sources_sha256(&[b, a]));
        assert_ne!(
            sources_hash,
            lean_infra_sources_sha256(&[a, (b.0, "import Base\ndef x := base\r\n")])
        );

        let commit = "a".repeat(40);
        let compiler = "b".repeat(64);
        let defs = ["MyStruct", "origin"];
        let lemmas = ["origin_x"];
        let smoke = receipt_digest(
            AuditMode::Smoke,
            &commit,
            b.0,
            &defs,
            &lemmas,
            &sources_hash,
            &compiler,
        )?;
        let complete = receipt_digest(
            AuditMode::Complete,
            &commit,
            b.0,
            &defs,
            &lemmas,
            &sources_hash,
            &compiler,
        )?;
        assert_ne!(smoke, complete);
        assert_eq!(
            smoke,
            receipt_digest(
                AuditMode::Smoke,
                &commit,
                b.0,
                &defs,
                &lemmas,
                &sources_hash,
                &compiler,
            )?
        );
        assert_ne!(
            smoke,
            receipt_digest(
                AuditMode::Smoke,
                &commit,
                b.0,
                &[defs[1], defs[0]],
                &lemmas,
                &sources_hash,
                &compiler,
            )?
        );
        assert_ne!(
            smoke,
            receipt_digest(
                AuditMode::Smoke,
                &commit,
                b.0,
                &defs,
                &lemmas,
                &sources_hash,
                &"c".repeat(64),
            )?
        );
        Ok(())
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "set AI4MATH_TEST_LEAN to the native Lean executable and run explicitly"]
    fn native_lean_infra_smoke_and_completion_seal_and_reject_unsound_cases() {
        let lean = std::env::var_os("AI4MATH_TEST_LEAN")
            .or_else(|| std::env::var_os("AI4MATH_TEST_NATIVE_LEAN"))
            .expect("native Lean path");
        let tool = Toolchain::lean(lean).unwrap();
        let broker = Broker::new();
        let commit = "a".repeat(40);

        let support = ("Support.lean", "def baseWeight : Nat := 7\n");
        let infra = (
            "Infra.lean",
            "import Support\n\
             structure WeightedBox where\n\
               weight : Nat\n\
               positive : weight > 0\n\
             class HasBox (α : Type) where\n\
               mkBox : α → WeightedBox\n\
             def defaultBox : WeightedBox := ⟨baseWeight, by decide⟩\n\
             instance instHasBoxNat : HasBox Nat where\n\
               mkBox n := ⟨n + baseWeight, Nat.add_pos_right n (by decide)⟩\n\
             namespace Smoke\n\
             theorem defaultBox_weight : defaultBox.weight = baseWeight := rfl\n\
             theorem box_positive (b : WeightedBox) : b.weight > 0 := b.positive\n\
             end Smoke\n\
             theorem helper_arith : baseWeight = 7 := rfl\n",
        );
        let downstream = (
            "Downstream.lean",
            "import Infra\n\
             def downstreamCheck : WeightedBox := HasBox.mkBox 3\n",
        );
        let sources = [support, infra, downstream];
        let defs = ["WeightedBox", "HasBox", "defaultBox"];
        let smoke_lemmas = [
            "Smoke.defaultBox_weight",
            "instHasBoxNat",
            "Smoke.box_positive",
        ];

        let verified_smoke = broker
            .verify_lean_infra_smoke(&tool, &commit, "Infra.lean", &defs, &smoke_lemmas, &sources)
            .unwrap();
        assert_eq!(verified_smoke.commit(), commit);
        assert_eq!(verified_smoke.module_path(), "Infra.lean");
        assert_eq!(verified_smoke.definitions(), &defs);
        assert_eq!(verified_smoke.smoke_lemmas(), &smoke_lemmas);
        assert_eq!(
            verified_smoke.sources_sha256(),
            lean_infra_sources_sha256(&sources)
        );
        assert_eq!(verified_smoke.compiler_sha256(), tool.compiler_sha256());
        assert_eq!(
            verified_smoke.receipt_sha256(),
            broker
                .verify_lean_infra_smoke(
                    &tool,
                    &commit,
                    "Infra.lean",
                    &defs,
                    &smoke_lemmas,
                    &sources,
                )
                .unwrap()
                .receipt_sha256()
        );

        // Completion check accepts theorem API lemmas and an empty API lemma list,
        // and rejects arbitrary definitions passed as API lemmas.
        let api_lemmas = [
            "Smoke.defaultBox_weight",
            "Smoke.box_positive",
            "helper_arith",
        ];
        let verified_complete = broker
            .verify_lean_infra_completion(
                &tool,
                &commit,
                "Infra.lean",
                &defs,
                &api_lemmas,
                &sources,
            )
            .unwrap();
        assert_eq!(verified_complete.commit(), commit);
        assert_eq!(verified_complete.module_path(), "Infra.lean");
        assert_eq!(verified_complete.definitions(), &defs);
        assert_eq!(verified_complete.api_lemmas(), &api_lemmas);
        assert_eq!(
            verified_complete.sources_sha256(),
            lean_infra_sources_sha256(&sources)
        );
        assert_eq!(verified_complete.compiler_sha256(), tool.compiler_sha256());
        assert_ne!(
            verified_smoke.receipt_sha256(),
            verified_complete.receipt_sha256()
        );

        let non_thm_complete_err = broker
            .verify_lean_infra_completion(
                &tool,
                &commit,
                "Infra.lean",
                &defs,
                &["instHasBoxNat"],
                &sources,
            )
            .unwrap_err();
        assert!(
            format!("{non_thm_complete_err:#}").contains("must be a theorem"),
            "unexpected non-theorem completion error: {non_thm_complete_err:#}"
        );

        let verified_complete_empty = broker
            .verify_lean_infra_completion(&tool, &commit, "Infra.lean", &defs, &[], &sources)
            .unwrap();
        assert!(verified_complete_empty.api_lemmas().is_empty());

        // Unrelated True smoke lemma must be rejected in smoke mode.
        let unrelated_src = format!("{}theorem unrelated_true : True := trivial\n", infra.1);
        let unrelated_err = broker
            .verify_lean_infra_smoke(
                &tool,
                &commit,
                "Infra.lean",
                &defs,
                &["Smoke.defaultBox_weight", "unrelated_true"],
                &[support, ("Infra.lean", &unrelated_src), downstream],
            )
            .unwrap_err();
        assert!(
            format!("{unrelated_err:#}").contains("does not directly reference"),
            "unexpected unrelated smoke error: {unrelated_err:#}"
        );

        // sorry and custom axioms must be rejected in both smoke and completion modes.
        for (lemma_name, snippet) in [
            (
                "sorry_smoke",
                "theorem sorry_smoke : defaultBox.weight = 999 := by sorry\n",
            ),
            (
                "axiom_smoke",
                "axiom fake_ax : defaultBox.weight = 999\n\
                 theorem axiom_smoke : defaultBox.weight = 999 := fake_ax\n",
            ),
        ] {
            let bad_src = format!("{}{snippet}", infra.1);
            let bad_sources = [support, ("Infra.lean", &bad_src), downstream];
            let smoke_err = broker
                .verify_lean_infra_smoke(
                    &tool,
                    &commit,
                    "Infra.lean",
                    &defs,
                    &[lemma_name],
                    &bad_sources,
                )
                .unwrap_err();
            assert!(
                format!("{smoke_err:#}").contains("non-kernel axiom"),
                "unexpected smoke error for {lemma_name}: {smoke_err:#}"
            );
            let complete_err = broker
                .verify_lean_infra_completion(
                    &tool,
                    &commit,
                    "Infra.lean",
                    &defs,
                    &[lemma_name],
                    &bad_sources,
                )
                .unwrap_err();
            assert!(
                format!("{complete_err:#}").contains("non-kernel axiom"),
                "unexpected completion error for {lemma_name}: {complete_err:#}"
            );
        }

        // Downstream breakage fails the full ordered project build.
        let broken_downstream = (
            "Downstream.lean",
            "import Infra\ndef broken : Nat := \"not a nat\"\n",
        );
        assert!(
            broker
                .verify_lean_infra_smoke(
                    &tool,
                    &commit,
                    "Infra.lean",
                    &defs,
                    &smoke_lemmas,
                    &[support, infra, broken_downstream],
                )
                .is_err()
        );
    }
}
