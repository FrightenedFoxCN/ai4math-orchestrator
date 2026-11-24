-- Durable metadata, leases, dependencies and untrusted results for ordinary
-- coordinator tasks. Loaded by StateStore with foreign_keys enabled.
CREATE TABLE IF NOT EXISTS scheduler_tasks (
  queue_seq INTEGER PRIMARY KEY AUTOINCREMENT,
  task_id TEXT NOT NULL UNIQUE CHECK (length(CAST(task_id AS BLOB)) BETWEEN 1 AND 100),
  track TEXT NOT NULL CHECK (track IN ('exploration', 'infrastructure')),
  engine TEXT NOT NULL CHECK (engine IN ('lean4', 'agda', 'rocq', 'smt', 'atp', 'cas', 'informal')),
  module_path TEXT NOT NULL CHECK (length(CAST(module_path AS BLOB)) BETWEEN 1 AND 256),
  target TEXT NOT NULL CHECK (length(CAST(target AS BLOB)) BETWEEN 1 AND 256),
  last_fence INTEGER NOT NULL DEFAULT 0 CHECK (last_fence >= 0),
  lease_expires_at_ms INTEGER,
  completed_at_ms INTEGER,
  CHECK (lease_expires_at_ms IS NULL OR (completed_at_ms IS NULL AND last_fence > 0)),
  CHECK (completed_at_ms IS NULL OR last_fence > 0)
) STRICT;

-- At most one untrusted candidate per enrolled task; not a proof or receipt.
-- Completion (including its timestamp) and insertion share one transaction.
CREATE TABLE IF NOT EXISTS scheduler_task_candidates (
  task_seq INTEGER PRIMARY KEY REFERENCES scheduler_tasks(queue_seq),
  base_commit TEXT NOT NULL CHECK (length(CAST(base_commit AS BLOB)) IN (40, 64)),
  claim_id TEXT NOT NULL CHECK (length(CAST(claim_id AS BLOB)) BETWEEN 1 AND 100),
  role TEXT NOT NULL CHECK (length(CAST(role AS BLOB)) BETWEEN 1 AND 64),
  executable_sha256 TEXT NOT NULL CHECK (length(CAST(executable_sha256 AS BLOB)) = 64),
  result_json TEXT NOT NULL CHECK (length(CAST(result_json AS BLOB)) <= 32768 AND json_valid(result_json))
) STRICT;

-- Additive migration for ledgers with native results. Model output has no
-- executable digest. The shared terminal lease fence prevents a task from
-- getting a candidate in both tables.
CREATE TABLE IF NOT EXISTS scheduler_model_task_candidates (
  task_seq INTEGER PRIMARY KEY REFERENCES scheduler_tasks(queue_seq),
  base_commit TEXT NOT NULL CHECK (length(CAST(base_commit AS BLOB)) IN (40, 64)),
  claim_id TEXT NOT NULL CHECK (length(CAST(claim_id AS BLOB)) BETWEEN 1 AND 100),
  role TEXT NOT NULL CHECK (role IN ('prover_worker', 'independent_reviewer')),
  result_json TEXT NOT NULL CHECK (length(CAST(result_json AS BLOB)) <= 32768 AND json_valid(result_json))
) STRICT;

-- Enrolling a task adds immutable edges to previously enrolled queue positions.
-- Strictly decreasing queue positions make a dependency cycle impossible.
CREATE TABLE IF NOT EXISTS scheduler_task_dependencies (
  task_seq INTEGER NOT NULL REFERENCES scheduler_tasks(queue_seq),
  dep_seq INTEGER NOT NULL REFERENCES scheduler_tasks(queue_seq),
  PRIMARY KEY (task_seq, dep_seq),
  CHECK (dep_seq < task_seq)
) STRICT;

CREATE TABLE IF NOT EXISTS task_scheduler_control (
  singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
  scope_id BLOB NOT NULL CHECK (length(scope_id) = 16),
  last_track TEXT CHECK (last_track IN ('exploration', 'infrastructure')),
  last_fence INTEGER NOT NULL DEFAULT 0 CHECK (last_fence >= 0)
) STRICT;

