//! Generalized 4-stratum `KnowledgeVault` queries, multi-facet retrieval,
//! 3-channel Shadow Hints, conflict-cluster detection, semantic vector syncing,
//! and automatable-literature discovery (ADR-0007 D31).

use super::{
    HIGH_SCORE, ShadowHint, Wiki,
    semantic::{
        EmbeddingProvider, SemanticIndex, SemanticSyncStats, load_cached_vector, rrf_fuse,
        save_cached_vector,
    },
};
use ai4math_core::{CitationStatus, KnowledgeSourceKind, KnowledgeStratum, ReproducibilityClass};
use anyhow::{Result, ensure};
use rusqlite::{Connection, params_from_iter, types::Value};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

/// Domain-agnostic query across any of the 4 knowledge strata.
#[derive(Debug, Clone, Copy)]
pub struct VaultQuery<'a> {
    pub query: &'a str,
    pub scope: KnowledgeStratum,
    pub domain_id: Option<&'a str>,
    pub backend_id: Option<&'a str>,
    pub source_kind: Option<KnowledgeSourceKind>,
    pub facets: &'a [(&'a str, &'a str)],
    /// Retracted entries are hidden from positive recall unless explicitly requested.
    pub include_retracted: bool,
    pub include_shadow_hint: bool,
    pub limit: u32,
}

impl<'a> VaultQuery<'a> {
    pub fn new(query: &'a str, scope: KnowledgeStratum) -> Self {
        Self {
            query,
            scope,
            domain_id: None,
            backend_id: None,
            source_kind: None,
            facets: &[],
            include_retracted: false,
            include_shadow_hint: true,
            limit: 20,
        }
    }
}

/// Domain-agnostic vault hit returned by `Wiki::search_vault` / `search_vault_hybrid`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct VaultHit {
    pub id: String,
    pub title: String,
    pub stratum: KnowledgeStratum,
    pub domain_id: String,
    pub backend_id: String,
    pub epistemic_level: String,
    pub source_kind: KnowledgeSourceKind,
    pub status: String,
    pub informal_gloss: String,
    pub formal_signature: Option<String>,
    pub reproducibility_class: Option<ReproducibilityClass>,
    pub citation_status: Option<CitationStatus>,
    pub condition_tags: Vec<String>,
    pub facets: BTreeMap<String, Vec<String>>,
    pub verified_at_commit: Option<String>,
    pub file_path: String,
    pub score: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct VaultSearchResult {
    pub hits: Vec<VaultHit>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shadow_hint: Option<ShadowHint>,
}

/// Group of entries sharing key facets or linked by explicit `contradicts` edges.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConflictCluster {
    pub reason: String,
    pub facet_key_values: BTreeMap<String, String>,
    pub entry_ids: Vec<String>,
}

/// Non-retracted `LITERATURE` entry eligible for idle-time formalization or replication.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AutomatableLiteratureEntry {
    pub entry_id: String,
    pub domain_id: String,
    pub title: String,
    pub reproducibility_class: ReproducibilityClass,
    pub condition_tags: Vec<String>,
    pub origin_claim: Option<String>,
}

pub(super) fn table_exists(db: &Connection, name: &str) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        [name],
        |r| r.get(0),
    )?)
}

fn build_fts_query(raw: &str) -> String {
    let mut seen = BTreeSet::new();
    raw.split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty() && seen.insert(*word))
        .map(|word| format!("\"{word}\""))
        .collect::<Vec<_>>()
        .join(" AND ")
}

fn load_entry_facets(db: &Connection, entry_id: &str) -> Result<BTreeMap<String, Vec<String>>> {
    let mut facets: BTreeMap<String, Vec<String>> = BTreeMap::new();
    if !table_exists(db, "entry_facets")? {
        return Ok(facets);
    }
    let mut stmt = db.prepare_cached(
        "SELECT facet_key, facet_value FROM entry_facets WHERE entry_id=?1 ORDER BY facet_key, facet_value",
    )?;
    let mut rows = stmt.query([entry_id])?;
    while let Some(r) = rows.next()? {
        facets.entry(r.get(0)?).or_default().push(r.get(1)?);
    }
    Ok(facets)
}

