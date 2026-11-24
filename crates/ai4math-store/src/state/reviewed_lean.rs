//! Operator-approved statement identity and sealed Lean comparison evidence.
use super::*;
use ai4math_broker::VerifiedLeanComparison;
use ai4math_core::{Engine, validate_source_path};

/// An operator-approved exact Git snapshot, not proof evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReviewedLeanSpec {
    pub repo: String,
    pub claim_id: String,
    pub approved_git_commit: String,
    pub spec_file: String,
    pub spec_decl: String,
    pub spec_prefix_sha256: String,
}

/// Durable evidence for a recorded promotion. A readback cannot mint a receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LeanSpecComparison {
    pub promoted_commit: String,
    pub claim_id: String,
    pub base_commit: String,
    pub spec_file: String,
    pub spec_decl: String,
    pub spec_prefix_sha256: String,
    pub candidate_receipt_sha256: String,
    pub receipt_sha256: String,
}

/// Durable evidence that the same managed proof and reviewed spec were rechecked
/// at the commit approved by an applied operator T3 plan. This is readback only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LeanSpecRecheck {
    pub claim_id: String,
    pub plan_sha256: String,
    pub reviewed_commit: String,
    pub promoted_commit: String,
    pub spec_file: String,
    pub spec_decl: String,
    pub spec_prefix_sha256: String,
    pub candidate_file: String,
    pub candidate_decl: String,
    pub candidate_source_sha256: String,
    pub candidate_receipt_sha256: String,
    pub comparison_receipt_sha256: String,
}

/// Private projection constructed only from a sealed broker comparison.
#[derive(Debug)]
pub(super) struct ComparedSpec {
    pub(super) spec_file: String,
    pub(super) spec_decl: String,
    pub(super) spec_prefix_sha256: String,
    pub(super) candidate_receipt_sha256: String,
    pub(super) receipt_sha256: String,
}

fn lowercase_hex(s: &str) -> bool {
    s.bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn canonical_lean_file(file: &str) -> bool {
    // Same managed module spelling as the broker; the core validation also excludes
    // protected files such as lakefile.lean.
    validate_source_path(Engine::Lean4, file).is_ok()
        && file.strip_suffix(".lean").is_some_and(|module| {
            module.starts_with(|c: char| c.is_ascii_uppercase())
                && module
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_')
        })
}

fn canonical_lean_decl(name: &str) -> bool {
    !name.starts_with("_root_.")
        && name.split('.').all(|part| {
            part != "_"
                && part.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
                && part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'\''))
        })
}

pub(super) fn read_binding(db: &Connection, claim_id: &str) -> Result<Option<ReviewedLeanSpec>> {
    Ok(db
        .query_row(
            "SELECT repo, claim_id, approved_git_commit, spec_file, spec_decl, spec_prefix_sha256
         FROM reviewed_lean_specs WHERE claim_id=?1",
            [claim_id],
            |r| {
                Ok(ReviewedLeanSpec {
                    repo: r.get(0)?,
                    claim_id: r.get(1)?,
                    approved_git_commit: r.get(2)?,
                    spec_file: r.get(3)?,
                    spec_decl: r.get(4)?,
                    spec_prefix_sha256: r.get(5)?,
                })
            },
        )
        .optional()?)
}

// Claim IDs may alias one physical target. Compare module spellings with ASCII
// case folding (the managed filesystem may be case-insensitive), but declaration
// names must match exactly. Only canonically scoped bindings enter this table.
fn binding_owner(db: &Connection, file: &str, decl: &str) -> Result<Option<String>> {
    Ok(db
        .query_row(
            "SELECT b.claim_id FROM reviewed_lean_specs b JOIN claims c USING(claim_id)
         WHERE substr(c.formal_target,1,instr(c.formal_target,'#')-1)=?1 COLLATE NOCASE
           AND substr(c.formal_target,instr(c.formal_target,'#')+1)=?2 LIMIT 1",
            params![file, decl],
            |r| r.get(0),
        )
        .optional()?)
}

