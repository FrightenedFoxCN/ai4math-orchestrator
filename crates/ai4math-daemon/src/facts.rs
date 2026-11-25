#[path = "facts_retention.rs"]
mod retention;
#[cfg(test)]
#[path = "facts_tests.rs"]
mod tests;

use crate::transport::MAX_FRAME_BYTES;
pub use ai4math_store::retained::{read_retained, valid_id};
use ai4math_store::{
    FactBody, Page,
    retained::{MAX_PACK_BYTES, compact_len, hash_fact},
};
use anyhow::{Result, ensure};
use serde::Serialize;
use std::path::Path;

// The shared compact budget must leave room for the JSON-RPC envelope.
const _: () = assert!(MAX_PACK_BYTES == MAX_FRAME_BYTES * 3 / 4);

#[derive(Serialize)]
pub struct FactPack {
    #[serde(flatten)]
    body: FactBody,
    pub fact_sha256: String,
}
impl FactPack {
    pub fn compile(mut body: FactBody) -> Result<Self> {
        // Independent budgets keep verbose incidental sections from starving a
        // milestone, refutation, or inbox card. Byte limits omit whole entries,
        // never shorten IDs or break bilingual statement/signature pairs.
        body.events.items.reverse();
        bound_page(&mut body.events, 96 * 1024)?;
        body.events.items.reverse();
        bound_page(&mut body.frontier.claims, 128 * 1024)?;
        let frontier_ids: std::collections::BTreeSet<_> = body
            .frontier
            .claims
            .items
            .iter()
            .map(|c| &c.claim_id)
            .collect();
        let plans_before = body.frontier.plans.items.len();
        body.frontier
            .plans
            .items
            .retain(|edge| frontier_ids.contains(&edge.target_claim_id));
        body.frontier.plans.truncated |= plans_before != body.frontier.plans.items.len();
        bound_page(&mut body.frontier.plans, 64 * 1024)?;
        bound_page(&mut body.milestones, 64 * 1024)?;
        bound_page(&mut body.refutations, 64 * 1024)?;
        bound_page(&mut body.risk_radar.events, 64 * 1024)?;
        bound_page(&mut body.breakers, 48 * 1024)?;
        bound_page(&mut body.inbox, 32 * 1024)?;
        // Do not trim alert_sources: every dispatched alert may refer to this
        // same pack. The store bounds the matched batch and each event shape.
        ensure!(
            compact_len(&body, MAX_PACK_BYTES)?.is_some(),
            "fact body exceeds compact byte budget"
        );
        let fact_sha256 = hash_fact(&serde_json::to_value(&body)?)?;
        let pack = Self { body, fact_sha256 };
        ensure!(
            compact_len(&pack, MAX_PACK_BYTES)?.is_some(),
            "fact pack exceeds compact byte budget"
        );
        Ok(pack)
    }
    pub fn to_seq(&self) -> i64 {
        self.body.event_to_inclusive
    }
    pub fn retain(&self, dir: &Path) -> Result<()> {
        ensure!(valid_id(&self.fact_sha256), "invalid fact ID");
        let md = format!(
            "# 复盘事实包 / Briefing facts\n\n- Fact hash: {}
- Event interval: ({}, {}]\n- Stale claims: {}\n- Pending decisions shown: {}\n",
            self.fact_sha256,
            self.body.event_from_exclusive,
            self.body.event_to_inclusive,
            self.body.stale_claims,
            self.body.inbox.items.len()
        );
        // This summary is intentionally plain; UI renders authentic statement/signature pairs from JSON.
        retention::retain(dir, self, md.as_bytes())
    }
}

fn bound_page<T: Serialize>(page: &mut Page<T>, limit: usize) -> Result<()> {
    let mut remaining = limit.saturating_sub(2); // []
    let mut retained = Vec::with_capacity(page.items.len());
    for item in std::mem::take(&mut page.items) {
        let separator = usize::from(!retained.is_empty());
        if let Some(bytes) = compact_len(&item, remaining.saturating_sub(separator))? {
            remaining -= bytes + separator;
            retained.push(item);
        } else {
            page.truncated = true;
        }
    }
    page.items = retained;
    Ok(())
}