struct RawVaultHitRow {
    title: String,
    stratum_s: String,
    trust_level: String,
    backend_id: String,
    status: String,
    informal_gloss: String,
    formal_signature: Option<String>,
    verified_at_commit: Option<String>,
    file_path: String,
    domain_id: String,
    source_kind_s: String,
    repro_s: Option<String>,
    cite_s: Option<String>,
    tags_json: String,
}

fn load_vault_hit(db: &Connection, entry_id: &str, score: f64) -> Result<VaultHit> {
    let has_meta = table_exists(db, "entry_vault_meta")?;
    let RawVaultHitRow {
        title,
        stratum_s,
        trust_level,
        backend_id,
        status,
        informal_gloss,
        formal_signature,
        verified_at_commit,
        file_path,
        domain_id,
        source_kind_s,
        repro_s,
        cite_s,
        tags_json,
    } = if has_meta {
        db.query_row(
            "SELECT e.title, e.stratum, e.trust_level, e.engine, e.status, e.informal_gloss,
                    e.formal_signature, e.verified_at_commit, e.file_path,
                    COALESCE(m.domain_id, 'itp'), COALESCE(m.source_kind, 'project_verified'),
                    m.reproducibility_class, m.citation_status, COALESCE(m.condition_tags_json, '[]')
             FROM entries e LEFT JOIN entry_vault_meta m ON m.entry_id=e.id
             WHERE e.id=?1",
            [entry_id],
            |r| {
                Ok(RawVaultHitRow {
                    title: r.get(0)?,
                    stratum_s: r.get(1)?,
                    trust_level: r.get(2)?,
                    backend_id: r.get(3)?,
                    status: r.get(4)?,
                    informal_gloss: r.get(5)?,
                    formal_signature: r.get(6)?,
                    verified_at_commit: r.get(7)?,
                    file_path: r.get(8)?,
                    domain_id: r.get(9)?,
                    source_kind_s: r.get(10)?,
                    repro_s: r.get(11)?,
                    cite_s: r.get(12)?,
                    tags_json: r.get(13)?,
                })
            },
        )?
    } else {
        db.query_row(
            "SELECT title, stratum, trust_level, engine, status, informal_gloss,
                    formal_signature, verified_at_commit, file_path
             FROM entries WHERE id=?1",
            [entry_id],
            |r| {
                Ok(RawVaultHitRow {
                    title: r.get(0)?,
                    stratum_s: r.get(1)?,
                    trust_level: r.get(2)?,
                    backend_id: r.get(3)?,
                    status: r.get(4)?,
                    informal_gloss: r.get(5)?,
                    formal_signature: r.get(6)?,
                    verified_at_commit: r.get(7)?,
                    file_path: r.get(8)?,
                    domain_id: "itp".to_owned(),
                    source_kind_s: "project_verified".to_owned(),
                    repro_s: None,
                    cite_s: None,
                    tags_json: "[]".to_owned(),
                })
            },
        )?
    };

    let stratum: KnowledgeStratum = stratum_s
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid stored stratum: {e}"))?;
    let source_kind: KnowledgeSourceKind = source_kind_s
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid stored source_kind: {e}"))?;
    let reproducibility_class = repro_s
        .map(|s| {
            s.parse::<ReproducibilityClass>()
                .map_err(|e| anyhow::anyhow!("invalid stored reproducibility_class: {e}"))
        })
        .transpose()?;
    let citation_status = cite_s
        .map(|s| {
            s.parse::<CitationStatus>()
                .map_err(|e| anyhow::anyhow!("invalid stored citation_status: {e}"))
        })
        .transpose()?;
    let condition_tags: Vec<String> = serde_json::from_str(&tags_json)?;
    let facets = load_entry_facets(db, entry_id)?;

    Ok(VaultHit {
        id: entry_id.to_owned(),
        title,
        stratum,
        domain_id,
        backend_id,
        epistemic_level: trust_level,
        source_kind,
        status,
        informal_gloss,
        formal_signature,
        reproducibility_class,
        citation_status,
        condition_tags,
        facets,
        verified_at_commit,
        file_path,
        score,
    })
}