INSERT OR IGNORE INTO task_scheduler_control(singleton, scope_id) VALUES (1, randomblob(16));

-- No row means neither a pending debounce nor a latched episode. The pending
-- timestamp and ledger watermark are replaced together, then atomically swapped
-- for the emitted event reference when starvation is confirmed.
CREATE TABLE IF NOT EXISTS scheduler_starvation (
  singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
  pending_since_ms INTEGER CHECK (pending_since_ms >= 0),
  pending_ledger_seq INTEGER CHECK (pending_ledger_seq >= 0),
  emitted_event_seq INTEGER REFERENCES events(event_seq),
  CHECK ((pending_since_ms IS NOT NULL) = (pending_ledger_seq IS NOT NULL)),
  CHECK ((pending_since_ms IS NOT NULL) <> (emitted_event_seq IS NOT NULL))
) STRICT;

CREATE INDEX IF NOT EXISTS scheduler_live_leases_idx
  ON scheduler_tasks (lease_expires_at_ms, track) WHERE lease_expires_at_ms IS NOT NULL;
CREATE INDEX IF NOT EXISTS scheduler_breaker_module_idx
  ON circuit_breakers (engine, module_path) WHERE state <> 'CLOSED';

-- One scheduling hold generation per task/observation. History is immutable
-- except for the single early-release timestamp; expiry needs no write.
CREATE TABLE IF NOT EXISTS scheduler_oracle_holds (
  id TEXT PRIMARY KEY NOT NULL CHECK(length(id)=64 AND id NOT GLOB '*[^0-9a-f]*'),
  task_seq INTEGER NOT NULL REFERENCES scheduler_tasks(queue_seq),
  observation_id TEXT NOT NULL REFERENCES oracle_observations(id),
  created_at_ms INTEGER NOT NULL CHECK(created_at_ms>=0),
  expires_at_ms INTEGER NOT NULL CHECK(expires_at_ms-created_at_ms BETWEEN 1000 AND 3600000),
  released_at_ms INTEGER CHECK(released_at_ms>=created_at_ms AND released_at_ms<expires_at_ms),
  event_seq INTEGER NOT NULL UNIQUE REFERENCES events(event_seq),
  UNIQUE(task_seq, observation_id)
) STRICT;
CREATE INDEX IF NOT EXISTS scheduler_oracle_holds_latest
  ON scheduler_oracle_holds (task_seq, event_seq DESC);
CREATE TRIGGER IF NOT EXISTS scheduler_oracle_holds_no_rewrite BEFORE UPDATE ON scheduler_oracle_holds
WHEN NEW.id IS NOT OLD.id OR NEW.task_seq IS NOT OLD.task_seq
  OR NEW.observation_id IS NOT OLD.observation_id OR NEW.created_at_ms IS NOT OLD.created_at_ms
  OR NEW.expires_at_ms IS NOT OLD.expires_at_ms OR NEW.event_seq IS NOT OLD.event_seq
  OR OLD.released_at_ms IS NOT NULL OR NEW.released_at_ms IS NULL
BEGIN SELECT RAISE(ABORT, 'only an early release can update an Oracle task hold'); END;
CREATE TRIGGER IF NOT EXISTS scheduler_oracle_holds_no_delete BEFORE DELETE ON scheduler_oracle_holds
BEGIN SELECT RAISE(ABORT, 'Oracle task hold history is immutable'); END;
CREATE TRIGGER IF NOT EXISTS scheduler_oracle_holds_no_replace BEFORE INSERT ON scheduler_oracle_holds
WHEN NEW.released_at_ms IS NOT NULL OR EXISTS (SELECT 1 FROM scheduler_oracle_holds h
  WHERE h.id=NEW.id OR h.event_seq=NEW.event_seq
    OR (h.task_seq=NEW.task_seq AND h.observation_id=NEW.observation_id))
BEGIN SELECT RAISE(ABORT, 'Oracle task hold history is immutable'); END;