pub(super) fn read_comparison(
    db: &Connection,
    git_commit: &str,
) -> Result<Option<LeanSpecComparison>> {
    Ok(db
        .query_row(
            "SELECT p.git_commit, p.claim_id, p.base_commit, c.spec_file, c.spec_decl,
                c.spec_prefix_sha256, c.candidate_receipt_sha256, c.receipt_sha256
         FROM lean_spec_comparisons c JOIN recorded_promotions p USING(git_commit)
         WHERE c.git_commit=?1",
            [git_commit],
            |r| {
                Ok(LeanSpecComparison {
                    promoted_commit: r.get(0)?,
                    claim_id: r.get(1)?,
                    base_commit: r.get(2)?,
                    spec_file: r.get(3)?,
                    spec_decl: r.get(4)?,
                    spec_prefix_sha256: r.get(5)?,
                    candidate_receipt_sha256: r.get(6)?,
                    receipt_sha256: r.get(7)?,
                })
            },
        )
        .optional()?)
}

pub(super) fn has_recheck(db: &Connection, claim_id: &str, plan_sha256: &str) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM lean_spec_rechecks WHERE claim_id=?1 AND plan_sha256=?2)",
        params![claim_id, plan_sha256],
        |r| r.get(0),
    )?)
}

pub(super) fn read_recheck(
    db: &Connection,
    claim_id: &str,
    plan_sha256: &str,
) -> Result<Option<LeanSpecRecheck>> {
    Ok(db
        .query_row(
            "SELECT reviewed_commit,promoted_commit,spec_file,spec_decl,spec_prefix_sha256,
                candidate_file,candidate_decl,candidate_source_sha256,candidate_receipt_sha256,
                comparison_receipt_sha256
         FROM lean_spec_rechecks WHERE claim_id=?1 AND plan_sha256=?2",
            params![claim_id, plan_sha256],
            |r| {
                Ok(LeanSpecRecheck {
                    claim_id: claim_id.into(),
                    plan_sha256: plan_sha256.into(),
                    reviewed_commit: r.get(0)?,
                    promoted_commit: r.get(1)?,
                    spec_file: r.get(2)?,
                    spec_decl: r.get(3)?,
                    spec_prefix_sha256: r.get(4)?,
                    candidate_file: r.get(5)?,
                    candidate_decl: r.get(6)?,
                    candidate_source_sha256: r.get(7)?,
                    candidate_receipt_sha256: r.get(8)?,
                    comparison_receipt_sha256: r.get(9)?,
                })
            },
        )
        .optional()?)
}

// A read-only StateStore may be looking at a legacy database whose additive
// tables have not been installed. Never use this fallback on the write path.
fn read_optional_table<T>(
    db: &Connection,
    name: &str,
    read: impl FnOnce(&Connection) -> Result<Option<T>>,
) -> Result<Option<T>> {
    match read(db) {
        Err(_)
            if !db.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name=?1)",
                [name],
                |r| r.get::<_, bool>(0),
            )? =>
        {
            Ok(None)
        }
        result => result,
    }
}