fn candidate_ids_for_vault_query(
    db: &Connection,
    query: VaultQuery<'_>,
    stratum_override: KnowledgeStratum,
) -> Result<Vec<(String, f64)>> {
    let fts = build_fts_query(query.query);
    if fts.is_empty() && query.facets.is_empty() && !query.query.trim().is_empty() {
        return Ok(Vec::new());
    }
    let has_meta = table_exists(db, "entry_vault_meta")?;
    let has_facets = table_exists(db, "entry_facets")?;
    let mut args = Vec::<Value>::new();
    let mut sql = if fts.is_empty() {
        String::from("WITH lexical(entry_id, rank) AS (SELECT NULL, NULL WHERE 0)")
    } else {
        args.extend([
            fts.clone().into(),
            stratum_override.as_str().to_owned().into(),
        ]);
        String::from(
            "WITH lexical AS MATERIALIZED (
               SELECT entry_id, bm25(entries_fts,0,0,4,2,2,2,1) AS rank
               FROM entries_fts WHERE entries_fts MATCH ? AND stratum=?)",
        )
    };

    sql.push_str(
        " SELECT e.id, CASE WHEN l.entry_id IS NOT NULL THEN 0.90 ELSE 0.70 END AS score, l.rank
          FROM entries e LEFT JOIN lexical l ON l.entry_id=e.id",
    );
    if has_meta {
        sql.push_str(" LEFT JOIN entry_vault_meta m ON m.entry_id=e.id");
    }
    sql.push_str(" WHERE e.stratum=? AND upper(e.id) NOT GLOB 'CORE-ADAPT-*'");
    args.push(stratum_override.as_str().to_owned().into());

    if !query.include_retracted {
        sql.push_str(" AND e.status <> 'retracted'");
        if has_meta {
            sql.push_str(" AND (m.citation_status IS NULL OR m.citation_status <> 'retracted')");
        }
    }
    if stratum_override == KnowledgeStratum::Core {
        sql.push_str(
            " AND e.trust_level NOT IN ('L2b','E2b') AND e.status IN ('active','unreferenced')",
        );
    }
    if let Some(domain) = query.domain_id {
        if has_meta {
            sql.push_str(" AND COALESCE(m.domain_id, 'itp') = ?");
            args.push(domain.to_owned().into());
        } else if domain != "itp" {
            return Ok(Vec::new());
        }
    }
    if let Some(backend) = query.backend_id {
        sql.push_str(" AND e.engine = ?");
        args.push(backend.to_owned().into());
    }
    if let Some(sk) = query.source_kind
        && has_meta
    {
        sql.push_str(" AND COALESCE(m.source_kind, 'project_verified') = ?");
        args.push(sk.as_str().to_owned().into());
    }
    if !fts.is_empty() && query.facets.is_empty() {
        sql.push_str(" AND l.entry_id IS NOT NULL");
    }
    for &(key, val) in query.facets {
        if has_facets {
            sql.push_str(
                " AND EXISTS (SELECT 1 FROM entry_facets WHERE entry_id=e.id AND facet_key=? AND facet_value=?)",
            );
            args.extend([key.to_owned().into(), val.to_owned().into()]);
        } else {
            return Ok(Vec::new());
        }
    }
    sql.push_str(" ORDER BY score DESC, l.rank IS NULL, l.rank, e.id");
    let mut stmt = db.prepare(&sql)?;
    let mut rows = stmt.query(params_from_iter(args))?;
    let mut out = Vec::new();
    while let Some(r) = rows.next()? {
        out.push((r.get(0)?, r.get(1)?));
    }
    Ok(out)
}

