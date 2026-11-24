#[cfg(test)]
#[path = "facts_tests.rs"]
mod tests;

use crate::transport::MAX_FRAME_BYTES;
#[cfg(test)]
use ai4math_store::retained::MAX_LEGACY_FILE_BYTES;
pub use ai4math_store::retained::{read_retained, valid_id};
use ai4math_store::{
    FactBody, Page,
    retained::{MAX_PACK_BYTES, compact_len, hash_fact, matches_retained_file},
};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
#[cfg(test)]
use std::fs;
use std::{io::Write, path::Path};

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
        let bytes = serde_json::to_vec(self)?;
        ensure!(
            bytes.len() <= MAX_PACK_BYTES,
            "fact pack exceeds compact byte budget"
        );
        write_once(dir, &format!("{}.json", self.fact_sha256), &bytes)?;
        write_once(dir, &format!("{}.md", self.fact_sha256), md.as_bytes())?;
        Ok(())
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

fn write_once(dir: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    let mut staged = tempfile::NamedTempFile::new_in(dir)?;
    staged.write_all(bytes)?;
    staged.as_file().sync_all()?;
    let final_path = dir.join(name);
    match staged.persist_noclobber(&final_path) {
        Ok(_) => std::fs::File::open(dir)?
            .sync_all()
            .context("sync retained fact pack directory"),
        Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => {
            ensure!(
                matches_retained_file(&final_path, bytes)?,
                "retained artifact mismatch: {name}"
            );
            Ok(())
        }
        Err(e) => Err(e.error.into()),
    }
}