impl StateStore {
    /// The trusted operator coordinator must establish the exact reviewed Git
    /// commit, spec prefix and declaration under its repository lock. The store
    /// checks ledger eligibility and immutability, not Git contents or ancestry.
    pub fn bind_lean_spec(&mut self, binding: &ReviewedLeanSpec) -> Result<()> {
        ensure!(
            Path::new(&binding.repo).is_absolute() && !binding.repo.chars().any(char::is_control),
            "expected a canonical managed repository path"
        );
        ensure!(
            matches!(binding.approved_git_commit.len(), 40 | 64)
                && lowercase_hex(&binding.approved_git_commit),
            "expected a full approved Git commit"
        );
        ensure!(
            canonical_lean_file(&binding.spec_file) && canonical_lean_decl(&binding.spec_decl),
            "expected a canonical Lean reviewed spec file and declaration"
        );
        ensure!(
            binding.spec_prefix_sha256.len() == 64 && lowercase_hex(&binding.spec_prefix_sha256),
            "expected a lowercase SHA-256 of the reviewed spec prefix"
        );
        self.write(|tx| {
            ensure!(read_intent(tx)?.is_none() && pivot::read_pending(tx)?.is_none(),
                "recover the pending Git transaction before binding a reviewed Lean spec");
            let c = require_claim(tx, &binding.claim_id)?;
            let (file, decl) = c.formal_target.as_deref().and_then(|s| s.split_once('#'))
                .context("expected a canonical Lean claim target file#declaration")?;
            ensure!(c.engine == "lean4" && canonical_lean_file(file) && canonical_lean_decl(decl)
                && c.module_path.as_deref().is_none_or(|path| path == file),
                "expected a canonical Lean claim target file#declaration");
            ensure!(!binding.spec_file.eq_ignore_ascii_case(file),
                "reviewed Lean spec file must differ from the claim module");
            tx.execute("INSERT OR IGNORE INTO managed_repo(singleton,repo) VALUES(1,?1)", [&binding.repo])?;
            let repo: String = tx.query_row("SELECT repo FROM managed_repo WHERE singleton=1", [], |r| r.get(0))?;
            ensure!(repo == binding.repo, "project is bound to a different Git repository");
            if let Some(old) = read_binding(tx, &binding.claim_id)? {
                ensure!(old == *binding, "reviewed Lean spec is immutable; register a new Claim with a new formal target");
                return Ok(());
            }
            ensure!(binding_owner(tx, file, decl)?.is_none(),
                "Lean target already has a reviewed spec binding on another Claim; register a new Claim with a new formal target");
            ensure!(
                !c.stale && !matches!(c.local_trust, Trust::L2b | Trust::L5) && c.receipt_id.is_none()
                    && !tx.query_row("SELECT EXISTS(SELECT 1 FROM recorded_promotions WHERE claim_id=?1)
                                     OR EXISTS(SELECT 1 FROM verified_refutations WHERE claim_id=?1)",
                        [&binding.claim_id], |r| r.get::<_,bool>(0))?,
                "only unpromoted, unrefuted and non-stale Claims can bind a reviewed Lean spec"
            );
            tx.execute("INSERT INTO reviewed_lean_specs
                        (claim_id,repo,approved_git_commit,spec_file,spec_decl,spec_prefix_sha256)
                        VALUES(?1,?2,?3,?4,?5,?6)", params![binding.claim_id,binding.repo,
                binding.approved_git_commit,binding.spec_file,binding.spec_decl,binding.spec_prefix_sha256])?;
            event(tx, Change {actor:"operator:lean-spec", git_commit:Some(&binding.approved_git_commit)},
                "LEAN_SPEC_BOUND", "claim", &binding.claim_id, json!({
                    "spec_file":binding.spec_file,"spec_decl":binding.spec_decl,
                    "spec_prefix_sha256":binding.spec_prefix_sha256,
                }))?;
            Ok(())
        })
    }

    pub fn lean_spec(&self, claim_id: &str) -> Result<Option<ReviewedLeanSpec>> {
        read_optional_table(&self.db, "reviewed_lean_specs", |db| {
            read_binding(db, claim_id)
        })
    }

    pub fn lean_spec_comparison(&self, git_commit: &str) -> Result<Option<LeanSpecComparison>> {
        read_optional_table(&self.db, "lean_spec_comparisons", |db| {
            read_comparison(db, git_commit)
        })
    }

    /// Includes Claim aliases for the same physical Lean module and declaration.
    pub fn lean_spec_target_owner(&self, file: &str, decl: &str) -> Result<Option<String>> {
        read_optional_table(&self.db, "reviewed_lean_specs", |db| {
            binding_owner(db, file, decl)
        })
    }

    /// Confirm that a retained comparator names the very T1 receipt joined to
    /// the historical recipe, rather than merely another row with the same spec.
    pub fn managed_lean_t1_matches_comparison(
        &self,
        recipe: &ManagedProofRecipe,
        comparison: &LeanSpecComparison,
    ) -> Result<bool> {
        if comparison.claim_id != recipe.claim_id
            || comparison.promoted_commit != recipe.promoted_commit
            || comparison.base_commit != recipe.base_commit
        {
            return Ok(false);
        }
        Ok(self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM recorded_promotions p JOIN verification_receipts r USING(receipt_id)
             WHERE p.git_commit=?1 AND p.claim_id=?2 AND p.base_commit=?3
             AND r.receipt_sha256=?4 AND r.tier='T1_PROMOTION' AND r.engine='lean4'
             AND r.base_commit=p.base_commit AND r.target_decl=?5
             AND json_extract(r.raw_receipt_json, '$.file_path')=?6 AND r.patch_sha256=?7)",
            params![recipe.promoted_commit,recipe.claim_id,recipe.base_commit,comparison.candidate_receipt_sha256,
                recipe.target_decl,recipe.file_path,recipe.source_sha256], |r| r.get(0),
        )?)
    }

    pub fn lean_spec_recheck(
        &self,
        claim_id: &str,
        plan_sha256: &str,
    ) -> Result<Option<LeanSpecRecheck>> {
        read_optional_table(&self.db, "lean_spec_rechecks", |db| {
            read_recheck(db, claim_id, plan_sha256)
        })
    }
}

