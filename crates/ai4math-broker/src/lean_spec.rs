//! Broker-only native comparison of a candidate theorem with a reviewed Lean spec.
use super::{
    Broker, Engine, Lane, Pressure, ReceiptTier, Toolchain, VerificationMode, VerificationReceipt,
    VerifiedCandidate,
    breaker_probe::{
        TOTAL_TIMEOUT, build_committed_sources, command_timeout, identifier,
        reject_installation_shadows,
    },
    digest, lean_breaker_sources_sha256, path_for, sandbox, stage_source, validate_modules,
};
use ai4math_evidence::LeanComparisonEvidence;
use anyhow::{Context, Result, ensure};
use std::{fs, time::Instant};

const CHECKER_FILE: &str = "Ai4mathSealedLeanComparison.lean";
const CHECKER: &str = include_str!("lean_spec_audit.lean");
// Existing T1/T3 comparison identities remain anchored to the pre-sketch checker.
const LEGACY_CHECKER_SHA256: &str =
    "7fa3d500c0f97bf99a72d02ca9f483816a31bda1014ab2a573295db03295c667";
const SEAL: &str = "ai4math:sealed-lean-spec-comparison:v1
";

/// The broker replayed both Lean declarations through the native kernel and
/// checked definitional equality against the reviewed definition's value. This
/// is comparison evidence, not a promotion or a claim about other verifiers.
#[derive(Debug)]
#[must_use]
pub struct VerifiedLeanComparison {
    spec_file: String,
    spec_decl: String,
    spec_prefix_sha256: String,
    base_commit: String,
    candidate_file: String,
    candidate_decl: String,
    candidate_source_sha256: String,
    candidate_receipt_sha256: String,
    receipt_sha256: String,
}

impl VerifiedLeanComparison {
    pub fn spec_file(&self) -> &str {
        &self.spec_file
    }
    pub fn spec_decl(&self) -> &str {
        &self.spec_decl
    }
    pub fn spec_prefix_sha256(&self) -> &str {
        &self.spec_prefix_sha256
    }
    pub fn base_commit(&self) -> &str {
        &self.base_commit
    }
    pub fn candidate_file(&self) -> &str {
        &self.candidate_file
    }
    pub fn candidate_decl(&self) -> &str {
        &self.candidate_decl
    }
    pub fn candidate_source_sha256(&self) -> &str {
        &self.candidate_source_sha256
    }
    /// Binds this in-memory comparison to the exact successful candidate receipt.
    pub fn candidate_receipt_sha256(&self) -> &str {
        &self.candidate_receipt_sha256
    }
    /// Durable, tier-specific comparison identity, independent of candidate verification timing.
    pub fn receipt_sha256(&self) -> &str {
        &self.receipt_sha256
    }
}

// SAFETY: Production values are constructed only after both declarations replay
// through the native Lean kernel and the sealed comparison checker returns its exact seal.
unsafe impl LeanComparisonEvidence for VerifiedLeanComparison {
    fn spec_file(&self) -> &str {
        self.spec_file()
    }

    fn spec_decl(&self) -> &str {
        self.spec_decl()
    }

    fn spec_prefix_sha256(&self) -> &str {
        self.spec_prefix_sha256()
    }

    fn base_commit(&self) -> &str {
        self.base_commit()
    }

    fn candidate_file(&self) -> &str {
        self.candidate_file()
    }

    fn candidate_decl(&self) -> &str {
        self.candidate_decl()
    }

    fn candidate_source_sha256(&self) -> &str {
        self.candidate_source_sha256()
    }

    fn candidate_receipt_sha256(&self) -> &str {
        self.candidate_receipt_sha256()
    }

    fn receipt_sha256(&self) -> &str {
        self.receipt_sha256()
    }
}

