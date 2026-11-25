//! Disposable metadata projected only from a caller-validated Git/ledger view.
use super::{UNKNOWN_DATE, WikiStratum, sediment};
use crate::retained::valid_id as valid_sha256;
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use std::collections::BTreeMap;

const PREFIX: &str = "CORE-ADAPT-";
const MAX_CLAIM_ID: usize = 256;
const MAX_TITLE: usize = 512;
const MAX_GLOSS: usize = 2048;

/// Git HEAD and ledger event sequence that the caller bound its view to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WikiManagedGeneration<'a> {
    pub head: &'a str,
    pub event_seq: i64,
}

/// Validated adaptation metadata. The index does not authenticate proof evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WikiManagedCoreEntry {
    pub id: String,
    pub archive_id: String,
    /// The historical claim recorded by the canonical Sediment archive.
    pub origin_claim: String,
    /// The adaptation's destination claim, not necessarily the historical claim.
    pub claim_id: String,
    pub title: String,
    pub informal_gloss: String,
    pub verified_at_commit: String,
}

pub(super) fn reserved_id(id: &str) -> bool {
    id.get(..PREFIX.len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(PREFIX))
}

fn canonical_id(id: &str, prefix: &str) -> bool {
    id.strip_prefix(prefix).is_some_and(valid_sha256)
}

fn canonical_git_oid(oid: &str) -> bool {
    match oid.len() {
        40 => oid
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        64 => valid_sha256(oid),
        _ => false,
    }
}

impl WikiManagedGeneration<'_> {
    fn is_valid(self) -> bool {
        canonical_git_oid(self.head) && self.event_seq >= 0
    }
}

/// Capture the required archives from the same bytes that the rebuild indexes.
pub(super) struct Archives<'a>(BTreeMap<&'a str, Option<String>>);

impl<'a> Archives<'a> {
    pub(super) fn new(entries: &'a [WikiManagedCoreEntry]) -> Result<Self> {
        let mut needed = BTreeMap::new();
        for entry in entries {
            ensure!(
                canonical_id(&entry.id, PREFIX),
                "invalid managed Wiki Core ID {}; expected CORE-ADAPT- followed by 64 lowercase hex digits",
                entry.id
            );
            ensure!(
                canonical_id(&entry.archive_id, "SED-"),
                "invalid managed Wiki source archive ID {}",
                entry.archive_id
            );
            ensure!(
                [&entry.claim_id, &entry.origin_claim, &entry.title,]
                    .iter()
                    .all(|value| !value.trim().is_empty()),
                "blank managed Wiki Core metadata for {}",
                entry.id
            );
            ensure!(
                entry.claim_id.len() <= MAX_CLAIM_ID
                    && entry.origin_claim.len() <= MAX_CLAIM_ID
                    && entry.title.len() <= MAX_TITLE
                    && entry.informal_gloss.len() <= MAX_GLOSS,
                "oversized managed Wiki Core metadata for {}",
                entry.id
            );
            ensure!(
                !reserved_id(&entry.claim_id),
                "managed Wiki target claim cannot use the reserved Core ID namespace"
            );
            needed.insert(entry.archive_id.as_str(), None);
        }
        Ok(Self(needed))
    }

    pub(super) fn observe(&mut self, id: &str, stratum: WikiStratum, source: &str) -> Result<()> {
        if stratum == WikiStratum::Sediment
            && let Some(claim_id) = self.0.get_mut(id)
        {
            *claim_id = Some(sediment::parse_sediment(id, source)?.claim_id);
        }
        Ok(())
    }

    pub(super) fn validate(
        &self,
        generation: WikiManagedGeneration<'_>,
        entries: &[WikiManagedCoreEntry],
    ) -> Result<()> {
        ensure!(generation.is_valid(), "invalid managed Wiki generation");
        for entry in entries {
            ensure!(
                canonical_git_oid(&entry.verified_at_commit)
                    && entry.verified_at_commit.len() == generation.head.len(),
                "invalid verification commit for managed Wiki Core entry {}",
                entry.id
            );
            let origin = self
                .0
                .get(entry.archive_id.as_str())
                .and_then(Option::as_deref);
            ensure!(
                origin == Some(&entry.origin_claim),
                "managed Wiki Core entry {} requires indexed Sediment archive {} matching origin claim {}",
                entry.id,
                entry.archive_id,
                entry.origin_claim
            );
        }
        Ok(())
    }
}

pub(super) fn install(
    tx: &Transaction<'_>,
    generation: WikiManagedGeneration<'_>,
    entries: &[WikiManagedCoreEntry],
) -> Result<()> {
    for entry in entries {
        insert(tx, entry)
            .with_context(|| format!("project managed Wiki Core entry {}", entry.id))?;
    }
    tx.execute(
        "INSERT INTO managed_core_generation (singleton,head,event_seq) VALUES (1,?1,?2)",
        params![generation.head, generation.event_seq],
    )?;
    Ok(())
}

fn insert(tx: &Transaction<'_>, entry: &WikiManagedCoreEntry) -> Result<()> {
    tx.execute(
        "INSERT INTO entries (id,stratum,trust_level,engine,status,title,informal_gloss,
            verified_at_commit,origin_claim,created_at,updated_at,file_path)
        VALUES (?1,'core','L5','lean4','active',?2,?3,?4,?5,?6,?6,?7)",
        params![
            entry.id,
            entry.title,
            entry.informal_gloss,
            entry.verified_at_commit,
            entry.claim_id,
            UNKNOWN_DATE,
            format!(".managed-core/{}", entry.id)
        ],
    )?;
    tx.execute(
        "INSERT INTO managed_core_fts (entry_id,title,informal_gloss) VALUES (?1,?2,?3)",
        params![entry.id, entry.title, entry.informal_gloss],
    )?;
    tx.execute(
        "INSERT INTO entry_edges (src_id,dst_id,edge_type)
        VALUES (?1,?2,'born_from_claim'), (?3,?1,'superseded_by')",
        params![entry.id, entry.claim_id, entry.archive_id],
    )?;
    Ok(())
}

pub(super) fn installed_generation(db: &Connection) -> Result<Option<(String, i64)>> {
    if !generation_tables_exist(db)? {
        return Ok(None);
    }
    // Empty projections are a cheap static-only read: no Git subprocess is needed.
    let installed: Option<(String, i64)> = db
        .query_row(
            "SELECT head,event_seq FROM managed_core_generation WHERE singleton=1
         AND EXISTS (SELECT 1 FROM entries WHERE id GLOB 'CORE-ADAPT-*')",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    Ok(installed.filter(|(head, event_seq)| {
        WikiManagedGeneration {
            head,
            event_seq: *event_seq,
        }
        .is_valid()
    }))
}

fn generation_tables_exist(db: &Connection) -> Result<bool> {
    Ok(db.query_row(
        "SELECT count(*)=2 FROM main.sqlite_schema
        WHERE type='table' AND name IN ('managed_core_generation','managed_core_fts')",
        [],
        |row| row.get(0),
    )?)
}

pub(super) fn visible(
    db: &Connection,
    generation: Option<WikiManagedGeneration<'_>>,
) -> Result<bool> {
    let Some(generation) = generation.filter(|generation| generation.is_valid()) else {
        return Ok(false);
    };
    // A read-only handle may be using an index built before the additive schema.
    if !generation_tables_exist(db)? {
        return Ok(false);
    }
    Ok(db.query_row(
        "SELECT EXISTS (SELECT 1 FROM managed_core_generation WHERE singleton=1 AND head=?1 AND event_seq=?2)",
        params![generation.head, generation.event_seq],
        |row| row.get(0),
    )?)
}
