//! Pluggable semantic vector retrieval abstractions (`EmbeddingProvider` &
//! `SemanticIndex`), zero-dependency `Noop` and `InMemoryCosineIndex` defaults,
//! SQLite content-hash vector caching, and Reciprocal Rank Fusion (RRF).

use anyhow::{Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

/// Pluggable text embedding generator (offline default: `NoopEmbeddingProvider`).
pub trait EmbeddingProvider: Send + Sync {
    fn model_id(&self) -> &str;
    fn dim(&self) -> usize;
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>>;

    fn embed_one(&self, text: &str) -> Result<Vec<f32>> {
        let mut out = self.embed_batch(&[text])?;
        ensure!(
            out.len() == 1,
            "embedding provider returned wrong batch size"
        );
        Ok(out.pop().expect("len == 1"))
    }
}

/// Pluggable dense vector similarity index (compatible with in-memory cosine,
/// `sqlite-vec`, or external vector engines).
pub trait SemanticIndex: Send + Sync {
    fn upsert(&mut self, entry_id: &str, content_sha256: &str, vector: &[f32]) -> Result<()>;
    fn remove_missing(&mut self, live_ids: &BTreeSet<String>) -> Result<()>;
    fn search(
        &self,
        query_vector: &[f32],
        allowed_ids: Option<&BTreeSet<String>>,
        top_k: usize,
    ) -> Result<Vec<(String, f64)>>;
}

/// Zero-dependency disabled embedding provider (preserves pure offline FTS5+Facet search).
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopEmbeddingProvider;

impl EmbeddingProvider for NoopEmbeddingProvider {
    fn model_id(&self) -> &str {
        "noop"
    }
    fn dim(&self) -> usize {
        0
    }
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        Ok(vec![Vec::new(); texts.len()])
    }
}

/// Zero-dependency disabled semantic index.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopSemanticIndex;

impl SemanticIndex for NoopSemanticIndex {
    fn upsert(&mut self, _entry_id: &str, _content_sha256: &str, _vector: &[f32]) -> Result<()> {
        Ok(())
    }
    fn remove_missing(&mut self, _live_ids: &BTreeSet<String>) -> Result<()> {
        Ok(())
    }
    fn search(
        &self,
        _query_vector: &[f32],
        _allowed_ids: Option<&BTreeSet<String>>,
        _top_k: usize,
    ) -> Result<Vec<(String, f64)>> {
        Ok(Vec::new())
    }
}

fn normalize_l2(vector: &[f32]) -> Result<Vec<f32>> {
    ensure!(!vector.is_empty(), "embedding vector cannot be empty");
    let mut norm_sq = 0.0f64;
    for &v in vector {
        ensure!(v.is_finite(), "embedding vector contains non-finite float");
        norm_sq += f64::from(v) * f64::from(v);
    }
    let norm = norm_sq.sqrt();
    if norm <= 1e-12 {
        return Ok(vec![0.0; vector.len()]);
    }
    Ok(vector
        .iter()
        .map(|&v| (f64::from(v) / norm) as f32)
        .collect())
}

/// Pure-Rust normalized cosine similarity index with deterministic tie-breaking.
#[derive(Debug, Clone)]
pub struct InMemoryCosineIndex {
    dim: usize,
    entries: BTreeMap<String, (String, Vec<f32>)>,
}