fn comparison_digest(
    candidate: &VerificationReceipt,
    spec_file: &str,
    spec_decl: &str,
    spec_prefix_sha256: &str,
) -> Result<String> {
    let domain = match candidate.tier() {
        // The exact T1 preimage is already persisted; do not change it.
        ReceiptTier::T1Promotion => "ai4math:lean-spec-comparison:v1",
        ReceiptTier::T3TaintRebuild => "ai4math:lean-spec-comparison:t3:v1",
    };
    Ok(digest(&serde_json::to_vec(&(
        domain,
        candidate.base_commit(),
        candidate.file_path(),
        candidate.target_id(),
        candidate.patch_sha256(),
        candidate.dependencies_sha256(),
        &candidate.compiler_sha256,
        spec_file,
        spec_decl,
        spec_prefix_sha256,
        CHECKER_FILE,
        LEGACY_CHECKER_SHA256,
    ))?))
}

fn formal_sketch_comparison_digest(
    candidate: &VerificationReceipt,
    spec_file: &str,
    spec_decl: &str,
    spec_prefix_sha256: &str,
) -> Result<String> {
    Ok(digest(&serde_json::to_vec(&(
        "ai4math:lean-spec-comparison:formal-sketch:v1",
        candidate.base_commit(),
        candidate.file_path(),
        candidate.target_id(),
        candidate.patch_sha256(),
        candidate.dependencies_sha256(),
        &candidate.compiler_sha256,
        spec_file,
        spec_decl,
        spec_prefix_sha256,
        CHECKER_FILE,
        digest(CHECKER.as_bytes()),
    ))?))
}

fn check_candidate_binding(candidate: &VerifiedCandidate) -> Result<()> {
    let receipt = candidate.receipt();
    ensure!(
        matches!(
            (receipt.tier(), candidate.mode),
            (
                ReceiptTier::T1Promotion,
                VerificationMode::Legacy(ReceiptTier::T1Promotion)
            ) | (ReceiptTier::T3TaintRebuild, VerificationMode::AssessT3)
                | (ReceiptTier::T3TaintRebuild, VerificationMode::FormalSketch)
        ),
        "reviewed Lean comparison requires a T1 promotion or an assessed T3 recheck"
    );
    ensure!(
        receipt.patch_sha256() == digest(candidate.source().as_bytes()),
        "comparison source differs from the verified candidate's source snapshot"
    );
    Ok(())
}

fn require_seal(log: &str) -> Result<()> {
    ensure!(
        log == SEAL,
        "missing or inexact sealed Lean spec audit: {log:?}"
    );
    Ok(())
}

impl Broker {
    /// Compare the exact broker-verified theorem against a public, module-owned,
    /// safe monomorphic definition of type `Prop` in an earlier module. The
    /// candidate must come from T1 verification or an assessed native T3 recheck.
    /// The caller supplies the exact ordered predecessor sources of that check;
    /// later modules never participate in resolving the spec's own closure.
    /// This returns comparison evidence only, not a regenerated certificate.
    pub fn compare_lean_spec(
        &self,
        tool: &Toolchain,
        candidate: &VerifiedCandidate,
        spec_file: &str,
        spec_decl: &str,
        dependencies: &[(&str, &str)],
    ) -> Result<VerifiedLeanComparison> {
        self.compare_lean_spec_with_policy(
            tool,
            candidate,
            spec_file,
            spec_decl,
            dependencies,
            false,
        )
    }

    pub fn compare_formal_sketch_spec(
        &self,
        tool: &Toolchain,
        candidate: &VerifiedCandidate,
        spec_file: &str,
        spec_decl: &str,
        dependencies: &[(&str, &str)],
    ) -> Result<VerifiedLeanComparison> {
        self.compare_lean_spec_with_policy(
            tool,
            candidate,
            spec_file,
            spec_decl,
            dependencies,
            true,
        )
    }