pub(super) fn literature_shadow_teaser(
    db: &Connection,
    raw_query: &str,
    facets: &[(&str, &str)],
) -> Result<(usize, Option<String>)> {
    let q = VaultQuery {
        query: raw_query,
        scope: KnowledgeStratum::Literature,
        domain_id: None,
        backend_id: None,
        source_kind: None,
        facets,
        include_retracted: false,
        include_shadow_hint: false,
        limit: 5,
    };
    let matches = candidate_ids_for_vault_query(db, q, KnowledgeStratum::Literature)?;
    let count = matches.len();
    let top = matches.into_iter().next().map(|(id, _)| id);
    Ok((count, top))
}

impl Wiki {
    /// Generalized multi-domain, 4-stratum vault search supporting arbitrary
    /// `entry_facets` filters and 3-channel Shadow Hints (sediment + graveyard + literature).
    pub fn search_vault(&self, query: VaultQuery<'_>) -> Result<VaultSearchResult> {
        let tx = self.db.unchecked_transaction()?;
        let limit = query.limit.max(1) as usize;
        let ranked = candidate_ids_for_vault_query(&tx, query, query.scope)?;
        let mut hits = Vec::with_capacity(ranked.len().min(limit));
        for (id, score) in ranked.iter().take(limit) {
            hits.push(load_vault_hit(&tx, id, *score)?);
        }

        let shadow_hint = if query.scope == KnowledgeStratum::Core
            && query.include_shadow_hint
            && (!query.query.trim().is_empty() || !query.facets.is_empty())
        {
            let mut hint = ShadowHint::default();
            let high_core = ranked.iter().filter(|(_, s)| *s > HIGH_SCORE).count();
            if high_core < 2 {
                let sed = candidate_ids_for_vault_query(&tx, query, KnowledgeStratum::Sediment)?;
                hint.sediment_count = sed.len();
            }
            let grv = candidate_ids_for_vault_query(&tx, query, KnowledgeStratum::Graveyard)?;
            hint.graveyard_count = grv.len();
            hint.top_graveyard_id = grv.into_iter().next().map(|(id, _)| id);

            let (lit_count, top_lit) = literature_shadow_teaser(&tx, query.query, query.facets)?;
            hint.literature_count = lit_count;
            hint.top_literature_id = top_lit;

            (hint.sediment_count > 0 || hint.graveyard_count > 0 || hint.literature_count > 0)
                .then_some(hint)
        } else {
            None
        };
        tx.commit()?;
        Ok(VaultSearchResult { hits, shadow_hint })
    }

