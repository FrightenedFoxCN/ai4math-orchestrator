-- AI4Math Orchestrator Unified Runtime State & Append-Only Event Store
-- Stored locally at .runtime/state.sqlite (untracked by Git; Git holds source, ADRs, RFCs, Wiki .md).

PRAGMA foreign_keys = ON;
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;

-- 1. Append-Only Event Log (monotonic timeline for Live HUD & Briefing Fact Pack diffs)
CREATE TABLE IF NOT EXISTS events (
  event_seq INTEGER PRIMARY KEY AUTOINCREMENT,
  occurred_at TEXT NOT NULL,
  event_type TEXT NOT NULL,           -- CLAIM_TRUST_CHANGED | PLAN_INVALIDATED | PROMOTION_COMMITTED | BREAKER_TRIPPED | ADR_APPLIED | ...
  entity_type TEXT NOT NULL,          -- claim | proof_plan | rfc | breaker | inbox | receipt | snapshot | cursor
  entity_id TEXT NOT NULL,
  git_commit TEXT,
  actor TEXT NOT NULL,                -- chief_orchestrator | broker | human | task:<id>
  payload_json TEXT NOT NULL DEFAULT '{}'
);

CREATE INDEX IF NOT EXISTS idx_events_type_seq
  ON events (event_type, event_seq);

CREATE INDEX IF NOT EXISTS idx_events_entity
  ON events (entity_type, entity_id, event_seq);

