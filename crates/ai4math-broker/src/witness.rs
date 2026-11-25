//! Broker-only refutation checks against a sealed Lean proposition declaration.
use super::{
    Broker, DiagnosticOutput, Engine, FrozenTarget, Lane, Pressure, ProofBodySubmission,
    SOURCE_CAP, Toolchain, dependencies_digest, digest, lean_axioms, path_for, render_proof_body,
    stage_source, t3, validate_modules,
};
use ai4math_evidence::WitnessEvidence;
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use std::{
    fs,
    time::{SystemTime, UNIX_EPOCH},
};

const CHECKER_FILE: &str = "Ai4mathSealedWitness.lean";
const WITNESS: &str = "witness";
const TYPE: &str = include_str!("witness_type.lean");
const GUARD: &str = include_str!("witness_audit.lean");
const AUDIT: &str = "\n#ai4math_check_sealed_witness\n";

/// Evidence of a native Lean refutation, not a promotable proof candidate.
/// A trusted coordinator supplies the exact source and ordered dependencies
/// read from the named commit; the broker never resolves candidate Git input.
#[derive(Debug, Serialize)]
#[must_use]
pub struct VerifiedWitness {
    base_commit: String,
    file_path: String,
    proposition_id: String,
    source_sha256: String,
    proof_body_sha256: String,
    replay_source_sha256: String,
    dependencies_sha256: String,
    compiler_sha256: String,
    observed_axioms: Vec<String>,
    issued_unix_seconds: u64,
    witness_sha256: String,
    #[serde(skip)]
    source: String,
}

impl VerifiedWitness {
    pub fn base_commit(&self) -> &str {
        &self.base_commit
    }
    pub fn file_path(&self) -> &str {
        &self.file_path
    }
    pub fn proposition_id(&self) -> &str {
        &self.proposition_id
    }
    pub fn proposition_name(&self) -> &str {
        self.proposition_id()
    }
    pub fn source_sha256(&self) -> &str {
        &self.source_sha256
    }
    pub fn proof_body_sha256(&self) -> &str {
        &self.proof_body_sha256
    }
    pub fn replay_source_sha256(&self) -> &str {
        &self.replay_source_sha256
    }
    pub fn dependencies_sha256(&self) -> &str {
        &self.dependencies_sha256
    }
    pub fn compiler_sha256(&self) -> &str {
        &self.compiler_sha256
    }
    pub fn observed_axioms(&self) -> &[String] {
        &self.observed_axioms
    }
    pub fn issued_unix_seconds(&self) -> u64 {
        self.issued_unix_seconds
    }
    pub fn witness_sha256(&self) -> &str {
        &self.witness_sha256
    }
    pub fn receipt_sha256(&self) -> &str {
        self.witness_sha256()
    }

    /// The complete checker module. To replay, stage it at this path and compile
    /// it after the immutable, ordered dependency modules and sealed module.
    pub fn source(&self) -> &str {
        &self.source
    }
    pub fn replay_file_path(&self) -> &'static str {
        CHECKER_FILE
    }

    /// Match source/proof bytes, not the pretty-printed proposition or normalized RHS.
    pub fn matches(
        &self,
        base_commit: &str,
        file_path: &str,
        proposition_id: &str,
        committed_source: &[u8],
        proof_body: &[u8],
    ) -> bool {
        self.base_commit == base_commit
            && self.file_path == file_path
            && self.proposition_id == proposition_id
            && self.source_sha256 == digest(committed_source)
            && self.proof_body_sha256 == digest(proof_body)
    }

    fn compute_identity(&self) -> Result<String> {
        Ok(digest(&serde_json::to_vec(&(
            "ai4math:lean-sealed-witness:v1",
            &self.base_commit,
            &self.file_path,
            &self.proposition_id,
            &self.source_sha256,
            &self.proof_body_sha256,
            CHECKER_FILE,
            &self.replay_source_sha256,
            &self.dependencies_sha256,
            &self.compiler_sha256,
            &self.observed_axioms,
        ))?))
    }
}

// SAFETY: Production values are constructed only after the committed proposition
// and generated witness pass native Lean kernel and axiom verification. Fields
// remain private and the wire type has no deserializer.
unsafe impl WitnessEvidence for VerifiedWitness {
    fn base_commit(&self) -> &str {
        self.base_commit()
    }

    fn file_path(&self) -> &str {
        self.file_path()
    }

    fn proposition_id(&self) -> &str {
        self.proposition_id()
    }

    fn source_sha256(&self) -> &str {
        self.source_sha256()
    }

    fn proof_body_sha256(&self) -> &str {
        self.proof_body_sha256()
    }

    fn witness_sha256(&self) -> &str {
        self.witness_sha256()
    }

    fn source(&self) -> &str {
        self.source()
    }

    fn matches(
        &self,
        base_commit: &str,
        file_path: &str,
        proposition_id: &str,
        committed_source: &[u8],
        proof_body: &[u8],
    ) -> bool {
        VerifiedWitness::matches(
            self,
            base_commit,
            file_path,
            proposition_id,
            committed_source,
            proof_body,
        )
    }

    fn receipt_json(&self) -> Result<String> {
        Ok(serde_json::to_string(self)?)
    }
}

