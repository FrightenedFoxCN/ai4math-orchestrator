//! Claim-scoped advisory discovery input; never a proof receipt or T3 approval.
use super::{
    Connection, ManagedProofRecipe, OptionalExtension, Result, ReviewedLeanSpec, StateStore, Trust,
    reviewed_lean,
};
use rusqlite::params;
use serde_json::{Value, json};
use std::path::Path;

/// Current managed ledger evidence for one Lean Claim, backed by its latest T1
/// promotion and, if selected, a sealed T3 restoration of that same proof. The
/// caller must separately verify Git provenance and ordered dependency sources.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct T3DiscoveryBaseline {
    pub claim_id: String,
    pub repo: String,
    pub engine: String,
    pub recipe: ManagedProofRecipe,
    /// The current T1 or T3 receipt and its ordered dependency digest.
    pub receipt_sha256: String,
    pub dependencies_sha256: String,
    pub verified_at_commit: String,
    pub baseline_scope: &'static str,
}

impl StateStore {
    /// Select the latest managed T1 promotion or the sealed T3 restoration
    /// of that same promotion, only if it supplies this non-stale L5 Claim's
    /// current receipt. A generic T3 receipt never qualifies.
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
    // Promotion writes its own trust transition before PROMOTION_COMMITTED, so
    // a later transition invalidates a reattached T1 receipt.
    let row = db.query_row(
        "WITH latest AS (
             SELECT git_commit, base_commit, claim_id, receipt_id, event_seq
             FROM recorded_promotions WHERE claim_id=?1 ORDER BY event_seq DESC LIMIT 1
         )
         SELECT m.repo,c.engine,p.base_commit,p.git_commit,q.file_path,q.target_decl,q.proof_body,
                r.patch_sha256,r.receipt_sha256,r.raw_receipt_json,e.payload_json,c.receipt_id,p.event_seq
         FROM latest p
         JOIN claims c ON c.claim_id=p.claim_id AND c.receipt_id IS NOT NULL
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
           AND e.event_type='PROMOTION_COMMITTED' AND e.entity_type='claim' AND e.actor='broker'
           AND (c.receipt_id<>p.receipt_id OR NOT EXISTS (
               SELECT 1 FROM events later WHERE later.entity_type='claim'
                 AND later.entity_id=p.claim_id AND later.event_seq>p.event_seq
                 AND later.event_type IN ('CLAIM_TRUST_CHANGED','CLAIM_TAINTED')))",
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
                r.get::<_, String>(11)?,
                r.get::<_, i64>(12)?,
            ))
        },
    ).optional()?;
    let Some((
        repo,
        engine,
        recipe,
        receipt_sha256,
        raw_receipt,
        promotion_event,
        current_receipt,
        promotion_seq,
    )) = row
    else {
        return Ok(None);
    };
    let (Ok(raw), Ok(event)) = (
        serde_json::from_str::<Value>(&raw_receipt),
        serde_json::from_str::<Value>(&promotion_event),
    ) else {
        return Ok(None);
    };
    let Some(dependencies_sha256) = receipt_dependencies(
        &raw,
        &engine,
        "T1_PROMOTION",
        &recipe.base_commit,
        &recipe,
        &receipt_sha256,
    ) else {
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
        || !sha256(&recipe.source_sha256)
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
    match (&spec, comparison) {
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

    let (verified_at_commit, receipt_sha256, dependencies_sha256, baseline_scope) =
        if current_receipt == receipt_sha256 {
            (
                recipe.promoted_commit.clone(),
                receipt_sha256,
                dependencies_sha256.into(),
                "claim_t1",
            )
        } else {
            let Some(spec) = spec else {
                return Ok(None);
            };
            let Some((commit, dependencies)) =
                current_sealed_recheck(db, &recipe, &spec, promotion_seq, &current_receipt)?
            else {
                return Ok(None);
            };
            (commit, current_receipt, dependencies, "claim_t3")
        };
    Ok(Some(T3DiscoveryBaseline {
        claim_id: claim_id.into(),
        repo,
        engine,
        recipe,
        receipt_sha256,
        dependencies_sha256,
        verified_at_commit,
        baseline_scope,
    }))
}

fn current_sealed_recheck(
    db: &Connection,
    recipe: &ManagedProofRecipe,
    spec: &ReviewedLeanSpec,
    promotion_seq: i64,
    receipt_id: &str,
) -> Result<Option<(String, String)>> {
    // Select the latest local transition before checking its shape: a later
    // taint or unrelated restoration must not expose an old sealed recheck.
    let current = db
        .query_row(
            "WITH latest AS (
             SELECT event_seq,event_type,git_commit,payload_json FROM events
             WHERE entity_type='claim' AND entity_id=?1
               AND event_type IN ('CLAIM_TRUST_CHANGED','CLAIM_TAINTED')
             ORDER BY event_seq DESC LIMIT 1
         )
         SELECT e.event_seq,e.git_commit,e.payload_json,r.raw_receipt_json
         FROM latest e JOIN verification_receipts r ON r.receipt_id=?2
         WHERE e.event_type='CLAIM_TRUST_CHANGED' AND e.event_seq>?3
           AND r.engine='lean4' AND r.tier='T3_TAINT_REBUILD'
           AND r.receipt_id=r.receipt_sha256 AND r.base_commit=e.git_commit
           AND r.target_decl=?4 AND r.patch_sha256=?5
           AND r.allowlist_passed=1 AND r.external_checker_passed IS NOT 0",
            params![
                recipe.claim_id,
                receipt_id,
                promotion_seq,
                recipe.target_decl,
                recipe.source_sha256
            ],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            },
        )
        .optional()?;
    let Some((restoration_seq, reviewed_commit, payload, raw)) = current else {
        return Ok(None);
    };
    if reviewed_commit.len() != recipe.promoted_commit.len() || !lowercase_hex(&reviewed_commit) {
        return Ok(None);
    }
    let (Ok(payload), Ok(raw)) = (
        serde_json::from_str::<Value>(&payload),
        serde_json::from_str::<Value>(&raw),
    ) else {
        return Ok(None);
    };
    let Some(dependencies) = receipt_dependencies(
        &raw,
        "lean4",
        "T3_TAINT_REBUILD",
        &reviewed_commit,
        recipe,
        receipt_id,
    ) else {
        return Ok(None);
    };
    let Some(plan_hash) = payload["detail"]["lean_spec_recheck"]["plan_sha256"]
        .as_str()
        .filter(|s| sha256(s))
    else {
        return Ok(None);
    };
    let Some(recheck) = reviewed_lean::read_optional_table(db, "lean_spec_rechecks", |db| {
        reviewed_lean::read_recheck(db, &recipe.claim_id, plan_hash)
    })?
    else {
        return Ok(None);
    };
    if recheck.reviewed_commit != reviewed_commit
        || recheck.promoted_commit != recipe.promoted_commit
        || recheck.candidate_file != recipe.file_path
        || recheck.candidate_decl != recipe.target_decl
        || recheck.candidate_source_sha256 != recipe.source_sha256
        || recheck.candidate_receipt_sha256 != receipt_id
        || recheck.spec_file != spec.spec_file
        || recheck.spec_decl != spec.spec_decl
        || recheck.spec_prefix_sha256 != spec.spec_prefix_sha256
        || !sha256(&recheck.comparison_receipt_sha256)
        || payload
            != json!({
                "previous_local": Trust::L5,
                "local": Trust::L5,
                "was_stale": true,
                "detail": {"receipt_id": receipt_id, "lean_spec_recheck": recheck},
            })
    {
        return Ok(None);
    }
    let Some(plan) = super::read_t3_plan(db, plan_hash)? else {
        return Ok(None);
    };
    if plan.reviewed_commit != reviewed_commit
        || !(promotion_seq..restoration_seq).contains(&plan.preview_event_seq)
        || plan.base_commit.len() != reviewed_commit.len()
        || !lowercase_hex(&plan.base_commit)
        || plan.base_commit == reviewed_commit
        || !super::read_applied_t3_claims(db, &plan)?.contains(&recipe.claim_id)
    {
        return Ok(None);
    }
    Ok(Some((reviewed_commit, dependencies.into())))
}

fn receipt_dependencies<'a>(
    raw: &'a Value,
    engine: &str,
    tier: &str,
    base_commit: &str,
    recipe: &ManagedProofRecipe,
    receipt_sha256: &str,
) -> Option<&'a str> {
    if !sha256(receipt_sha256)
        || !matches_fields(
            raw,
            [
                ("engine", engine),
                ("tier", tier),
                ("base_commit", base_commit),
                ("target_id", &recipe.target_decl),
                ("file_path", &recipe.file_path),
                ("patch_sha256", &recipe.source_sha256),
                ("receipt_sha256", receipt_sha256),
            ],
        )
    {
        return None;
    }
    raw["dependencies_sha256"].as_str().filter(|s| sha256(s))
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
