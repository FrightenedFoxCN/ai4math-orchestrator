//! Read-only potential route analysis for one operator-registered breaker's transaction.
use super::*;

#[cfg(test)]
mod tests;

pub(super) struct Impact {
    pub(super) first_claim: String,
    pub(super) claim_count: usize,
}

pub(super) fn newly_blocked(
    tx: &Transaction<'_>,
    engine: &str,
    module: &str,
    target: &str,
) -> Result<Option<Impact>> {
    let formal_target = format!("{module}#{target}");
    let watched = tx
        .prepare(
            "SELECT claim_id FROM claims WHERE engine=?1 AND module_path=?2 AND formal_target=?3",
        )?
        .query_map(params![engine, module, formal_target], |r| {
            r.get::<_, String>(0)
        })?
        .collect::<rusqlite::Result<BTreeSet<_>>>()?;
    if watched.is_empty() {
        return Ok(None);
    }
    let prior = tx
        .prepare(
            "SELECT DISTINCT c.claim_id FROM circuit_breakers b JOIN claims c
            ON c.engine=b.engine AND c.module_path=b.module_path
            AND c.formal_target=(b.module_path || '#' || b.target_definition_id)
            WHERE b.state<>'CLOSED'",
        )?
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<BTreeSet<_>>>()?;
    let module_already_gated: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM circuit_breakers WHERE engine=?1 AND module_path=?2 AND state<>'CLOSED')",
        params![engine, module],
        |r| r.get(0),
    )?;
    let claims = all_claims(tx)?;
    let mut plans = tx.prepare(
        "SELECT p.plan_id,
        p.status IN ('ACTIVE','PROVED','SUSPENDED_SOFT_PRUNE') AND b.plan_id IS NULL
        FROM proof_plans p LEFT JOIN proof_plan_route_blocks b USING(plan_id)
        WHERE p.target_claim_id=?1",
    )?;
    let mut deps = tx.prepare("SELECT dep_claim_id FROM plan_dependencies WHERE plan_id=?1")?;
    let mut potential = BTreeMap::<String, (bool, bool)>::new();
    let mut impacted = None::<Impact>;
    // One finite topological pass compares two snapshots differing only in the
    // exact watched claim(s). Physical module fencing is not a graph edge.
    for id in graph_order(tx)? {
        let c = &claims[&id];
        let is_watched = watched.contains(&id);
        let (mut before, mut after) = (false, false);
        let current_verified = c.effective_trust == Trust::L5 && !c.stale && !c.effective_stale;
        if c.local_trust == Trust::L2b {
            // A recorded kernel refutation is not a potential dependency.
        } else if current_verified {
            // A scheduler freeze does not invalidate an already available proof.
            (before, after) = (true, true);
        } else if !prior.contains(&id) {
            let mut has_recorded_plan = false;
            for row in plans.query_map([&id], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, bool>(1)?))
            })? {
                let (plan, eligible) = row?;
                has_recorded_plan = true;
                if !eligible {
                    continue;
                }
                let (mut branch_before, mut branch_after) = (true, true);
                for dep in deps.query_map([plan], |r| r.get::<_, String>(0))? {
                    let (dep_before, dep_after) = potential[&dep?];
                    branch_before &= dep_before;
                    branch_after &= dep_after;
                    if !branch_before {
                        break;
                    }
                }
                before |= branch_before;
                after |= branch_after;
                if after {
                    break;
                }
            }
            // No plans is an unknown/leaf hypothesis, not exhaustion. Stale
            // and soft-pruned work is still potentially recoverable.
            before |= !has_recorded_plan;
            after = (after || !has_recorded_plan) && !is_watched;
        }
        if before && !after && c.critical && !c.stale && !c.effective_stale
            && !matches!(c.effective_trust, Trust::L5 | Trust::L2b)
            // The direct seed additionally requires a new physical gate. A
            // different exact declaration in that module may still cut a new
            // logical route to a downstream critical claim.
            && !(is_watched && module_already_gated)
        {
            match &mut impacted {
                Some(impact) => {
                    impact.claim_count += 1;
                    if id < impact.first_claim {
                        impact.first_claim.clone_from(&id);
                    }
                }
                None => {
                    impacted = Some(Impact {
                        first_claim: id.clone(),
                        claim_count: 1,
                    })
                }
            }
        }
        potential.insert(id, (before, after));
    }
    Ok(impacted)
}
