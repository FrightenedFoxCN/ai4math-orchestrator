-- Generalized multi-domain AND-OR-Conflict epistemic factor graph & BP snapshots (migration 9).
-- Preserves legacy ITP claims/proof_plans tables and foreign keys untouched while enabling
-- multi-domain claims (E0..E5 and L0..L5), condition tags, explicit contradicts/replicates
-- edges, sealed receipt upgrades, and persisted Subjective Logic BP + VoI snapshots.

CREATE TABLE epistemic_nodes (
  node_id TEXT NOT NULL PRIMARY KEY CHECK(
    length(node_id) BETWEEN 1 AND 128 AND node_id NOT GLOB '*[^A-Za-z0-9_.:-]*'
  ),
  domain_id TEXT NOT NULL CHECK(
    length(domain_id) BETWEEN 1 AND 64 AND domain_id NOT GLOB '*[^A-Za-z0-9_-]*'
  ),
  backend_id TEXT NOT NULL CHECK(
    length(backend_id) BETWEEN 1 AND 64 AND backend_id NOT GLOB '*[^A-Za-z0-9_.:-]*'
  ),
  title TEXT NOT NULL CHECK(length(title) BETWEEN 1 AND 512),
  statement TEXT NOT NULL CHECK(length(statement) BETWEEN 1 AND 16384),
  is_critical_path INTEGER NOT NULL DEFAULT 0 CHECK(is_critical_path IN (0, 1)),
  local_level TEXT NOT NULL CHECK(
    local_level IN ('L0','L1','L2a','L2b','L3','L4a','L4b','L5','E0','E1','E2a','E2b','E3','E4a','E4b','E5')
  ),
  condition_tags_json TEXT NOT NULL DEFAULT '[]',
  local_debts_json TEXT NOT NULL DEFAULT '[]',
  reproducibility_class TEXT CHECK(
    reproducibility_class IS NULL
    OR reproducibility_class IN ('formalizable_in_itp','replicable_by_protocol','requires_wet_lab','background_only')
  ),
  source_entry_id TEXT CHECK(source_entry_id IS NULL OR (length(source_entry_id) BETWEEN 1 AND 128)),
  sealed_receipt_sha256 TEXT CHECK(
    sealed_receipt_sha256 IS NULL
    OR (length(sealed_receipt_sha256)=64 AND sealed_receipt_sha256 NOT GLOB '*[^0-9a-f]*')
  ),
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
) STRICT;

-- Enforce strict domain <-> epistemic level separation in SQLite itself:
-- 'itp' domain only permits L0..L5; non-ITP domains only permit E0..E5.
CREATE TRIGGER epistemic_nodes_domain_level_insert BEFORE INSERT ON epistemic_nodes
WHEN (NEW.domain_id = 'itp' AND NEW.local_level NOT IN ('L0','L1','L2a','L2b','L3','L4a','L4b','L5'))
  OR (NEW.domain_id <> 'itp' AND NEW.local_level NOT IN ('E0','E1','E2a','E2b','E3','E4a','E4b','E5'))
BEGIN SELECT RAISE(ABORT, 'epistemic_nodes local_level does not match domain_id'); END;

CREATE TRIGGER epistemic_nodes_domain_level_update BEFORE UPDATE ON epistemic_nodes
WHEN NEW.domain_id <> OLD.domain_id
  OR (NEW.domain_id = 'itp' AND NEW.local_level NOT IN ('L0','L1','L2a','L2b','L3','L4a','L4b','L5'))
  OR (NEW.domain_id <> 'itp' AND NEW.local_level NOT IN ('E0','E1','E2a','E2b','E3','E4a','E4b','E5'))
BEGIN SELECT RAISE(ABORT, 'epistemic_nodes domain_id is immutable and must match local_level'); END;

CREATE TABLE epistemic_plans (
  plan_id TEXT NOT NULL PRIMARY KEY CHECK(
    length(plan_id) BETWEEN 1 AND 128 AND plan_id NOT GLOB '*[^A-Za-z0-9_.:-]*'
  ),
  target_node_id TEXT NOT NULL REFERENCES epistemic_nodes(node_id) ON DELETE RESTRICT,
  name TEXT NOT NULL CHECK(length(name) BETWEEN 1 AND 256),
  active INTEGER NOT NULL DEFAULT 1 CHECK(active IN (0, 1)),
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
) STRICT;

CREATE INDEX idx_epistemic_plans_target
  ON epistemic_plans (target_node_id, active);

CREATE TABLE epistemic_plan_dependencies (
  plan_id TEXT NOT NULL REFERENCES epistemic_plans(plan_id) ON DELETE RESTRICT,
  dep_node_id TEXT NOT NULL REFERENCES epistemic_nodes(node_id) ON DELETE RESTRICT,
  PRIMARY KEY (plan_id, dep_node_id)
) STRICT;

CREATE TABLE epistemic_conflict_edges (
  edge_id TEXT NOT NULL PRIMARY KEY CHECK(
    length(edge_id) BETWEEN 1 AND 128 AND edge_id NOT GLOB '*[^A-Za-z0-9_.:-]*'
  ),
  source_node_id TEXT NOT NULL CHECK(length(source_node_id) BETWEEN 1 AND 128),
  target_node_id TEXT NOT NULL CHECK(length(target_node_id) BETWEEN 1 AND 128),
  edge_kind TEXT NOT NULL CHECK(edge_kind IN ('contradicts','replicates')),
  weight REAL NOT NULL CHECK(weight > 0.0 AND weight <= 1.0),
  evidence_receipt_sha256 TEXT CHECK(
    evidence_receipt_sha256 IS NULL
    OR (length(evidence_receipt_sha256)=64 AND evidence_receipt_sha256 NOT GLOB '*[^0-9a-f]*')
  ),
  rationale TEXT NOT NULL DEFAULT '',
  event_seq INTEGER NOT NULL REFERENCES events(event_seq),
  created_at TEXT NOT NULL
) STRICT;

CREATE INDEX idx_epistemic_conflict_target
  ON epistemic_conflict_edges (target_node_id, edge_kind);

CREATE TABLE epistemic_belief_snapshots (
  snapshot_seq INTEGER PRIMARY KEY AUTOINCREMENT,
  event_seq INTEGER NOT NULL REFERENCES events(event_seq),
  converged INTEGER NOT NULL CHECK(converged IN (0, 1)),
  iterations INTEGER NOT NULL CHECK(iterations >= 1),
  report_json TEXT NOT NULL,
  computed_at TEXT NOT NULL
) STRICT;

CREATE TRIGGER epistemic_belief_snapshots_no_update BEFORE UPDATE ON epistemic_belief_snapshots
BEGIN SELECT RAISE(ABORT, 'belief snapshots are append-only'); END;

CREATE TRIGGER epistemic_belief_snapshots_no_delete BEFORE DELETE ON epistemic_belief_snapshots
BEGIN SELECT RAISE(ABORT, 'belief snapshots cannot be deleted'); END;
