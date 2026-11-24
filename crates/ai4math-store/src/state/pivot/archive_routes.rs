//! Exact retained frontier selection and archive write, inside Pivot's shared transaction.
use super::*;
use crate::FrontierEdge;

#[cfg(test)]
mod tests;

/// The reviewed, complete edge set of one selected proof plan.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PivotArchivePlan {
    pub plan_id: String,
    pub target_claim_id: String,
    /// A plan with no dependencies is shown as one row with both dependency fields null.
    pub frontier_edges: Vec<FrontierEdge>,
}

pub(super) fn preview(
    db: &Connection,
    breaker: &BreakerProjection,
    breaker_sha256: &str,
    review: &RetainedReview,
    plan_ids: &[&str],
) -> Result<(Vec<PivotArchivePlan>, String)> {
    capture(
        db,
        breaker,
        breaker_sha256,
        plan_ids.iter().map(|&id| {
            // A globally truncated page may contain this plan in full. Neither its
            // truncation flag nor the presence of one row proves completeness.
            let mut shown: Vec<_> = review
                .frontier_edges
                .iter()
                .filter(|e| e.plan_id == id)
                .collect();
            shown.sort_unstable_by(|a, b| a.dep_claim_id.cmp(&b.dep_claim_id));
            (id, shown)
        }),
    )
}

pub(super) fn confirm(
    db: &Connection,
    breaker: &BreakerProjection,
    breaker_sha256: &str,
    plans: &[PivotArchivePlan],
) -> Result<String> {
    let (actual, sha256) = capture(
        db,
        breaker,
        breaker_sha256,
        plans
            .iter()
            .map(|plan| (plan.plan_id.as_str(), plan.frontier_edges.iter().collect())),
    )
    .context("selected Pivot route changed since review")?;
    ensure!(actual == plans, "selected Pivot route changed since review");
    Ok(sha256)
}

fn capture<'a>(
    db: &Connection,
    breaker: &BreakerProjection,
    breaker_sha256: &str,
    selection: impl IntoIterator<Item = (&'a str, Vec<&'a FrontierEdge>)>,
) -> Result<(Vec<PivotArchivePlan>, String)> {
    let formal_target = format!("{}#{}", breaker.module_path, breaker.target_definition_id);
    let mut route = db.prepare(
        "SELECT p.target_claim_id,
             json_array(p.name,p.track,p.status,p.effective_trust_level,p.rfc_id,p.created_at,p.updated_at,
                        c.local_trust_level,c.effective_trust_level,c.prior_trust_level,c.stale,c.effective_stale,
                        c.is_critical_path,c.taint_root_commit,c.receipt_id,c.updated_at,
                        (SELECT max(e.event_seq) FROM events e WHERE e.entity_type='proof_plan' AND e.entity_id=p.plan_id),
                        (SELECT max(e.event_seq) FROM events e WHERE e.entity_type='claim' AND e.entity_id=c.claim_id))
         FROM proof_plans p JOIN claims c ON c.claim_id=p.target_claim_id
         LEFT JOIN proof_plan_route_blocks b ON b.plan_id=p.plan_id
         WHERE p.plan_id=?1 AND p.status='ACTIVE' AND b.plan_id IS NULL
             AND c.engine=?2 AND c.module_path=?3 AND c.formal_target=?4
             AND c.stale=0 AND c.effective_stale=0
             AND c.local_trust_level NOT IN ('L2a','L2b','L5')
             AND c.effective_trust_level NOT IN ('L2a','L2b','L5')",
    )?;
    let mut edges = db.prepare(
        "SELECT p.target_claim_id,p.plan_id,p.status,d.dep_claim_id,d.dep_kind
         FROM proof_plans p LEFT JOIN plan_dependencies d USING(plan_id)
         WHERE p.plan_id=?1 ORDER BY d.dep_claim_id LIMIT ?2",
    )?;
    let mut plans = Vec::new();
    let mut private = Vec::new();
    for (id, shown) in selection {
        ensure!(
            !shown.is_empty(),
            "selected proof plan {id} is missing from the retained frontier"
        );
        let (claim, fingerprint) = route.query_row(
            params![id, breaker.engine, breaker.module_path, formal_target],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        ).optional()?.with_context(|| format!("selected proof plan {id} is not an active, unblocked route for the exact eligible breaker Claim"))?;
        ensure!(safe_text(&claim, 256), "invalid proof plan target Claim ID");
        // Read at most one row beyond the review to positively detect a partial page.
        let complete = edges
            .query_map(params![id, shown.len() + 1], |row| {
                Ok(FrontierEdge {
                    target_claim_id: row.get(0)?,
                    plan_id: row.get(1)?,
                    plan_status: row.get(2)?,
                    dep_claim_id: row.get(3)?,
                    dep_kind: row.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        ensure!(
            complete.iter().eq(shown),
            "retained frontier does not contain the exact complete edge set for proof plan {id}"
        );
        plans.push(PivotArchivePlan {
            plan_id: id.into(),
            target_claim_id: claim,
            frontier_edges: complete,
        });
        private.push(fingerprint);
    }
    // Route/Claim event heads detect changes followed by reversals, while the
    // exact current rows and edges also cover unjournaled projection drift.
    let sha256 = digest(&(breaker_sha256, &plans, private))?;
    Ok((plans, sha256))
}

pub(super) fn land(
    tx: &Transaction<'_>,
    change: Change<'_>,
    adr_id: &str,
    plans: &[PivotArchivePlan],
) -> Result<()> {
    for plan in plans {
        ensure!(tx.execute(
            &format!("UPDATE proof_plans SET status='ARCHIVED_BY_ADR',effective_trust_level='L0',updated_at={NOW}
              WHERE plan_id=?1 AND target_claim_id=?2 AND status='ACTIVE'"),
            params![plan.plan_id, plan.target_claim_id],
        )? == 1, "selected proof plan changed during archive");
        event(
            tx,
            change,
            "PLAN_ARCHIVED",
            "proof_plan",
            &plan.plan_id,
            json!({"target":plan.target_claim_id,"adr_id":adr_id}),
        )?;
    }
    super::super::recompute_without_route_alerts(tx, change)
}
