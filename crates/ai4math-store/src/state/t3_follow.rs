use super::{StateStore, reviewed_lean::read_optional_table};
use anyhow::{Context, Result, ensure};
use rusqlite::params;

const MAX_APPLIED_T3_PLANS_PER_HEAD: usize = 8;

fn lowercase_hex(value: &str) -> bool {
    value
        .bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

impl StateStore {
    /// List up to eight canonical applied plan hashes in lexicographic order for
    /// the exact reviewed Git commit, including completed sweeps. A ninth match
    /// is an error rather than a partial listing.
    ///
    /// This is advisory dispatch eligibility only; the sweep must still validate
    /// the applied approval and worklist. No ledger updates or migrations occur.
    pub fn applied_t3_hashes_for_head(&self, reviewed: &str) -> Result<Vec<String>> {
        ensure!(
            matches!(reviewed.len(), 40 | 64)
                && lowercase_hex(reviewed)
                && reviewed.bytes().any(|b| b != b'0'),
            "expected a full lowercase reviewed Git commit"
        );

        let hashes = read_optional_table(&self.db, "applied_t3_plans", |db| {
            // The reviewed_commit predicate can use either the current index or
            // the legacy unique index. Sort only after bounding the result.
            let mut stmt = db.prepare(
                "SELECT plan_sha256 FROM main.applied_t3_plans WHERE reviewed_commit=?1 LIMIT ?2",
            )?;
            let mut rows =
                stmt.query(params![reviewed, MAX_APPLIED_T3_PLANS_PER_HEAD as i64 + 1])?;
            let mut hashes = Vec::new();
            while let Some(row) = rows.next()? {
                ensure!(
                    hashes.len() < MAX_APPLIED_T3_PLANS_PER_HEAD,
                    "more than {MAX_APPLIED_T3_PLANS_PER_HEAD} applied T3 plans for reviewed HEAD"
                );
                let hash = row
                    .get_ref(0)?
                    .as_str()
                    .context("invalid applied T3 plan hash")?;
                ensure!(
                    hash.len() == 64 && lowercase_hex(hash),
                    "invalid applied T3 plan hash: expected 64 lowercase hexadecimal digits"
                );
                hashes.push(hash.to_owned());
            }
            hashes.sort_unstable();
            Ok(Some(hashes))
        })?;
        Ok(hashes.unwrap_or_default())
    }
}

#[cfg(test)]
mod tests;