impl VerifiedReceipt {
    /// Attach the sealed comparison from this exact Lean T1 candidate receipt.
    pub fn from_compared_promotion(
        candidate: &ai4math_broker::VerifiedCandidate,
        artifacts: &ai4math_broker::RegressionArtifacts,
        comparison: &VerifiedLeanComparison,
    ) -> Result<Self> {
        Self::from_promotion(candidate, artifacts)?.with_comparison(comparison, "T1_PROMOTION")
    }

    /// Attach the sealed comparison from this exact Lean T3 candidate receipt.
    pub fn from_compared_recheck(
        candidate: &ai4math_broker::VerifiedCandidate,
        comparison: &VerifiedLeanComparison,
    ) -> Result<Self> {
        Self::from_broker(candidate)?.with_comparison(comparison, "T3_TAINT_REBUILD")
    }

    fn with_comparison(mut self, comparison: &VerifiedLeanComparison, tier: &str) -> Result<Self> {
        ensure!(
            self.engine == "lean4"
                && self.tier == tier
                && comparison.base_commit() == self.base_commit
                && comparison.candidate_receipt_sha256() == self.receipt_sha256
                && comparison.candidate_file() == self.file_path
                && comparison.candidate_decl() == self.target_decl
                && comparison.candidate_source_sha256() == self.patch_sha256,
            "Lean comparison does not match this exact candidate receipt"
        );
        self.lean_comparison = Some(ComparedSpec {
            spec_file: comparison.spec_file().into(),
            spec_decl: comparison.spec_decl().into(),
            spec_prefix_sha256: comparison.spec_prefix_sha256().into(),
            candidate_receipt_sha256: comparison.candidate_receipt_sha256().into(),
            receipt_sha256: comparison.receipt_sha256().into(),
        });
        Ok(self)
    }
}

pub(super) fn check_comparison(
    db: &Connection,
    intent: &PromotionIntent,
    receipt: &VerifiedReceipt,
) -> Result<()> {
    let binding = read_binding(db, &intent.claim_id)?;
    match (binding, &receipt.lean_comparison) {
        (None, None) => {
            if receipt.engine == "lean4" {
                // Use the sealed candidate's target (not an unverified journal
                // target) so a legacy registration cannot bypass this gate.
                ensure!(
                    binding_owner(db, &receipt.file_path, &receipt.target_decl)?.is_none(),
                    "Lean target is bound to a reviewed spec on another Claim"
                );
            }
            Ok(())
        }
        (None, Some(_)) => anyhow::bail!("Lean comparison has no reviewed spec binding"),
        (Some(_), None) => anyhow::bail!("bound Lean claim requires a verified spec comparison"),
        (Some(spec), Some(comparison)) => {
            // The CLI establishes approved_git_commit ancestry while holding the
            // Git lock. Here we match the exact reviewed source prefix and proof.
            let repo: String =
                db.query_row("SELECT repo FROM managed_repo WHERE singleton=1", [], |r| {
                    r.get(0)
                })?;
            ensure!(
                spec.repo == repo && repo == intent.repo,
                "reviewed Lean spec repository mismatch"
            );
            ensure!(
                comparison.spec_file == spec.spec_file
                    && comparison.spec_decl == spec.spec_decl
                    && comparison.spec_prefix_sha256 == spec.spec_prefix_sha256,
                "verified Lean comparison does not match the reviewed spec binding"
            );
            ensure!(
                receipt.regression_checked
                    && receipt.engine == "lean4"
                    && receipt.tier == "T1_PROMOTION"
                    && receipt.base_commit == intent.base_commit
                    && receipt.file_path == intent.file_path
                    && receipt.target_decl == intent.target_decl
                    && comparison.candidate_receipt_sha256 == receipt.receipt_sha256,
                "verified Lean comparison does not match its promotion candidate"
            );
            Ok(())
        }
    }
}

