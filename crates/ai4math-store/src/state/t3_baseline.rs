//! Claim-scoped advisory discovery input; never a proof receipt or T3 approval.
use super::{Connection, ManagedProofRecipe, OptionalExtension, Result, StateStore, reviewed_lean};
use serde_json::Value;
use std::path::Path;

/// Current managed T1 ledger evidence for one Lean Claim. The caller must
/// separately verify the Git parent, tree and ordered dependency sources.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct T3DiscoveryBaseline {
    pub claim_id: String,
    pub repo: String,
    pub engine: String,
    pub recipe: ManagedProofRecipe,
    pub receipt_sha256: String,
    pub dependencies_sha256: String,
}

impl StateStore {
    /// Select only the latest managed T1 promotion if it is still the source
    /// of this non-stale L5 Claim's current receipt. A T3 receipt does not
    /// qualify, even if it may have verified the same managed proof again.
    pub fn t3_discovery_baseline(&self, claim_id: &str) -> Result<Option<T3DiscoveryBaseline>> {
        let snapshot = self.db.unchecked_transaction()?;
        let baseline =
            reviewed_lean::read_optional_table(&snapshot, "managed_proof_recipes", |db| {
                read_baseline(db, claim_id)
            })?;
        snapshot.commit()?;
        Ok(baseline)
    }
}

fn read_baseline(db: &Connection, claim_id: &str) -> Result<Option<T3DiscoveryBaseline>> {
    // Pick the latest promotion before filtering on receipt, recipe, or event
    // shape. A dangling or ineligible newer promotion must not expose an older one.
    let row = db.query_row(
        "WITH latest AS (
             SELECT git_commit, base_commit, claim_id, receipt_id, event_seq
             FROM recorded_promotions WHERE claim_id=?1 ORDER BY event_seq DESC LIMIT 1
         )
         SELECT m.repo,c.engine,p.base_commit,p.git_commit,q.file_path,q.target_decl,q.proof_body,
                r.patch_sha256,r.receipt_sha256,r.raw_receipt_json,e.payload_json
         FROM latest p
         JOIN claims c ON c.claim_id=p.claim_id AND c.receipt_id=p.receipt_id
         JOIN managed_repo m ON m.singleton=1
         JOIN managed_proof_recipes q USING(git_commit)
         JOIN verification_receipts r ON r.receipt_id=p.receipt_id
         JOIN events e ON e.event_seq=p.event_seq AND e.git_commit=p.git_commit AND e.entity_id=p.claim_id
         WHERE c.engine='lean4' AND c.local_trust_level='L5' AND c.effective_trust_level='L5'
           AND c.stale=0 AND c.effective_stale=0 AND c.module_path=q.file_path
           AND c.formal_target=(q.file_path || '#' || q.target_decl)
           AND r.engine=c.engine AND r.tier='T1_PROMOTION' AND r.base_commit=p.base_commit
           AND r.target_decl=q.target_decl AND r.receipt_id=r.receipt_sha256
           AND r.allowlist_passed=1 AND r.external_checker_passed IS NOT 0
           AND e.event_type='PROMOTION_COMMITTED' AND e.entity_type='claim' AND e.actor='broker'",
        [claim_id],
        |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                ManagedProofRecipe {
                    claim_id: claim_id.into(),
                    base_commit: r.get(2)?,
                    promoted_commit: r.get(3)?,
                    file_path: r.get(4)?,
                    target_decl: r.get(5)?,
                    proof_body: r.get(6)?,
                    source_sha256: r.get(7)?,
                },
                r.get::<_, String>(8)?,
                r.get::<_, String>(9)?,
                r.get::<_, String>(10)?,
            ))
        },
    ).optional()?;
    let Some((repo, engine, recipe, receipt_sha256, raw_receipt, promotion_event)) = row else {
        return Ok(None);
    };
    let (Ok(raw), Ok(event)) = (
        serde_json::from_str::<Value>(&raw_receipt),
        serde_json::from_str::<Value>(&promotion_event),
    ) else {
        return Ok(None);
    };
    let Some(dependencies_sha256) = raw["dependencies_sha256"].as_str().filter(|s| sha256(s))
    else {
        return Ok(None);
    };
    if !Path::new(&repo).is_absolute()
        || recipe.file_path.is_empty()
        || recipe.target_decl.is_empty()
        || !matches!(recipe.base_commit.len(), 40 | 64)
        || recipe.base_commit.len() != recipe.promoted_commit.len()
        || recipe.base_commit == recipe.promoted_commit
        || !lowercase_hex(&recipe.base_commit)
        || !lowercase_hex(&recipe.promoted_commit)
        || !sha256(&receipt_sha256)
        || !sha256(&recipe.source_sha256)
        || !matches_fields(
            &raw,
            [
                ("engine", &engine),
                ("tier", "T1_PROMOTION"),
                ("base_commit", &recipe.base_commit),
                ("target_id", &recipe.target_decl),
                ("file_path", &recipe.file_path),
                ("patch_sha256", &recipe.source_sha256),
                ("receipt_sha256", &receipt_sha256),
            ],
        )
        || !matches_fields(
            &event,
            [
                ("base_commit", &recipe.base_commit),
                ("receipt_id", &receipt_sha256),
                ("file_path", &recipe.file_path),
                ("target_decl", &recipe.target_decl),
                ("patch_sha256", &recipe.source_sha256),
            ],
        )
    {
        return Ok(None);
    }

    let spec = reviewed_lean::read_optional_table(db, "reviewed_lean_specs", |db| {
        reviewed_lean::read_binding(db, claim_id)
    })?;
    let comparison = reviewed_lean::read_optional_table(db, "lean_spec_comparisons", |db| {
        reviewed_lean::read_comparison(db, &recipe.promoted_commit)
    })?;
    match (spec, comparison) {
        (Some(spec), Some(comparison)) => {
            if spec.repo != repo
                || comparison.spec_file != spec.spec_file
                || comparison.spec_decl != spec.spec_decl
                || comparison.spec_prefix_sha256 != spec.spec_prefix_sha256
                || comparison.candidate_receipt_sha256 != receipt_sha256
                || !matches_fields(
                    &event,
                    [(
                        "comparison_receipt_sha256",
                        comparison.receipt_sha256.as_str(),
                    )],
                )
                || !reviewed_lean::matches_managed_t1(db, &recipe, Some(&comparison), None)?
            {
                return Ok(None);
            }
        }
        (None, None) if event.get("comparison_receipt_sha256").is_none() => {
            // A legacy Claim alias may point at the same reviewed Lean target.
            if reviewed_lean::read_optional_table(db, "reviewed_lean_specs", |db| {
                reviewed_lean::binding_owner(db, &recipe.file_path, &recipe.target_decl)
            })?
            .is_some()
            {
                return Ok(None);
            }
        }
        _ => return Ok(None),
    }

    Ok(Some(T3DiscoveryBaseline {
        claim_id: claim_id.into(),
        repo,
        engine,
        recipe,
        receipt_sha256,
        dependencies_sha256: dependencies_sha256.into(),
    }))
}

fn matches_fields<const N: usize>(json: &Value, fields: [(&str, &str); N]) -> bool {
    fields
        .into_iter()
        .all(|(name, expected)| json[name].as_str() == Some(expected))
}

fn lowercase_hex(s: &str) -> bool {
    s.bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn sha256(s: &str) -> bool {
    s.len() == 64 && lowercase_hex(s)
}

#[cfg(test)]
mod tests;