-- 2. Human Checkpoint Cursors (tracks exact boundary for incremental review briefings)
CREATE TABLE IF NOT EXISTS human_checkpoints (
  cursor_id TEXT PRIMARY KEY,
  updated_at TEXT NOT NULL,
  event_seq INTEGER NOT NULL REFERENCES events(event_seq),
  git_commit TEXT NOT NULL,
  last_adr_id TEXT,
  is_current INTEGER NOT NULL DEFAULT 0 CHECK (is_current IN (0, 1))
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_human_checkpoints_single_current
  ON human_checkpoints (is_current) WHERE is_current = 1;

-- 3. Claim Graph: Claims (OR nodes) with dual trust levels
CREATE TABLE IF NOT EXISTS claims (
  claim_id TEXT PRIMARY KEY,
  title TEXT NOT NULL,
  informal_statement TEXT NOT NULL,
  formal_signature TEXT,
  formal_target TEXT,                 -- e.g. Infra/Spectral/Proofs/Gap.lean#spectral_gap_pos
  module_path TEXT,                   -- physical file path for .olean/.agdai/.vo coupling
  engine TEXT NOT NULL CHECK (engine IN ('lean4', 'agda', 'rocq', 'smt', 'atp', 'cas', 'informal')),
  is_critical_path INTEGER NOT NULL DEFAULT 0 CHECK (is_critical_path IN (0, 1)),
  local_trust_level TEXT NOT NULL CHECK (
    local_trust_level IN ('L0', 'L1', 'L2a', 'L2b', 'L3', 'L4a', 'L4b', 'L5')
  ),
  effective_trust_level TEXT NOT NULL CHECK (
    effective_trust_level IN ('L0', 'L1', 'L2a', 'L2b', 'L3', 'L4a', 'L4b', 'L5')
  ),
  prior_trust_level TEXT CHECK (
    prior_trust_level IS NULL OR prior_trust_level IN ('L0', 'L1', 'L2a', 'L2b', 'L3', 'L4a', 'L4b', 'L5')
  ),
  stale INTEGER NOT NULL DEFAULT 0 CHECK (stale IN (0, 1)),
  effective_stale INTEGER NOT NULL DEFAULT 0 CHECK (effective_stale IN (0, 1)),
  taint_root_commit TEXT,
  oracle_debt INTEGER NOT NULL DEFAULT 0 CHECK (oracle_debt IN (0, 1)),
  def_fingerprint TEXT,
  wiki_id TEXT,
  receipt_id TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_claims_trust_stale
  ON claims (effective_trust_level, stale, is_critical_path);

CREATE INDEX IF NOT EXISTS idx_claims_module
  ON claims (module_path);

-- 4. Claim Graph: ProofPlans (AND nodes) & dependencies
CREATE TABLE IF NOT EXISTS proof_plans (
  plan_id TEXT PRIMARY KEY,
  target_claim_id TEXT NOT NULL REFERENCES claims(claim_id) ON DELETE CASCADE,
  name TEXT NOT NULL,
  track TEXT NOT NULL CHECK (track IN ('exploration', 'infrastructure')),
  status TEXT NOT NULL CHECK (
    status IN ('ACTIVE', 'PROVED', 'SUSPENDED_SOFT_PRUNE', 'INVALIDATED_BY_REFUTATION', 'ARCHIVED_BY_ADR')
  ),
  effective_trust_level TEXT NOT NULL CHECK (
    effective_trust_level IN ('L0', 'L1', 'L2a', 'L2b', 'L3', 'L4a', 'L4b', 'L5')
  ),
  rfc_id TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_proof_plans_target
  ON proof_plans (target_claim_id, status);

CREATE TABLE IF NOT EXISTS plan_dependencies (
  plan_id TEXT NOT NULL REFERENCES proof_plans(plan_id) ON DELETE CASCADE,
  dep_claim_id TEXT NOT NULL REFERENCES claims(claim_id) ON DELETE RESTRICT,
  dep_kind TEXT NOT NULL CHECK (dep_kind IN ('lemma', 'definition', 'oracle_placeholder')),
  PRIMARY KEY (plan_id, dep_claim_id)
);

CREATE INDEX IF NOT EXISTS idx_plan_deps_reverse
  ON plan_dependencies (dep_claim_id, plan_id);

-- 5. Physical Module Graph & Trusted Golden Artifact Snapshots (.olean / .agdai / .vo)
CREATE TABLE IF NOT EXISTS module_dependencies (
  module_path TEXT NOT NULL,
  imports_module_path TEXT NOT NULL,
  engine TEXT NOT NULL CHECK (engine IN ('lean4', 'agda', 'rocq')),
  layer TEXT NOT NULL CHECK (layer IN ('defs_spec', 'leaf_proof', 'aggregator')),
  PRIMARY KEY (module_path, imports_module_path)
);

CREATE INDEX IF NOT EXISTS idx_module_deps_reverse
  ON module_dependencies (imports_module_path, module_path);

CREATE TABLE IF NOT EXISTS golden_snapshots (
  snapshot_id TEXT PRIMARY KEY,
  engine TEXT NOT NULL CHECK (engine IN ('lean4', 'agda', 'rocq')),
  git_commit TEXT NOT NULL,
  toolchain_version TEXT NOT NULL,
  cache_dir_path TEXT NOT NULL UNIQUE,
  defs_layer_hash TEXT NOT NULL,      -- if unchanged across promotions, Fast Lane REPL needs no reload
  pinned INTEGER NOT NULL DEFAULT 0 CHECK (pinned IN (0, 1)),
  pin_reason TEXT,                    -- head_recent | breaker:<id> | adr:<id>
  created_at TEXT NOT NULL,
  last_used_at TEXT NOT NULL
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_golden_engine_commit
  ON golden_snapshots (engine, git_commit);

-- 6. Broker-Issued Formal Verification Receipts (sole promotion authority)
CREATE TABLE IF NOT EXISTS verification_receipts (
  receipt_id TEXT PRIMARY KEY,
  issued_at TEXT NOT NULL,
  tier TEXT NOT NULL CHECK (tier IN ('T1_PROMOTION', 'T2_AUDIT', 'T3_TAINT_REBUILD')),
  engine TEXT NOT NULL CHECK (engine IN ('lean4', 'agda', 'rocq')),
  base_commit TEXT NOT NULL,
  patch_sha256 TEXT NOT NULL,
  target_decl TEXT NOT NULL,
  allowlist_passed INTEGER NOT NULL CHECK (allowlist_passed IN (0, 1)),
  axioms_used_json TEXT NOT NULL,
  external_checker_passed INTEGER CHECK (external_checker_passed IN (0, 1)),
  compile_seconds REAL NOT NULL,
  max_heartbeats INTEGER,
  receipt_sha256 TEXT NOT NULL UNIQUE,
  raw_receipt_json TEXT NOT NULL
);

-- 7. Resumable Local Circuit Breakers (Definition Smell protection)
CREATE TABLE IF NOT EXISTS circuit_breakers (
  breaker_id TEXT PRIMARY KEY,
  target_definition_id TEXT NOT NULL,
  module_path TEXT NOT NULL,
  engine TEXT NOT NULL CHECK (engine IN ('lean4', 'agda', 'rocq')),
  state TEXT NOT NULL CHECK (state IN ('CLOSED', 'TRIPPED', 'HALF_OPEN', 'ARCHIVED')),
  pinned_snapshot_id TEXT REFERENCES golden_snapshots(snapshot_id),
  retained_worktree_path TEXT,
  paused_task_ids_json TEXT NOT NULL DEFAULT '[]',
  trigger_signals_json TEXT NOT NULL DEFAULT '{}',
  linked_inbox_id TEXT,
  tripped_at TEXT,
  updated_at TEXT NOT NULL
);

-- 8. Decision Inbox (Amber Zone non-blocking queue)
CREATE TABLE IF NOT EXISTS decision_inbox (
  inbox_id TEXT PRIMARY KEY,
  created_at TEXT NOT NULL,
  priority TEXT NOT NULL CHECK (priority IN ('P0', 'P1', 'P2')),
  category TEXT NOT NULL CHECK (
    category IN (
      'breaking_def_change',
      'strategy_branch_pruning',
      'hard_bottleneck_escalation',
      'cross_engine_admission',
      'oracle_debt_waiver'
    )
  ),
  leverage_score REAL NOT NULL DEFAULT 0,
  summary TEXT NOT NULL,
  payload_json TEXT NOT NULL,
  status TEXT NOT NULL CHECK (status IN ('PENDING', 'RESOLVED', 'DISMISSED')),
  resolved_adr TEXT,
  resolved_at TEXT
);

CREATE INDEX IF NOT EXISTS idx_inbox_pending_rank
  ON decision_inbox (status, priority, leverage_score DESC);