/// Validate everything the store can establish without reopening the physical Git snapshot.
pub(super) fn validate_recheck(
    db: &Connection,
    claim: &ClaimState,
    spec: &ReviewedLeanSpec,
    plan: &T3PlanApproval,
    receipt: &VerifiedReceipt,
) -> Result<LeanSpecRecheck> {
    let comparison = receipt
        .lean_comparison
        .as_ref()
        .context("bound Lean claim requires a verified spec comparison for T3")?;
    ensure!(
        claim.stale
            && claim.prior_trust == Some(Trust::L5)
            && claim.taint_root_commit.as_deref() == Some(plan.reviewed_commit.as_str())
            && receipt.base_commit == plan.reviewed_commit
            && receipt.engine == "lean4"
            && receipt.tier == "T3_TAINT_REBUILD"
            && comparison.candidate_receipt_sha256 == receipt.receipt_sha256,
        "verified Lean comparison does not match the candidate and applied T3 taint snapshot"
    );
    ensure!(
        comparison.spec_file == spec.spec_file
            && comparison.spec_decl == spec.spec_decl
            && comparison.spec_prefix_sha256 == spec.spec_prefix_sha256,
        "verified Lean comparison does not match the reviewed spec binding"
    );
    let recipe = read_managed_proof_recipe(db, &claim.id, None)?
        .context("bound Lean claim lacks a latest managed proof recipe")?;
    let original = read_comparison(db, &recipe.promoted_commit)?
        .context("latest managed promotion is missing its retained Lean comparison")?;
    ensure!(
        original.claim_id == claim.id
            && original.base_commit == recipe.base_commit
            && original.spec_file == spec.spec_file
            && original.spec_decl == spec.spec_decl
            && original.spec_prefix_sha256 == spec.spec_prefix_sha256,
        "latest managed promotion comparison does not match the reviewed spec binding"
    );
    let original_is_t1: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM recorded_promotions p JOIN verification_receipts r USING(receipt_id)
         WHERE p.git_commit=?1 AND r.receipt_sha256=?2 AND r.tier='T1_PROMOTION' AND r.engine='lean4'
           AND r.base_commit=p.base_commit AND r.target_decl=?3)",
        params![recipe.promoted_commit, original.candidate_receipt_sha256, recipe.target_decl],
        |r| r.get(0),
    )?;
    ensure!(
        original_is_t1,
        "retained Lean comparison does not identify its managed T1 proof receipt"
    );
    // The patch hash is over the entire promoted source, including the frozen target and body.
    // The operator coordinator separately checks that exact blob at the reviewed Git commit.
    ensure!(
        receipt.file_path == recipe.file_path
            && receipt.target_decl == recipe.target_decl
            && receipt.patch_sha256 == recipe.source_sha256,
        "T3 receipt does not match the latest managed proof target and source"
    );
    Ok(LeanSpecRecheck {
        claim_id: claim.id.clone(),
        plan_sha256: plan.plan_sha256.clone(),
        reviewed_commit: plan.reviewed_commit.clone(),
        promoted_commit: recipe.promoted_commit,
        spec_file: comparison.spec_file.clone(),
        spec_decl: comparison.spec_decl.clone(),
        spec_prefix_sha256: comparison.spec_prefix_sha256.clone(),
        candidate_file: receipt.file_path.clone(),
        candidate_decl: receipt.target_decl.clone(),
        candidate_source_sha256: receipt.patch_sha256.clone(),
        candidate_receipt_sha256: comparison.candidate_receipt_sha256.clone(),
        comparison_receipt_sha256: comparison.receipt_sha256.clone(),
    })
}

pub(super) fn persist_recheck(tx: &Transaction<'_>, evidence: &LeanSpecRecheck) -> Result<()> {
    tx.execute(
        "INSERT INTO lean_spec_rechecks (claim_id,plan_sha256,reviewed_commit,promoted_commit,spec_file,
         spec_decl,spec_prefix_sha256,candidate_file,candidate_decl,candidate_source_sha256,
         candidate_receipt_sha256,comparison_receipt_sha256) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
        params![evidence.claim_id,evidence.plan_sha256,evidence.reviewed_commit,evidence.promoted_commit,
            evidence.spec_file,evidence.spec_decl,evidence.spec_prefix_sha256,evidence.candidate_file,
            evidence.candidate_decl,evidence.candidate_source_sha256,evidence.candidate_receipt_sha256,
            evidence.comparison_receipt_sha256],
    )?;
    Ok(())
}

pub(super) fn persist_comparison(
    tx: &Transaction<'_>,
    git_commit: &str,
    comparison: &ComparedSpec,
) -> Result<()> {
    tx.execute("INSERT INTO lean_spec_comparisons
                (git_commit,spec_file,spec_decl,spec_prefix_sha256,candidate_receipt_sha256,receipt_sha256)
                VALUES(?1,?2,?3,?4,?5,?6)", params![git_commit,comparison.spec_file,comparison.spec_decl,
        comparison.spec_prefix_sha256,comparison.candidate_receipt_sha256,comparison.receipt_sha256])?;
    Ok(())
}