fn checker_source(module: &str, proposition: &str, rhs: &ProofBodySubmission) -> Result<String> {
    // Only normal ASCII root names can enter broker strings (never Lean expressions).
    ensure!(
        proposition.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
            && proposition
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_')
            && proposition != WITNESS,
        "expected a conventional root Lean proposition name other than witness"
    );
    let theorem = render_proof_body(
        &FrozenTarget {
            engine: Engine::Lean4,
            file_path: CHECKER_FILE.into(),
            target_id: WITNESS.into(),
            prefix: "theorem witness : ai4mathSealedRefutation := ".into(),
            suffix: "\n".into(),
        },
        rhs,
    )?;
    Ok(format!(
        concat!(
            "import Lean\nimport «{module}»\n\n",
            "private def ai4mathSealedModule : Lean.Name := Lean.Name.mkSimple \"{module}\"\n",
            "private def ai4mathSealedProposition : Lean.Name := Lean.Name.mkSimple \"{proposition}\"\n\n",
            "{TYPE}{theorem}\n{GUARD}"
        ),
        module = module,
        proposition = proposition,
        TYPE = TYPE,
        theorem = theorem,
        GUARD = GUARD
    ))
}

impl Broker {
    /// Kernel-check a proof RHS for the negation of the named committed proposition.
    /// The proposition must be a public, safe, closed, monomorphic definition of
    /// type `Prop` originating in `file_path`, for example `def target : Prop := 0 = 1`.
    /// A theorem, opaque constant, axiom, or value of some other type is not a
    /// sealed proposition. The target and dependency source bytes are staged
    /// unchanged and built before processing the generated witness theorem.
    #[allow(clippy::too_many_arguments)] // Keep the trusted source/reference binding explicit.
    pub fn verify_lean_witness_with_dependencies(
        &self,
        tool: &Toolchain,
        file_path: &str,
        proposition_name: &str,
        committed_source: &str,
        submission: &ProofBodySubmission,
        base_commit: &str,
        dependencies: &[(&str, &str)],
    ) -> Result<VerifiedWitness> {
        ensure!(
            tool.engine == Engine::Lean4,
            "sealed witnesses require native Lean"
        );
        ensure!(
            matches!(base_commit.len(), 40 | 64)
                && base_commit.bytes().all(|c| c.is_ascii_hexdigit()),
            "expected a full Git base commit hash"
        );
        let module = path_for(Engine::Lean4, file_path)?;
        let mut trusted = dependencies.to_vec();
        trusted.push((file_path, committed_source));
        validate_modules(Engine::Lean4, &trusted, Some(CHECKER_FILE))?;
        for &(relative, _) in &trusted {
            let module = path_for(Engine::Lean4, relative)?;
            ensure!(
                !["init", "lean", "std"].contains(&module.to_ascii_lowercase().as_str())
                    && !tool
                        .reads
                        .iter()
                        .any(|root| root.join(format!("{module}.olean")).is_file()),
                "sealed witness module shadows the trusted Lean installation: {relative:?}"
            );
        }
        let checker = checker_source(module, proposition_name, submission)?;
        let source = format!("{checker}{AUDIT}");
        ensure!(
            source.len() <= SOURCE_CAP,
            "witness checker exceeds source limit"
        );

        let _lease = self.resources.acquire(Lane::Heavy, Pressure::Red)?;
        let room = tempfile::tempdir()?;
        let output = room.path().join("output");
        fs::create_dir(&output)?;
        self.build_modules(tool, room.path(), &output, &trusted)
            .context("build sealed proposition and verification dependencies")?;
        stage_source(Engine::Lean4, room.path(), &output, CHECKER_FILE, &source)?;
        let diagnostics = self
            .check_with_output(
                tool,
                room.path(),
                &output,
                CHECKER_FILE,
                false,
                DiagnosticOutput::LeanJson,
            )
            .context("kernel-check sealed Lean witness")?;
        let axioms = t3::axiom_report_at(&diagnostics, CHECKER_FILE, &checker)?;
        let mut observed_axioms = lean_axioms(&axioms, WITNESS)?;
        observed_axioms.sort_unstable();
        let mut verified = VerifiedWitness {
            base_commit: base_commit.into(),
            file_path: file_path.into(),
            proposition_id: proposition_name.into(),
            source_sha256: digest(committed_source.as_bytes()),
            proof_body_sha256: digest(submission.proof_body.as_bytes()),
            replay_source_sha256: digest(source.as_bytes()),
            dependencies_sha256: dependencies_digest(dependencies),
            compiler_sha256: tool.binary_sha256.clone(),
            observed_axioms,
            issued_unix_seconds: SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
            witness_sha256: String::new(),
            source,
        };
        verified.witness_sha256 = verified.compute_identity()?;
        Ok(verified)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_only_the_reserved_witness_rhs() -> Result<()> {
        let rhs = ProofBodySubmission {
            target_id: WITNESS.into(),
            proof_body: "by\n  change ¬ (current = 0)\n  decide".into(),
        };
        let checker = checker_source("Target", "target", &rhs)?;
        assert!(checker.starts_with("import Lean\nimport «Target»\n"));
        assert!(checker.contains("Lean.Name.mkSimple \"target\""));
        assert!(checker.contains("theorem witness : ai4mathSealedRefutation :="));
        assert!(checker.contains("change ¬ (current = 0)"));
        assert!(checker_source("Target", "Outside.target", &rhs).is_err());
        assert!(checker_source("Target", WITNESS, &rhs).is_err());
        let wrong_rhs_id = ProofBodySubmission {
            target_id: "target".into(),
            ..rhs
        };
        assert!(checker_source("Target", "target", &wrong_rhs_id).is_err());
        Ok(())
    }
}