    /// Synchronize the pluggable `SemanticIndex` against all indexed entries,
    /// using the SQLite `entry_vectors` cache to skip re-embedding unchanged files.
    pub fn sync_semantic_index(
        &mut self,
        embedder: &dyn EmbeddingProvider,
        semantic: &mut dyn SemanticIndex,
    ) -> Result<SemanticSyncStats> {
        let dim = embedder.dim();
        if dim == 0 {
            return Ok(SemanticSyncStats::default());
        }
        ensure!(
            table_exists(&self.db, "entry_vectors")? && table_exists(&self.db, "entry_vault_meta")?,
            "vault index schema predates semantic vector caching; rebuild the index first"
        );
        let model_id = embedder.model_id();
        let rows: Vec<(String, String, String, String)> = {
            let mut stmt = self.db.prepare(
                "SELECT e.id, e.title, e.informal_gloss, COALESCE(m.content_sha256, '')
                 FROM entries e LEFT JOIN entry_vault_meta m ON m.entry_id=e.id
                 WHERE upper(e.id) NOT GLOB 'CORE-ADAPT-*'
                 ORDER BY e.id",
            )?;
            let mapped =
                stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
            mapped.collect::<std::result::Result<_, _>>()?
        };

        let mut stats = SemanticSyncStats {
            total_entries: rows.len(),
            cache_hits: 0,
            embedded_fresh: 0,
        };
        let mut live_ids = BTreeSet::new();
        let mut to_embed: Vec<(String, String, String)> = Vec::new();

        for (id, title, gloss, sha) in rows {
            live_ids.insert(id.clone());
            if let Some(vec) = load_cached_vector(&self.db, &id, model_id, dim, &sha)? {
                semantic.upsert(&id, &sha, &vec)?;
                stats.cache_hits += 1;
            } else {
                to_embed.push((
                    id,
                    sha,
                    format!(
                        "{title}
{gloss}"
                    ),
                ));
            }
        }

        if !to_embed.is_empty() {
            let texts: Vec<&str> = to_embed.iter().map(|(_, _, t)| t.as_str()).collect();
            let vectors = embedder.embed_batch(&texts)?;
            ensure!(
                vectors.len() == to_embed.len(),
                "embedding provider returned mismatched vector count"
            );
            for ((id, sha, _), vec) in to_embed.into_iter().zip(vectors) {
                ensure!(vec.len() == dim, "embedding vector dimension mismatch");
                save_cached_vector(&self.db, &id, model_id, dim, &sha, &vec)?;
                semantic.upsert(&id, &sha, &vec)?;
                stats.embedded_fresh += 1;
            }
        }
        semantic.remove_missing(&live_ids)?;
        Ok(stats)
    }

    /// Three-way RRF hybrid retrieval fusing deterministic `Facet + FTS5` candidates
    /// with pluggable dense `SemanticIndex` similarity (gracefully degrades to
    /// deterministic search when `embedder.dim() == 0`).
    pub fn search_vault_hybrid(
        &self,
        query: VaultQuery<'_>,
        embedder: &dyn EmbeddingProvider,
        semantic: &dyn SemanticIndex,
    ) -> Result<VaultSearchResult> {
        let base = self.search_vault(query)?;
        if embedder.dim() == 0 || query.query.trim().is_empty() {
            return Ok(base);
        }
        let tx = self.db.unchecked_transaction()?;
        // Build allowed scope ID set matching stratum + domain + facet + non-retracted constraints.
        let scope_only = VaultQuery { query: "", ..query };
        let scope_candidates = candidate_ids_for_vault_query(&tx, scope_only, query.scope)?;
        let allowed_ids: BTreeSet<String> =
            scope_candidates.into_iter().map(|(id, _)| id).collect();
        if allowed_ids.is_empty() {
            return Ok(base);
        }
        let q_vec = embedder.embed_one(query.query)?;
        let limit = query.limit.max(1) as usize;
        let sem_hits = semantic.search(&q_vec, Some(&allowed_ids), limit * 2)?;
        if sem_hits.is_empty() {
            return Ok(base);
        }

        let lex_ids: Vec<String> = base.hits.iter().map(|h| h.id.clone()).collect();
        let sem_ids: Vec<String> = sem_hits.into_iter().map(|(id, _)| id).collect();
        let fused = rrf_fuse(&[&lex_ids, &sem_ids], 60.0);

        let mut hits = Vec::with_capacity(fused.len().min(limit));
        for (id, score) in fused.into_iter().take(limit) {
            hits.push(load_vault_hit(&tx, &id, score)?);
        }
        tx.commit()?;
        Ok(VaultSearchResult {
            hits,
            shadow_hint: base.shadow_hint,
        })
    }

