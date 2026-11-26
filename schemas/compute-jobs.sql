-- Persistent asynchronous compute job ledger for local GPU and SLURM execution (migration 8).
-- Preserves in-flight external job IDs, broker-private blinded array unblinding plans,
-- cluster quota accounting, and branch-scoped batch cancellation across daemon restarts.
CREATE TABLE compute_jobs (
  job_id TEXT NOT NULL PRIMARY KEY CHECK(
    length(job_id) BETWEEN 1 AND 128 AND job_id NOT GLOB '*[^A-Za-z0-9_.:-]*'
  ),
  prereg_id TEXT REFERENCES protocol_preregistrations(prereg_id),
  protocol_id TEXT NOT NULL CHECK(length(protocol_id) BETWEEN 1 AND 128),
  protocol_sha256 TEXT NOT NULL CHECK(
    length(protocol_sha256)=64 AND protocol_sha256 NOT GLOB '*[^0-9a-f]*'
  ),
  base_commit TEXT NOT NULL CHECK(
    length(base_commit) IN (40,64) AND base_commit NOT GLOB '*[^0-9a-f]*'
  ),
  candidate_id TEXT NOT NULL CHECK(length(candidate_id) BETWEEN 1 AND 128),
  candidate_sha256 TEXT NOT NULL CHECK(
    length(candidate_sha256)=64 AND candidate_sha256 NOT GLOB '*[^0-9a-f]*'
  ),
  target_kind TEXT NOT NULL CHECK(target_kind IN ('local_sandbox','slurm')),
  cluster_or_host_id TEXT NOT NULL CHECK(length(cluster_or_host_id) BETWEEN 1 AND 64),
  external_job_id TEXT CHECK(
    external_job_id IS NULL OR (length(external_job_id) BETWEEN 1 AND 64)
  ),
  staging_ref TEXT NOT NULL CHECK(length(staging_ref) BETWEEN 1 AND 1024),
  bundle_sha256 TEXT NOT NULL CHECK(
    length(bundle_sha256)=64 AND bundle_sha256 NOT GLOB '*[^0-9a-f]*'
  ),
  resource_request_json TEXT NOT NULL,
  requested_gpus_total INTEGER NOT NULL CHECK(requested_gpus_total >= 0),
  array_len INTEGER NOT NULL CHECK(array_len BETWEEN 1 AND 128),
  blinded_plan_json TEXT NOT NULL,
  branch_tag TEXT CHECK(branch_tag IS NULL OR (length(branch_tag) BETWEEN 1 AND 128)),
  status TEXT NOT NULL CHECK(
    status IN ('queued','running','completed','failed','timed_out','cancelled')
  ),
  detail TEXT,
  sealed_receipt_sha256 TEXT UNIQUE CHECK(
    sealed_receipt_sha256 IS NULL
    OR (length(sealed_receipt_sha256)=64 AND sealed_receipt_sha256 NOT GLOB '*[^0-9a-f]*')
  ),
  submitted_event_seq INTEGER NOT NULL REFERENCES events(event_seq),
  updated_event_seq INTEGER NOT NULL REFERENCES events(event_seq),
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
) STRICT;

CREATE INDEX idx_compute_jobs_active_cluster
  ON compute_jobs (cluster_or_host_id, status);

CREATE INDEX idx_compute_jobs_branch_status
  ON compute_jobs (branch_tag, status);

CREATE TRIGGER compute_jobs_no_delete BEFORE DELETE ON compute_jobs
BEGIN SELECT RAISE(ABORT, 'compute jobs are permanent audit records'); END;

CREATE TRIGGER compute_jobs_monotonic_transition BEFORE UPDATE ON compute_jobs
WHEN OLD.job_id <> NEW.job_id
  OR OLD.prereg_id IS NOT NEW.prereg_id
  OR OLD.protocol_id <> NEW.protocol_id
  OR OLD.protocol_sha256 <> NEW.protocol_sha256
  OR OLD.base_commit <> NEW.base_commit
  OR OLD.candidate_id <> NEW.candidate_id
  OR OLD.candidate_sha256 <> NEW.candidate_sha256
  OR OLD.target_kind <> NEW.target_kind
  OR OLD.cluster_or_host_id <> NEW.cluster_or_host_id
  OR OLD.external_job_id IS NOT NEW.external_job_id
  OR OLD.staging_ref <> NEW.staging_ref
  OR OLD.bundle_sha256 <> NEW.bundle_sha256
  OR OLD.resource_request_json <> NEW.resource_request_json
  OR OLD.requested_gpus_total <> NEW.requested_gpus_total
  OR OLD.array_len <> NEW.array_len
  OR OLD.blinded_plan_json <> NEW.blinded_plan_json
  OR OLD.branch_tag IS NOT NEW.branch_tag
  OR OLD.submitted_event_seq <> NEW.submitted_event_seq
  OR OLD.created_at <> NEW.created_at
  OR (OLD.status IN ('completed','failed','timed_out','cancelled')
      AND NEW.status IN ('queued','running'))
  OR (OLD.sealed_receipt_sha256 IS NOT NULL
      AND OLD.sealed_receipt_sha256 IS NOT NEW.sealed_receipt_sha256)
BEGIN SELECT RAISE(ABORT, 'invalid or non-monotonic compute_jobs update'); END;
