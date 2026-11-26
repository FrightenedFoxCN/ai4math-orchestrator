-- Local Knowledge Vault SQLite Index Schema (evolved smoothly from Local Math Wiki).
-- Static entries come from .mathwiki/{core,sediment,graveyard,literature}/*.md (Git-tracked).
-- Managed Core metadata comes from a caller-validated Git/ledger generation.
-- This SQLite database (.mathwiki/.index.sqlite) is local, single-file, and fully rebuildable.

PRAGMA foreign_keys = ON;
PRAGMA journal_mode = WAL;

-- 1. Core metadata table (never hard-delete unreferenced entries; demote stratum instead)
CREATE TABLE IF NOT EXISTS entries (
  id TEXT PRIMARY KEY,
  stratum TEXT NOT NULL CHECK (stratum IN ('core', 'sediment', 'graveyard', 'literature')),
  trust_level TEXT NOT NULL CHECK (
    trust_level IN (
      'L0', 'L1', 'L2a', 'L2b', 'L3', 'L4a', 'L4b', 'L5',
      'E0', 'E1', 'E2a', 'E2b', 'E3', 'E4a', 'E4b', 'E5'
    )
  ),
  engine TEXT NOT NULL CHECK (length(engine) BETWEEN 1 AND 64),
  status TEXT NOT NULL CHECK (status IN ('active', 'unreferenced', 'superseded', 'refuted', 'retracted')),
  pinned INTEGER NOT NULL DEFAULT 0 CHECK (pinned IN (0, 1)),
  title TEXT NOT NULL,
  informal_gloss TEXT NOT NULL,
  formal_signature TEXT,
  tags_json TEXT NOT NULL DEFAULT '[]',
  verified_at_commit TEXT,
  origin_branch TEXT,
  origin_claim TEXT,
  def_fingerprints_json TEXT NOT NULL DEFAULT '{}',
  compat_status TEXT NOT NULL DEFAULT 'unknown' CHECK (
    compat_status IN ('compatible', 'needs_adaptation', 'incompatible', 'unknown')
  ),
  ref_count INTEGER NOT NULL DEFAULT 0,
  access_count INTEGER NOT NULL DEFAULT 0,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  last_accessed_at TEXT,
  file_path TEXT NOT NULL UNIQUE
);

CREATE INDEX IF NOT EXISTS idx_entries_stratum_trust
  ON entries (stratum, trust_level, engine);

CREATE INDEX IF NOT EXISTS idx_entries_status_ref
  ON entries (stratum, status, ref_count, pinned);

-- 2. Symbolic structure inverted index (Loogle/Hoogle-style formal signature retrieval for ITP)
CREATE TABLE IF NOT EXISTS entry_symbols (
  entry_id TEXT NOT NULL REFERENCES entries(id) ON DELETE CASCADE,
  role TEXT NOT NULL CHECK (
    role IN ('conclusion_head', 'hypothesis_head', 'typeclass', 'const')
  ),
  symbol TEXT NOT NULL,
  PRIMARY KEY (entry_id, role, symbol)
);

CREATE INDEX IF NOT EXISTS idx_entry_symbols_lookup
  ON entry_symbols (role, symbol, entry_id);

-- 3. Provenance and scientific dependency graph edges
CREATE TABLE IF NOT EXISTS entry_edges (
  src_id TEXT NOT NULL REFERENCES entries(id) ON DELETE CASCADE,
  dst_id TEXT NOT NULL,
  edge_type TEXT NOT NULL CHECK (
    edge_type IN (
      'depends_on',
      'used_by',
      'refutes',
      'generalizes',
      'superseded_by',
      'born_from_claim',
      'bridge_source',
      'extracted_from',
      'replicates',
      'contradicts',
      'calibrates_protocol',
      'cites_secondary'
    )
  ),
  metadata_json TEXT NOT NULL DEFAULT '{}',
  PRIMARY KEY (src_id, dst_id, edge_type)
);

CREATE INDEX IF NOT EXISTS idx_entry_edges_dst
  ON entry_edges (dst_id, edge_type);

-- 4. Full-text search index (FTS5 BM25 over cross-domain concepts, glosses, and signatures)
CREATE VIRTUAL TABLE IF NOT EXISTS entries_fts USING fts5(
  entry_id UNINDEXED,
  stratum UNINDEXED,
  title,
  informal_gloss,
  tags,
  formal_signature,
  notes,
  tokenize = 'unicode61'
);

-- 5. Disposable managed Core projection. CORE-ADAPT-* entries and their incident
-- edges use the tables above. The generation marker and FTS corpus are separate
-- so old indexes need no table migration and hidden rows cannot change default rank.
CREATE TABLE IF NOT EXISTS managed_core_generation (
  singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
  head TEXT NOT NULL CHECK (length(head) IN (40, 64) AND head NOT GLOB '*[^0-9a-f]*'),
  event_seq INTEGER NOT NULL CHECK (event_seq >= 0)
);

CREATE VIRTUAL TABLE IF NOT EXISTS managed_core_fts USING fts5(
  entry_id UNINDEXED,
  title,
  informal_gloss,
  tokenize = 'unicode61'
);

-- 6. Generalized domain metadata & facet inverted index (ADR-0007 D31).
-- Additive companion tables preserve read compatibility with legacy .index.sqlite files.
CREATE TABLE IF NOT EXISTS entry_vault_meta (
  entry_id TEXT PRIMARY KEY REFERENCES entries(id) ON DELETE CASCADE,
  domain_id TEXT NOT NULL DEFAULT 'itp',
  source_kind TEXT NOT NULL DEFAULT 'project_verified' CHECK (
    source_kind IN (
      'project_verified',
      'project_archived',
      'project_refuted',
      'literature_extracted',
      'human_curated'
    )
  ),
  reproducibility_class TEXT CHECK (
    reproducibility_class IS NULL
    OR reproducibility_class IN (
      'formalizable_in_itp',
      'replicable_by_protocol',
      'requires_wet_lab',
      'background_only'
    )
  ),
  citation_status TEXT CHECK (
    citation_status IS NULL
    OR citation_status IN ('active', 'corrected', 'retracted', 'preprint', 'unverified')
  ),
  condition_tags_json TEXT NOT NULL DEFAULT '[]',
  content_sha256 TEXT NOT NULL DEFAULT ''
);

CREATE INDEX IF NOT EXISTS idx_entry_vault_meta_domain
  ON entry_vault_meta (domain_id, source_kind, reproducibility_class);

CREATE TABLE IF NOT EXISTS entry_facets (
  entry_id TEXT NOT NULL REFERENCES entries(id) ON DELETE CASCADE,
  facet_key TEXT NOT NULL,
  facet_value TEXT NOT NULL,
  PRIMARY KEY (entry_id, facet_key, facet_value)
);

CREATE INDEX IF NOT EXISTS idx_entry_facets_lookup
  ON entry_facets (facet_key, facet_value, entry_id);

-- 7. Content-addressed local vector cache backing pluggable SemanticIndex providers.
CREATE TABLE IF NOT EXISTS entry_vectors (
  entry_id TEXT NOT NULL REFERENCES entries(id) ON DELETE CASCADE,
  model_id TEXT NOT NULL,
  dim INTEGER NOT NULL CHECK (dim >= 1),
  content_sha256 TEXT NOT NULL,
  vector_blob BLOB NOT NULL,
  PRIMARY KEY (entry_id, model_id)
);