    /// Detect explicit contradiction edges (`contradicts`) and multi-entry clusters
    /// sharing identical facet tuples (e.g. `["target", "metric"]`) so conflicting
    /// literature or local-vs-literature results surface explicitly instead of colliding silently.
    pub fn find_conflict_clusters(&self, group_by_facets: &[&str]) -> Result<Vec<ConflictCluster>> {
        let mut clusters = Vec::new();
        // 1. Explicit `contradicts` edges.
        {
            let mut stmt = self.db.prepare(
                "SELECT src_id, dst_id FROM entry_edges WHERE edge_type='contradicts' ORDER BY src_id, dst_id",
            )?;
            let mut rows = stmt.query([])?;
            while let Some(r) = rows.next()? {
                let src: String = r.get(0)?;
                let dst: String = r.get(1)?;
                clusters.push(ConflictCluster {
                    reason: "explicit_contradicts_edge".to_owned(),
                    facet_key_values: BTreeMap::new(),
                    entry_ids: vec![src, dst],
                });
            }
        }
        // 2. Multi-entry facet-tuple collisions (e.g. two papers reporting different values for same target+metric).
        if !group_by_facets.is_empty() && table_exists(&self.db, "entry_facets")? {
            let mut by_entry: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
            let mut stmt = self.db.prepare(
                "SELECT f.entry_id, f.facet_key, f.facet_value
                 FROM entry_facets f JOIN entries e ON e.id=f.entry_id
                 WHERE e.status <> 'retracted'
                 ORDER BY f.entry_id, f.facet_key, f.facet_value",
            )?;
            let mut rows = stmt.query([])?;
            while let Some(r) = rows.next()? {
                let eid: String = r.get(0)?;
                let k: String = r.get(1)?;
                let v: String = r.get(2)?;
                if group_by_facets.contains(&k.as_str()) {
                    by_entry.entry(eid).or_default().entry(k).or_insert(v);
                }
            }
            let mut buckets: BTreeMap<BTreeMap<String, String>, Vec<String>> = BTreeMap::new();
            for (eid, kv) in by_entry {
                if kv.len() == group_by_facets.len() {
                    buckets.entry(kv).or_default().push(eid);
                }
            }
            for (kv, mut ids) in buckets {
                if ids.len() >= 2 {
                    ids.sort();
                    clusters.push(ConflictCluster {
                        reason: "shared_facet_tuple_candidates".to_owned(),
                        facet_key_values: kv,
                        entry_ids: ids,
                    });
                }
            }
        }
        Ok(clusters)
    }

    /// Enumerate all non-retracted `LITERATURE` entries whose `reproducibility_class`
    /// supports automated idle-time formalization or computational replication.
    pub fn list_automatable_literature(&self) -> Result<Vec<AutomatableLiteratureEntry>> {
        if !table_exists(&self.db, "entry_vault_meta")? {
            return Ok(Vec::new());
        }
        let mut stmt = self.db.prepare(
            "SELECT e.id, m.domain_id, e.title, m.reproducibility_class,
                    m.condition_tags_json, e.origin_claim
             FROM entries e JOIN entry_vault_meta m ON m.entry_id=e.id
             WHERE e.stratum='literature'
               AND e.status <> 'retracted'
               AND (m.citation_status IS NULL OR m.citation_status <> 'retracted')
               AND m.reproducibility_class IN ('formalizable_in_itp','replicable_by_protocol')
             ORDER BY e.id",
        )?;
        let mut rows = stmt.query([])?;
        let mut out = Vec::new();
        while let Some(r) = rows.next()? {
            let repro: ReproducibilityClass = r
                .get::<_, String>(3)?
                .parse()
                .map_err(|e| anyhow::anyhow!("invalid reproducibility_class: {e}"))?;
            let tags_json: String = r.get(4)?;
            out.push(AutomatableLiteratureEntry {
                entry_id: r.get(0)?,
                domain_id: r.get(1)?,
                title: r.get(2)?,
                reproducibility_class: repro,
                condition_tags: serde_json::from_str(&tags_json)?,
                origin_claim: r.get(5)?,
            });
        }
        Ok(out)
    }
}
