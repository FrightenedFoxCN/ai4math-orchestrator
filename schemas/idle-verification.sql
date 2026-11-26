-- Persistent idle-time literature formalization & computational replication queue (migration 10).
-- Supports both BP backward-VoI automatic tickets and explicit Human-queued tickets,
-- tracks independent idle wall/GPU usage budgets, and binds sealed verification outcomes.

CREATE TABLE idle_verification_queue (
  ticket_id TEXT NOT NULL PRIMARY KEY CHECK(
    length(ticket_id) BETWEEN 1 AND 128 AND ticket_id NOT GLOB '*[^A-Za-z0-9_.:-]*'
  ),
  source TEXT NOT NULL CHECK(source IN ('bp_voi', 'human')),
  entry_id TEXT NOT NULL CHECK(length(entry_id) BETWEEN 1 AND 128),
  epistemic_node_id TEXT REFERENCES epistemic_nodes(node_id),
  domain_id TEXT NOT NULL CHECK(
    length(domain_id) BETWEEN 1 AND 64 AND domain_id NOT GLOB '*[^A-Za-z0-9_-]*'
  ),
  reproducibility_class TEXT NOT NULL CHECK(
    reproducibility_class IN ('formalizable_in_itp', 'replicable_by_protocol')
  ),
  target_protocol_or_engine TEXT NOT NULL CHECK(length(target_protocol_or_engine) BETWEEN 1 AND 128),
  priority_score REAL NOT NULL CHECK(priority_score >= 0.0),
  human_note TEXT,
  status TEXT NOT NULL CHECK(
    status IN ('queued', 'dispatched', 'replicated', 'contradicted', 'failed', 'cancelled')
  ),
  compute_job_id TEXT REFERENCES compute_jobs(job_id),
  sealed_receipt_sha256 TEXT CHECK(
    sealed_receipt_sha256 IS NULL
    OR (length(sealed_receipt_sha256)=64 AND sealed_receipt_sha256 NOT GLOB '*[^0-9a-f]*')
  ),
  wall_seconds_spent REAL NOT NULL DEFAULT 0.0 CHECK(wall_seconds_spent >= 0.0),
  gpu_seconds_spent REAL NOT NULL DEFAULT 0.0 CHECK(gpu_seconds_spent >= 0.0),
  detail TEXT,
  created_event_seq INTEGER NOT NULL REFERENCES events(event_seq),
  updated_event_seq INTEGER NOT NULL REFERENCES events(event_seq),
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
) STRICT;

CREATE INDEX idx_idle_queue_status_priority
  ON idle_verification_queue (status, source, priority_score DESC, created_event_seq ASC);

CREATE INDEX idx_idle_queue_entry
  ON idle_verification_queue (entry_id, status);

CREATE TRIGGER idle_verification_queue_no_delete BEFORE DELETE ON idle_verification_queue
BEGIN SELECT RAISE(ABORT, 'idle verification queue records are permanent'); END;

CREATE TRIGGER idle_verification_queue_monotonic BEFORE UPDATE ON idle_verification_queue
WHEN NEW.ticket_id <> OLD.ticket_id
  OR NEW.entry_id <> OLD.entry_id
  OR NEW.domain_id <> OLD.domain_id
  OR NEW.reproducibility_class <> OLD.reproducibility_class
  OR NEW.created_event_seq <> OLD.created_event_seq
  OR NEW.created_at <> OLD.created_at
  OR (OLD.status IN ('replicated','contradicted','failed','cancelled')
      AND NEW.status IN ('queued','dispatched'))
  OR (OLD.sealed_receipt_sha256 IS NOT NULL
      AND OLD.sealed_receipt_sha256 IS NOT NEW.sealed_receipt_sha256)
BEGIN SELECT RAISE(ABORT, 'invalid or non-monotonic idle_verification_queue update'); END;