impl InMemoryCosineIndex {
    pub fn new(dim: usize) -> Result<Self> {
        ensure!(dim >= 1, "cosine index dimension must be >= 1");
        Ok(Self {
            dim,
            entries: BTreeMap::new(),
        })
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl SemanticIndex for InMemoryCosineIndex {
    fn upsert(&mut self, entry_id: &str, content_sha256: &str, vector: &[f32]) -> Result<()> {
        ensure!(
            vector.len() == self.dim,
            "vector dimension mismatch: expected {}, got {}",
            self.dim,
            vector.len()
        );
        let unit = normalize_l2(vector)?;
        self.entries
            .insert(entry_id.to_owned(), (content_sha256.to_owned(), unit));
        Ok(())
    }

    fn remove_missing(&mut self, live_ids: &BTreeSet<String>) -> Result<()> {
        self.entries.retain(|id, _| live_ids.contains(id));
        Ok(())
    }

    fn search(
        &self,
        query_vector: &[f32],
        allowed_ids: Option<&BTreeSet<String>>,
        top_k: usize,
    ) -> Result<Vec<(String, f64)>> {
        if top_k == 0 || self.entries.is_empty() {
            return Ok(Vec::new());
        }
        ensure!(
            query_vector.len() == self.dim,
            "query vector dimension mismatch: expected {}, got {}",
            self.dim,
            query_vector.len()
        );
        let q = normalize_l2(query_vector)?;
        let mut scored = Vec::new();
        for (id, (_, vec)) in &self.entries {
            if let Some(allowed) = allowed_ids
                && !allowed.contains(id)
            {
                continue;
            }
            let dot: f64 = q
                .iter()
                .zip(vec.iter())
                .map(|(&a, &b)| f64::from(a) * f64::from(b))
                .sum();
            scored.push((id.clone(), dot.clamp(-1.0, 1.0)));
        }
        scored.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        scored.truncate(top_k);
        Ok(scored)
    }
}

/// Statistics returned by `Wiki::sync_semantic_index`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct SemanticSyncStats {
    pub total_entries: usize,
    pub cache_hits: usize,
    pub embedded_fresh: usize,
}

pub(super) fn encode_f32_le(vector: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(vector.len() * 4);
    for &v in vector {
        bytes.extend_from_slice(&v.to_le_bytes());
    }
    bytes
}

pub(super) fn decode_f32_le(bytes: &[u8], expected_dim: usize) -> Result<Vec<f32>> {
    ensure!(
        bytes.len() == expected_dim * 4,
        "cached vector blob length does not match expected dimension"
    );
    let (chunks, rem) = bytes.as_chunks::<4>();
    ensure!(rem.is_empty(), "cached vector blob is not 4-byte aligned");
    let mut out = Vec::with_capacity(expected_dim);
    for &chunk in chunks {
        let v = f32::from_le_bytes(chunk);
        ensure!(
            v.is_finite(),
            "cached vector blob contains non-finite float"
        );
        out.push(v);
    }
    Ok(out)
}

pub(super) fn load_cached_vector(
    db: &Connection,
    entry_id: &str,
    model_id: &str,
    dim: usize,
    content_sha256: &str,
) -> Result<Option<Vec<f32>>> {
    let row: Option<(i64, String, Vec<u8>)> = db
        .query_row(
            "SELECT dim, content_sha256, vector_blob
             FROM entry_vectors WHERE entry_id=?1 AND model_id=?2",
            params![entry_id, model_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((cached_dim, cached_sha, blob)) = row else {
        return Ok(None);
    };
    if cached_dim as usize != dim || cached_sha != content_sha256 {
        return Ok(None);
    }
    Ok(Some(decode_f32_le(&blob, dim)?))
}

pub(super) fn save_cached_vector(
    db: &Connection,
    entry_id: &str,
    model_id: &str,
    dim: usize,
    content_sha256: &str,
    vector: &[f32],
) -> Result<()> {
    let blob = encode_f32_le(vector);
    db.execute(
        "INSERT INTO entry_vectors(entry_id, model_id, dim, content_sha256, vector_blob)
         VALUES(?1,?2,?3,?4,?5)
         ON CONFLICT(entry_id, model_id) DO UPDATE SET
           dim=excluded.dim,
           content_sha256=excluded.content_sha256,
           vector_blob=excluded.vector_blob",
        params![
            entry_id,
            model_id,
            i64::try_from(dim)?,
            content_sha256,
            blob
        ],
    )?;
    Ok(())
}

/// Reciprocal Rank Fusion (RRF) combining arbitrary ranked ID lists cleanly and
/// deterministically: `score(id) = sum_i 1 / (k + rank_i)` (1-based rank).
pub fn rrf_fuse(ranked_lists: &[&[String]], k: f64) -> Vec<(String, f64)> {
    let k = if k.is_finite() && k > 0.0 { k } else { 60.0 };
    let mut scores: BTreeMap<String, f64> = BTreeMap::new();
    for list in ranked_lists {
        for (rank_zero, id) in list.iter().enumerate() {
            let contrib = 1.0 / (k + (rank_zero as f64) + 1.0);
            *scores.entry(id.clone()).or_insert(0.0) += contrib;
        }
    }
    let mut out: Vec<(String, f64)> = scores.into_iter().collect();
    out.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cosine_index_and_rrf_rank_deterministically() -> Result<()> {
        let mut idx = InMemoryCosineIndex::new(3)?;
        idx.upsert("A", "sha_a", &[1.0, 0.0, 0.0])?;
        idx.upsert("B", "sha_b", &[0.8, 0.6, 0.0])?;
        idx.upsert("C", "sha_c", &[0.0, 1.0, 0.0])?;
        let hits = idx.search(&[1.0, 0.0, 0.0], None, 2)?;
        assert_eq!(hits[0].0, "A");
        assert_eq!(hits[1].0, "B");

        let fused = rrf_fuse(
            &[
                &["B".to_owned(), "A".to_owned()],
                &["A".to_owned(), "C".to_owned()],
            ],
            60.0,
        );
        // A appears in both lists (#2 and #1) -> strictly beats B (#1 only) and C (#2 only).
        assert_eq!(fused[0].0, "A");
        Ok(())
    }
}
