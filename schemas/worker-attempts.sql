-- Durable RPC worker attempts. A request ID owns at most one scheduler lease;
-- private fences remain in scheduler_tasks/task_scheduler_control and are never
-- serialized to agents. Candidate JSON is untrusted and carries no proof trust.
CREATE TABLE IF NOT EXISTS worker_attempts (
  attempt_id TEXT PRIMARY KEY CHECK(length(attempt_id)=67 AND attempt_id GLOB 'wa-[0-9a-f]*'),
  request_id TEXT NOT NULL UNIQUE CHECK(length(request_id) BETWEEN 1 AND 128),
  request_sha256 TEXT NOT NULL CHECK(length(request_sha256)=64 AND request_sha256 NOT GLOB '*[^0-9a-f]*'),
  principal TEXT NOT NULL CHECK(length(principal) BETWEEN 1 AND 161),
  role TEXT NOT NULL CHECK(role IN ('prover_worker','independent_reviewer')),
  task_seq INTEGER NOT NULL REFERENCES scheduler_tasks(queue_seq),
  task_id TEXT NOT NULL CHECK(length(task_id) BETWEEN 1 AND 100),
  claim_id TEXT NOT NULL CHECK(length(claim_id) BETWEEN 1 AND 100),
  base_commit TEXT NOT NULL CHECK(length(base_commit) IN (40,64)),
  wall_seconds INTEGER NOT NULL CHECK(wall_seconds BETWEEN 10 AND 600),
  expires_at_ms INTEGER NOT NULL CHECK(expires_at_ms >= 0),
  lease_fence INTEGER NOT NULL CHECK(lease_fence > 0),
  status TEXT NOT NULL CHECK(status IN ('RUNNING','SUBMITTED','CANCELLED')),
  suggestion_sha256 TEXT CHECK(length(suggestion_sha256)=64 AND suggestion_sha256 NOT GLOB '*[^0-9a-f]*'),
  result_json TEXT CHECK(length(result_json) <= 32768 AND json_valid(result_json)),
  event_seq INTEGER REFERENCES events(event_seq),
  created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
  updated_at_ms INTEGER NOT NULL CHECK(updated_at_ms >= created_at_ms),
  CHECK (
    (status='RUNNING' AND suggestion_sha256 IS NULL AND result_json IS NULL AND event_seq IS NULL)
    OR (status='SUBMITTED' AND suggestion_sha256 IS NOT NULL AND result_json IS NOT NULL AND event_seq IS NOT NULL)
    OR (status='CANCELLED' AND suggestion_sha256 IS NULL AND result_json IS NULL AND event_seq IS NOT NULL)
  )
) STRICT;

CREATE INDEX IF NOT EXISTS worker_attempts_task_idx ON worker_attempts(task_seq,status);