    fn compare_lean_spec_with_policy(
        &self,
        tool: &Toolchain,
        candidate: &VerifiedCandidate,
        spec_file: &str,
        spec_decl: &str,
        dependencies: &[(&str, &str)],
        allow_sorry: bool,
    ) -> Result<VerifiedLeanComparison> {
        let receipt = candidate.receipt();
        ensure!(
            tool.engine == Engine::Lean4 && receipt.engine() == Engine::Lean4,
            "reviewed Lean comparison requires a native Lean candidate and compiler"
        );
        ensure!(
            receipt.compiler_sha256 == tool.binary_sha256,
            "comparison compiler differs from the verified candidate's compiler"
        );
        check_candidate_binding(candidate)?;
        let spec_module = path_for(Engine::Lean4, spec_file)?;
        let candidate_module = path_for(Engine::Lean4, receipt.file_path())?;
        ensure!(
            !spec_module.eq_ignore_ascii_case(candidate_module),
            "reviewed spec must be in a module distinct from the candidate"
        );
        ensure!(
            spec_decl.split('.').all(identifier) && receipt.target_id().split('.').all(identifier),
            "expected exact public Lean declaration names"
        );
        ensure!(dependencies.len() < super::MAX_MODULES, "too many modules");
        let mut sources = dependencies.to_vec();
        sources.push((receipt.file_path(), candidate.source()));
        validate_modules(Engine::Lean4, &sources, Some(CHECKER_FILE))?;
        reject_installation_shadows(tool, &sources)?;
        let prefix_len = dependencies
            .iter()
            .position(|&(path, _)| path == spec_file)
            .context("reviewed spec must be in the ordered candidate dependencies")?
            + 1;
        ensure!(
            lean_breaker_sources_sha256(dependencies) == receipt.dependencies_sha256(),
            "comparison dependencies differ from the verified candidate's source snapshot"
        );
        let spec_prefix_sha256 = lean_breaker_sources_sha256(&dependencies[..prefix_len]);

        let _lease = self.resources.acquire(Lane::Heavy, Pressure::Red)?;
        let deadline = Instant::now() + TOTAL_TIMEOUT;
        let build = tempfile::tempdir()?;
        let build_path = build.path().canonicalize()?;
        let prefix_objects = build_path.join("spec-prefix");
        let objects = build_path.join("objects");
        build_committed_sources(
            tool,
            &build_path,
            &sources,
            deadline,
            Some((prefix_len, &prefix_objects)),
        )?;

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
        // No LEAN_PATH during startup. The static checker alone imports the
        // newly compiled artifacts as data, under a separate data-only grant.
        // Only this run may supply the exact seal (including no extra stderr);
        // build-stage output cannot authorize a comparison.
        let log = sandbox::run_grouped(
            sandbox::Invocation {
                executable: &tool.executable,
                args: &[
                    "--run".into(),
                    CHECKER_FILE.into(),
                    if allow_sorry { "sketch" } else { "strict" }.into(),
                    prefix_objects
                        .to_str()
                        .context("non-UTF-8 spec object path")?
                        .into(),
                    objects
                        .to_str()
                        .context("non-UTF-8 comparison object path")?
                        .into(),
                    spec_module.into(),
                    spec_decl.into(),
                    candidate_module.into(),
                    receipt.target_id().into(),
                ],
                room: audit.path(),
                writable: &writable,
                reads: &tool.reads,
                env: &[],
                timeout: command_timeout(deadline)?,
            },
            &[prefix_objects, objects],
            &[],
            tool.sandbox_group,
        )
        .context("kernel-check sealed Lean spec comparison")?;
        command_timeout(deadline)?;
        require_seal(&log)?;

        let receipt_sha256 = if allow_sorry {
            formal_sketch_comparison_digest(receipt, spec_file, spec_decl, &spec_prefix_sha256)?
        } else {
            comparison_digest(receipt, spec_file, spec_decl, &spec_prefix_sha256)?
        };
        Ok(VerifiedLeanComparison {
            spec_file: spec_file.into(),
            spec_decl: spec_decl.into(),
            spec_prefix_sha256,
            base_commit: receipt.base_commit().into(),
            candidate_file: receipt.file_path().into(),
            candidate_decl: receipt.target_id().into(),
            candidate_source_sha256: receipt.patch_sha256().into(),
            candidate_receipt_sha256: receipt.receipt_sha256().into(),
            receipt_sha256,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::candidate_fixture;

    #[test]
    fn audit_seal_must_be_exact() {
        assert!(require_seal(SEAL).is_ok());
        for bad in [
            "",
            SEAL.trim_end(),
            &format!(
                "warning
{SEAL}"
            ),
            &format!("{SEAL}{SEAL}"),
        ] {
            assert!(require_seal(bad).is_err());
        }
    }

    #[test]
    fn candidate_binding_requires_native_tier_and_exact_source() -> Result<()> {
        let mut candidate = candidate_fixture(&[]);
        check_candidate_binding(&candidate)?;

        candidate.receipt.tier = ReceiptTier::T3TaintRebuild;
        assert!(check_candidate_binding(&candidate).is_err());
        candidate.mode = VerificationMode::Legacy(ReceiptTier::T3TaintRebuild);
        assert!(check_candidate_binding(&candidate).is_err());
        candidate.mode = VerificationMode::AssessT3;
        check_candidate_binding(&candidate)?;

        candidate.receipt.tier = ReceiptTier::T1Promotion;
        assert!(check_candidate_binding(&candidate).is_err());
        candidate.receipt.tier = ReceiptTier::T3TaintRebuild;
        candidate.source.push(' ');
        let error = check_candidate_binding(&candidate).unwrap_err();
        assert!(error.to_string().contains("source differs"));
        Ok(())
    }

    #[test]
    fn durable_identity_ignores_candidate_receipt_timing_but_binds_the_tier() -> Result<()> {
        let deps = [(
            "Spec.lean",
            "def promised : Prop := True
",
        )];
        let prefix = lean_breaker_sources_sha256(&deps);
        let mut candidate = candidate_fixture(&deps);
        let receipt = comparison_digest(candidate.receipt(), "Spec.lean", "promised", &prefix)?;
        // Computed with the pre-T3 v1 tuple; existing T1 comparison records still match.
        assert_eq!(
            receipt,
            "b7eee059b063eec4b4449a1383b322f3c89deffa9bbc86dbbb622715d0bc2350"
        );
        candidate.receipt.tier = ReceiptTier::T3TaintRebuild;
        let t3 = comparison_digest(candidate.receipt(), "Spec.lean", "promised", &prefix)?;
        assert_ne!(receipt, t3);
        candidate.receipt.tier = ReceiptTier::T1Promotion;
        candidate.receipt.issued_unix_seconds += 1;
        candidate.receipt.compile_seconds += 0.25;
        candidate.receipt.receipt_sha256 = "e".repeat(64);
        candidate.receipt.tier = ReceiptTier::T3TaintRebuild;
        assert_eq!(
            t3,
            comparison_digest(candidate.receipt(), "Spec.lean", "promised", &prefix)?
        );
        candidate.receipt.tier = ReceiptTier::T1Promotion;
        assert_eq!(
            receipt,
            comparison_digest(candidate.receipt(), "Spec.lean", "promised", &prefix)?
        );
        assert_ne!(
            receipt,
            comparison_digest(candidate.receipt(), "Spec.lean", "other", &prefix)?
        );
        assert_ne!(
            receipt,
            comparison_digest(
                candidate.receipt(),
                "Spec.lean",
                "promised",
                &"f".repeat(64)
            )?
        );
        candidate.receipt.patch_sha256 = "c".repeat(64);
        assert_ne!(
            receipt,
            comparison_digest(candidate.receipt(), "Spec.lean", "promised", &prefix)?
        );
        Ok(())
    }
}
