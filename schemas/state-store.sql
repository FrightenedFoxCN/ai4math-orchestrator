-- AI4Math Orchestrator Unified Runtime State & Append-Only Event Store
-- Stored locally at .runtime/state.sqlite (untracked by Git; Git holds source, ADRs, RFCs, Wiki .md).

PRAGMA foreign_keys = ON;
PRAGMA journal_mode = WAL;
PRAGMA synchronous = FULL;

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

-- 9. Daemon P0 scan/deduplication. Internal, not an agent-writable interface.
CREATE TABLE IF NOT EXISTS daemon_alert_cursor (
  singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
  event_seq INTEGER NOT NULL CHECK (event_seq >= 0)
);
INSERT OR IGNORE INTO daemon_alert_cursor(singleton,event_seq) VALUES(1,0);

CREATE TABLE IF NOT EXISTS daemon_alerts (
  alert_id TEXT PRIMARY KEY,
  event_seq INTEGER NOT NULL UNIQUE REFERENCES events(event_seq),
  created_at TEXT NOT NULL,
  reason_code TEXT NOT NULL,
  dedupe_key TEXT NOT NULL,
  headline TEXT NOT NULL,
  entity_id TEXT NOT NULL,
  briefing_id TEXT NOT NULL,
  dispatched_unix INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_daemon_alerts_dedupe ON daemon_alerts(dedupe_key, dispatched_unix);

-- Derived refutation state does not change the kernel trust of a claim.
CREATE TABLE IF NOT EXISTS proof_plan_route_blocks (
  plan_id TEXT PRIMARY KEY REFERENCES proof_plans(plan_id),
  refuted_claim_id TEXT NOT NULL REFERENCES claims(claim_id)
);
CREATE TABLE IF NOT EXISTS claim_route_exhaustion (
  claim_id TEXT PRIMARY KEY REFERENCES claims(claim_id),
  refuted_claim_id TEXT NOT NULL REFERENCES claims(claim_id)
);
CREATE TABLE IF NOT EXISTS state_migrations (version INTEGER PRIMARY KEY);

-- Cooperating CLI uses a durable prepare record before updating Git. The
-- recovery process re-verifies source rather than treating the record as evidence.
CREATE TABLE IF NOT EXISTS managed_repo (singleton INTEGER PRIMARY KEY CHECK (singleton = 1), repo TEXT NOT NULL);

-- Internal regression episode state, never projected or used as trust evidence.
-- Holds only the last observed base; a newer observed base or final promotion
-- clears all pending and latched contexts. At most 4096 context rows per base
-- (enforced by the typed writer), each with up to two pending candidate hashes.
CREATE TABLE IF NOT EXISTS managed_regression_base (
  singleton INTEGER PRIMARY KEY CHECK(singleton=1) REFERENCES managed_repo(singleton),
  base_commit TEXT NOT NULL CHECK(length(base_commit) IN (40,64) AND base_commit NOT GLOB '*[^0-9a-f]*')
);
CREATE TABLE IF NOT EXISTS managed_regression_incidents (
  context_sha256 TEXT PRIMARY KEY NOT NULL CHECK(length(context_sha256)=64 AND context_sha256 NOT GLOB '*[^0-9a-f]*'),
  opened_at_ms INTEGER CHECK(opened_at_ms>=0),
  first_sha256 TEXT CHECK(length(first_sha256)=64 AND first_sha256 NOT GLOB '*[^0-9a-f]*'),
  second_sha256 TEXT CHECK(length(second_sha256)=64 AND second_sha256 NOT GLOB '*[^0-9a-f]*' AND second_sha256<>first_sha256),
  emitted_event_seq INTEGER UNIQUE REFERENCES events(event_seq),
  CHECK (
    (emitted_event_seq IS NULL AND opened_at_ms IS NOT NULL AND first_sha256 IS NOT NULL) OR
    (emitted_event_seq IS NOT NULL AND opened_at_ms IS NULL AND first_sha256 IS NULL AND second_sha256 IS NULL)
  )
);
CREATE TABLE IF NOT EXISTS promotion_intent (
  singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
  repo TEXT NOT NULL,
  claim_id TEXT NOT NULL REFERENCES claims(claim_id),
  base_commit TEXT NOT NULL,
  new_commit TEXT NOT NULL,
  file_path TEXT NOT NULL,
  target_decl TEXT NOT NULL,
  proof_body TEXT NOT NULL
);
-- Single in-flight cross-resource ADR transaction; not an event and not an approval by itself.
CREATE TABLE IF NOT EXISTS pivot_resume_intent (
  singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
  adr_id TEXT NOT NULL,
  plan_json TEXT NOT NULL,
  new_commit TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS recorded_pivots (
  adr_id TEXT PRIMARY KEY,
  plan_json TEXT NOT NULL,
  git_commit TEXT NOT NULL UNIQUE,
  event_seq INTEGER NOT NULL UNIQUE REFERENCES events(event_seq),
  reviewed_through INTEGER NOT NULL REFERENCES events(event_seq)
);
-- One approved whole-batch native probe plan at a time per breaker. Old
-- plans remain audit history; a fresh approval supersedes them by event order.
CREATE TABLE IF NOT EXISTS breaker_probe_batches (
  approval_id TEXT PRIMARY KEY,
  breaker_id TEXT NOT NULL REFERENCES circuit_breakers(breaker_id),
  adr_id TEXT NOT NULL REFERENCES recorded_pivots(adr_id),
  plan_json TEXT NOT NULL,
  arm_event_seq INTEGER NOT NULL UNIQUE REFERENCES events(event_seq),
  required INTEGER NOT NULL CHECK (required BETWEEN 1 AND 2),
  attempts INTEGER NOT NULL DEFAULT 0 CHECK (attempts BETWEEN 0 AND 8),
  status TEXT NOT NULL CHECK(status IN ('READY','RUNNING','NEEDS_PROBE','PASSED')),
  receipt_sha256 TEXT,
  CHECK ((status = 'PASSED') = (receipt_sha256 IS NOT NULL))
);
CREATE INDEX IF NOT EXISTS idx_breaker_probe_latest ON breaker_probe_batches(breaker_id,arm_event_seq DESC);
CREATE TABLE IF NOT EXISTS recorded_promotions (
  git_commit TEXT PRIMARY KEY,
  base_commit TEXT NOT NULL,
  claim_id TEXT NOT NULL REFERENCES claims(claim_id),
  receipt_id TEXT NOT NULL UNIQUE REFERENCES verification_receipts(receipt_id),
  event_seq INTEGER NOT NULL UNIQUE REFERENCES events(event_seq)
);

-- Human-reviewed Lean spec prefixes are immutable per Claim. A binding adds no trust.
CREATE TABLE IF NOT EXISTS reviewed_lean_specs (
  claim_id TEXT PRIMARY KEY NOT NULL REFERENCES claims(claim_id),
  repo TEXT NOT NULL,
  approved_git_commit TEXT NOT NULL CHECK(length(approved_git_commit) IN (40,64) AND approved_git_commit NOT GLOB '*[^0-9a-f]*'),
  spec_file TEXT NOT NULL,
  spec_decl TEXT NOT NULL,
  spec_prefix_sha256 TEXT NOT NULL CHECK(length(spec_prefix_sha256)=64 AND spec_prefix_sha256 NOT GLOB '*[^0-9a-f]*')
);
CREATE TRIGGER IF NOT EXISTS reviewed_lean_specs_no_update BEFORE UPDATE ON reviewed_lean_specs
BEGIN SELECT RAISE(ABORT, 'reviewed Lean specs are immutable'); END;
CREATE TRIGGER IF NOT EXISTS reviewed_lean_specs_no_delete BEFORE DELETE ON reviewed_lean_specs
BEGIN SELECT RAISE(ABORT, 'reviewed Lean specs are immutable'); END;
CREATE TRIGGER IF NOT EXISTS reviewed_lean_specs_no_replace BEFORE INSERT ON reviewed_lean_specs
WHEN EXISTS (SELECT 1 FROM reviewed_lean_specs WHERE claim_id = NEW.claim_id)
BEGIN SELECT RAISE(ABORT, 'reviewed Lean specs are immutable'); END;

-- Recorded in the same transaction as its promotion. A journal is not comparison evidence.
CREATE TABLE IF NOT EXISTS lean_spec_comparisons (
  git_commit TEXT PRIMARY KEY NOT NULL REFERENCES recorded_promotions(git_commit),
  spec_file TEXT NOT NULL,
  spec_decl TEXT NOT NULL,
  spec_prefix_sha256 TEXT NOT NULL CHECK(length(spec_prefix_sha256)=64 AND spec_prefix_sha256 NOT GLOB '*[^0-9a-f]*'),
  candidate_receipt_sha256 TEXT NOT NULL REFERENCES verification_receipts(receipt_sha256),
  receipt_sha256 TEXT NOT NULL CHECK(length(receipt_sha256)=64 AND receipt_sha256 NOT GLOB '*[^0-9a-f]*')
);
CREATE TRIGGER IF NOT EXISTS lean_spec_comparisons_no_update BEFORE UPDATE ON lean_spec_comparisons
BEGIN SELECT RAISE(ABORT, 'Lean comparison evidence is immutable'); END;
CREATE TRIGGER IF NOT EXISTS lean_spec_comparisons_no_delete BEFORE DELETE ON lean_spec_comparisons
BEGIN SELECT RAISE(ABORT, 'Lean comparison evidence is immutable'); END;
CREATE TRIGGER IF NOT EXISTS lean_spec_comparisons_no_replace BEFORE INSERT ON lean_spec_comparisons
WHEN EXISTS (SELECT 1 FROM lean_spec_comparisons WHERE git_commit = NEW.git_commit)
BEGIN SELECT RAISE(ABORT, 'Lean comparison evidence is immutable'); END;

-- Applied operator plans pin taint to an observed Git snapshot and ledger generation.
CREATE TABLE IF NOT EXISTS applied_t3_plans (
  plan_sha256 TEXT PRIMARY KEY,
  base_commit TEXT NOT NULL,
  reviewed_commit TEXT NOT NULL,
  anchor_module TEXT NOT NULL,
  preview_event_seq INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_applied_t3_reviewed_commit ON applied_t3_plans(reviewed_commit);

-- Operator sweep navigation only, never evidence or an event. A missing row is
-- not started; a NULL continuation in an existing row latches completion.
CREATE TABLE IF NOT EXISTS t3_sweep_progress (
  plan_sha256 TEXT PRIMARY KEY NOT NULL REFERENCES applied_t3_plans(plan_sha256),
  next_after_claim TEXT,
  checkpoint_event_seq INTEGER NOT NULL REFERENCES events(event_seq),
  revision INTEGER NOT NULL CHECK(revision > 0)
) STRICT;

-- Sealed reviewed Lean T3 evidence, written atomically with the new proof receipt and trust restoration.
CREATE TABLE IF NOT EXISTS lean_spec_rechecks (
  claim_id TEXT NOT NULL REFERENCES reviewed_lean_specs(claim_id),
  plan_sha256 TEXT NOT NULL REFERENCES applied_t3_plans(plan_sha256),
  reviewed_commit TEXT NOT NULL,
  promoted_commit TEXT NOT NULL REFERENCES lean_spec_comparisons(git_commit),
  spec_file TEXT NOT NULL,
  spec_decl TEXT NOT NULL,
  spec_prefix_sha256 TEXT NOT NULL CHECK(length(spec_prefix_sha256)=64 AND spec_prefix_sha256 NOT GLOB '*[^0-9a-f]*'),
  candidate_file TEXT NOT NULL,
  candidate_decl TEXT NOT NULL,
  candidate_source_sha256 TEXT NOT NULL CHECK(length(candidate_source_sha256)=64 AND candidate_source_sha256 NOT GLOB '*[^0-9a-f]*'),
  candidate_receipt_sha256 TEXT NOT NULL REFERENCES verification_receipts(receipt_sha256),
  comparison_receipt_sha256 TEXT NOT NULL CHECK(length(comparison_receipt_sha256)=64 AND comparison_receipt_sha256 NOT GLOB '*[^0-9a-f]*'),
  PRIMARY KEY (claim_id, plan_sha256)
);
CREATE TRIGGER IF NOT EXISTS lean_spec_rechecks_no_update BEFORE UPDATE ON lean_spec_rechecks
BEGIN SELECT RAISE(ABORT, 'Lean recheck evidence is immutable'); END;
CREATE TRIGGER IF NOT EXISTS lean_spec_rechecks_no_delete BEFORE DELETE ON lean_spec_rechecks
BEGIN SELECT RAISE(ABORT, 'Lean recheck evidence is immutable'); END;
CREATE TRIGGER IF NOT EXISTS lean_spec_rechecks_no_replace BEFORE INSERT ON lean_spec_rechecks
WHEN EXISTS (SELECT 1 FROM lean_spec_rechecks WHERE claim_id=NEW.claim_id AND plan_sha256=NEW.plan_sha256)
BEGIN SELECT RAISE(ABORT, 'Lean recheck evidence is immutable'); END;

-- Refutable claims bind to a human-reviewed Git context; names alone are not statement identity.
CREATE TABLE IF NOT EXISTS witness_spec_bindings (
  claim_id TEXT PRIMARY KEY REFERENCES claims(claim_id),
  repo TEXT NOT NULL,
  commit_id TEXT NOT NULL,
  file_path TEXT NOT NULL,
  proposition_name TEXT NOT NULL
);

-- Kernel-verified exact counterexamples; the matching Markdown archive is written before this transaction commits.
CREATE TABLE IF NOT EXISTS verified_refutations (
  claim_id TEXT PRIMARY KEY REFERENCES claims(claim_id),
  wiki_id TEXT NOT NULL UNIQUE,
  witness_sha256 TEXT NOT NULL,
  commit_id TEXT NOT NULL,
  file_path TEXT NOT NULL,
  proposition_name TEXT NOT NULL,
  source_sha256 TEXT NOT NULL,
  proof_body TEXT NOT NULL CHECK(length(CAST(proof_body AS BLOB))<=262144),
  receipt_json TEXT NOT NULL,
  event_seq INTEGER NOT NULL UNIQUE REFERENCES events(event_seq)
);

-- Immutable replay material for managed promotions. It is never exposed via the daemon.
CREATE TABLE IF NOT EXISTS managed_proof_recipes (
  git_commit TEXT PRIMARY KEY REFERENCES recorded_promotions(git_commit),
  file_path TEXT NOT NULL,
  target_decl TEXT NOT NULL,
  proof_body TEXT NOT NULL CHECK(length(CAST(proof_body AS BLOB))<=262144)
);

-- A native Lean sediment replay is advisory readback; digest fields deliberately
-- do not reference or mint verification receipts and confer no current trust.
CREATE TABLE IF NOT EXISTS sediment_replays (
  id TEXT PRIMARY KEY NOT NULL CHECK(length(id)=64 AND id NOT GLOB '*[^0-9a-f]*'),
  event_seq INTEGER NOT NULL UNIQUE REFERENCES events(event_seq),
  repo TEXT NOT NULL CHECK(length(repo)>0),
  archive_id TEXT NOT NULL CHECK(length(archive_id)=68 AND substr(archive_id,1,4)='SED-'
    AND substr(archive_id,5) NOT GLOB '*[^0-9a-f]*'),
  claim_id TEXT NOT NULL REFERENCES claims(claim_id),
  promoted_commit TEXT NOT NULL REFERENCES recorded_promotions(git_commit)
    CHECK(length(promoted_commit) IN (40,64) AND promoted_commit NOT GLOB '*[^0-9a-f]*'),
  reviewed_commit TEXT NOT NULL
    CHECK(length(reviewed_commit)=length(promoted_commit) AND reviewed_commit NOT GLOB '*[^0-9a-f]*'),
  expected_event_seq INTEGER NOT NULL CHECK(expected_event_seq>=0 AND expected_event_seq<event_seq),
  candidate_receipt_sha256 TEXT NOT NULL CHECK(length(candidate_receipt_sha256)=64 AND candidate_receipt_sha256 NOT GLOB '*[^0-9a-f]*'),
  comparison_receipt_sha256 TEXT CHECK(length(comparison_receipt_sha256)=64 AND comparison_receipt_sha256 NOT GLOB '*[^0-9a-f]*')
) STRICT;
CREATE TRIGGER IF NOT EXISTS sediment_replays_no_update BEFORE UPDATE ON sediment_replays
BEGIN SELECT RAISE(ABORT, 'sediment replay evidence is immutable'); END;
CREATE TRIGGER IF NOT EXISTS sediment_replays_no_delete BEFORE DELETE ON sediment_replays
BEGIN SELECT RAISE(ABORT, 'sediment replay evidence is immutable'); END;
CREATE TRIGGER IF NOT EXISTS sediment_replays_no_replace BEFORE INSERT ON sediment_replays
WHEN EXISTS (SELECT 1 FROM sediment_replays WHERE id=NEW.id OR event_seq=NEW.event_seq)
BEGIN SELECT RAISE(ABORT, 'sediment replay evidence is immutable'); END;

-- Explicit operator approval of a historical adaptation input. This record is
-- advisory only; it does not authorize dispatch or create a proof receipt.
CREATE TABLE IF NOT EXISTS sediment_adaptation_approvals (
  approval_sha256 TEXT PRIMARY KEY NOT NULL CHECK(length(approval_sha256)=64 AND approval_sha256 NOT GLOB '*[^0-9a-f]*'),
  event_seq INTEGER NOT NULL UNIQUE REFERENCES events(event_seq),
  archive_id TEXT NOT NULL CHECK(length(archive_id)=68 AND substr(archive_id,1,4)='SED-'
    AND substr(archive_id,5) NOT GLOB '*[^0-9a-f]*'),
  origin_claim TEXT NOT NULL REFERENCES reviewed_lean_specs(claim_id)
    CHECK(length(CAST(origin_claim AS BLOB)) BETWEEN 1 AND 256),
  promoted_commit TEXT NOT NULL REFERENCES lean_spec_comparisons(git_commit)
    CHECK(length(promoted_commit) IN (40,64) AND promoted_commit NOT GLOB '*[^0-9a-f]*'),
  target_claim TEXT NOT NULL REFERENCES reviewed_lean_specs(claim_id)
    CHECK(length(CAST(target_claim AS BLOB)) BETWEEN 1 AND 256 AND target_claim<>origin_claim),
  reviewed_commit TEXT NOT NULL
    CHECK(length(reviewed_commit)=length(promoted_commit) AND reviewed_commit NOT GLOB '*[^0-9a-f]*'),
  repo TEXT NOT NULL CHECK(length(CAST(repo AS BLOB)) BETWEEN 1 AND 4096),
  target_module TEXT NOT NULL CHECK(length(CAST(target_module AS BLOB)) BETWEEN 1 AND 256),
  target_decl TEXT NOT NULL CHECK(length(CAST(target_decl AS BLOB)) BETWEEN 1 AND 256),
  frozen_source_sha256 TEXT NOT NULL CHECK(length(frozen_source_sha256)=64 AND frozen_source_sha256 NOT GLOB '*[^0-9a-f]*'),
  spec_file TEXT NOT NULL CHECK(length(CAST(spec_file AS BLOB)) BETWEEN 1 AND 256),
  spec_decl TEXT NOT NULL CHECK(length(CAST(spec_decl AS BLOB)) BETWEEN 1 AND 256),
  spec_prefix_sha256 TEXT NOT NULL CHECK(length(spec_prefix_sha256)=64 AND spec_prefix_sha256 NOT GLOB '*[^0-9a-f]*'),
  expected_event_seq INTEGER NOT NULL CHECK(expected_event_seq>=0 AND expected_event_seq<event_seq)
) STRICT;
CREATE TRIGGER IF NOT EXISTS sediment_adaptation_approvals_no_update BEFORE UPDATE ON sediment_adaptation_approvals
BEGIN SELECT RAISE(ABORT, 'sediment adaptation approvals are immutable'); END;
CREATE TRIGGER IF NOT EXISTS sediment_adaptation_approvals_no_delete BEFORE DELETE ON sediment_adaptation_approvals
BEGIN SELECT RAISE(ABORT, 'sediment adaptation approvals are immutable'); END;
CREATE TRIGGER IF NOT EXISTS sediment_adaptation_approvals_no_replace BEFORE INSERT ON sediment_adaptation_approvals
WHEN EXISTS (SELECT 1 FROM sediment_adaptation_approvals
             WHERE approval_sha256=NEW.approval_sha256 OR event_seq=NEW.event_seq)
BEGIN SELECT RAISE(ABORT, 'sediment adaptation approvals are immutable'); END;

-- A scoped scheduler row can be leased only by explicitly citing its immutable
-- approval. Ordinary scheduling never discovers this row after enrollment.
CREATE TABLE IF NOT EXISTS scheduler_sediment_adaptations (
  task_seq INTEGER PRIMARY KEY REFERENCES scheduler_tasks(queue_seq),
  approval_sha256 TEXT NOT NULL UNIQUE REFERENCES sediment_adaptation_approvals(approval_sha256),
  registered_event_seq INTEGER NOT NULL UNIQUE REFERENCES events(event_seq)
) STRICT;
CREATE TRIGGER IF NOT EXISTS scheduler_sediment_adaptations_no_update BEFORE UPDATE ON scheduler_sediment_adaptations
BEGIN SELECT RAISE(ABORT, 'sediment adaptation scheduler links are immutable'); END;
CREATE TRIGGER IF NOT EXISTS scheduler_sediment_adaptations_no_delete BEFORE DELETE ON scheduler_sediment_adaptations
BEGIN SELECT RAISE(ABORT, 'sediment adaptation scheduler links are immutable'); END;
CREATE TRIGGER IF NOT EXISTS scheduler_sediment_adaptations_no_replace BEFORE INSERT ON scheduler_sediment_adaptations
WHEN EXISTS (SELECT 1 FROM scheduler_sediment_adaptations
             WHERE task_seq=NEW.task_seq OR approval_sha256=NEW.approval_sha256
               OR registered_event_seq=NEW.registered_event_seq)
BEGIN SELECT RAISE(ABORT, 'sediment adaptation scheduler links are immutable'); END;

-- Historical edge only: this does not create a proof, Wiki entry, or new scheduler state.
CREATE TABLE IF NOT EXISTS sediment_adaptation_lineage (
  approval_sha256 TEXT PRIMARY KEY NOT NULL REFERENCES sediment_adaptation_approvals(approval_sha256),
  task_seq INTEGER NOT NULL REFERENCES scheduler_sediment_adaptations(task_seq)
    REFERENCES scheduler_task_candidates(task_seq),
  promoted_commit TEXT NOT NULL UNIQUE REFERENCES lean_spec_comparisons(git_commit)
    CHECK(length(promoted_commit) IN (40,64) AND promoted_commit NOT GLOB '*[^0-9a-f]*'),
  origin_t1_receipt_sha256 TEXT NOT NULL REFERENCES verification_receipts(receipt_sha256),
  origin_comparison_receipt_sha256 TEXT NOT NULL
    CHECK(length(origin_comparison_receipt_sha256)=64 AND origin_comparison_receipt_sha256 NOT GLOB '*[^0-9a-f]*'),
  candidate_receipt_sha256 TEXT NOT NULL REFERENCES verification_receipts(receipt_sha256),
  comparison_receipt_sha256 TEXT NOT NULL
    CHECK(length(comparison_receipt_sha256)=64 AND comparison_receipt_sha256 NOT GLOB '*[^0-9a-f]*'),
  expected_event_seq INTEGER NOT NULL CHECK(expected_event_seq>=0 AND expected_event_seq<event_seq),
  event_seq INTEGER NOT NULL UNIQUE REFERENCES events(event_seq)
) STRICT;
CREATE TRIGGER IF NOT EXISTS sediment_adaptation_lineage_no_update BEFORE UPDATE ON sediment_adaptation_lineage
BEGIN SELECT RAISE(ABORT, 'sediment adaptation lineage is immutable'); END;
CREATE TRIGGER IF NOT EXISTS sediment_adaptation_lineage_no_delete BEFORE DELETE ON sediment_adaptation_lineage
BEGIN SELECT RAISE(ABORT, 'sediment adaptation lineage is immutable'); END;
CREATE TRIGGER IF NOT EXISTS sediment_adaptation_lineage_no_replace BEFORE INSERT ON sediment_adaptation_lineage
WHEN EXISTS (SELECT 1 FROM sediment_adaptation_lineage
             WHERE approval_sha256=NEW.approval_sha256 OR event_seq=NEW.event_seq
                OR promoted_commit=NEW.promoted_commit)
BEGIN SELECT RAISE(ABORT, 'sediment adaptation lineage is immutable'); END;

-- Immutable advisory observations tied to the reviewed Claim snapshot, not to
-- its current row. No trigger may turn this evidence into a trust transition.
CREATE TABLE IF NOT EXISTS oracle_observations (
  id TEXT PRIMARY KEY NOT NULL CHECK(length(id)=64 AND id NOT GLOB '*[^0-9a-f]*'),
  approval_sha256 TEXT NOT NULL CHECK(length(approval_sha256)=64 AND approval_sha256 NOT GLOB '*[^0-9a-f]*'),
  claim_id TEXT NOT NULL CHECK(length(CAST(claim_id AS BLOB)) BETWEEN 1 AND 256),
  git_commit TEXT NOT NULL CHECK(length(git_commit) IN (40,64) AND git_commit NOT GLOB '*[^0-9a-f]*'),
  repo TEXT NOT NULL CHECK(length(CAST(repo AS BLOB)) BETWEEN 1 AND 4096),
  engine TEXT NOT NULL CHECK(engine IN ('lean4','agda','rocq')),
  formal_target TEXT NOT NULL CHECK(length(CAST(formal_target AS BLOB)) BETWEEN 1 AND 513),
  informal_statement TEXT NOT NULL CHECK(length(CAST(informal_statement AS BLOB)) BETWEEN 1 AND 16384),
  query_sha256 TEXT NOT NULL CHECK(length(query_sha256)=64 AND query_sha256 NOT GLOB '*[^0-9a-f]*'),
  z3_sha256 TEXT NOT NULL CHECK(length(z3_sha256)=64 AND z3_sha256 NOT GLOB '*[^0-9a-f]*'),
  cvc5_sha256 TEXT NOT NULL CHECK(length(cvc5_sha256)=64 AND cvc5_sha256 NOT GLOB '*[^0-9a-f]*'),
  observation_sha256 TEXT NOT NULL CHECK(length(observation_sha256)=64 AND observation_sha256 NOT GLOB '*[^0-9a-f]*'),
  observation_json TEXT NOT NULL CHECK(length(CAST(observation_json AS BLOB)) BETWEEN 1 AND 131072 AND json_valid(observation_json)),
  event_seq INTEGER NOT NULL UNIQUE REFERENCES events(event_seq),
  UNIQUE (approval_sha256, observation_sha256)
);
CREATE TRIGGER IF NOT EXISTS oracle_observations_no_update BEFORE UPDATE ON oracle_observations
BEGIN SELECT RAISE(ABORT, 'Oracle observations are immutable'); END;
CREATE TRIGGER IF NOT EXISTS oracle_observations_no_delete BEFORE DELETE ON oracle_observations
BEGIN SELECT RAISE(ABORT, 'Oracle observations are immutable'); END;
CREATE TRIGGER IF NOT EXISTS oracle_observations_no_replace BEFORE INSERT ON oracle_observations
WHEN EXISTS (SELECT 1 FROM oracle_observations
  WHERE id=NEW.id OR event_seq=NEW.event_seq
    OR (approval_sha256=NEW.approval_sha256 AND observation_sha256=NEW.observation_sha256))
BEGIN SELECT RAISE(ABORT, 'Oracle observations are immutable'); END;

CREATE TRIGGER IF NOT EXISTS events_no_replace BEFORE INSERT ON events
WHEN EXISTS (SELECT 1 FROM events WHERE event_seq = NEW.event_seq)
BEGIN SELECT RAISE(ABORT, 'events are append-only'); END;
