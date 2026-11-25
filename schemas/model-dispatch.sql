-- State migration 4. A task ID is never reusable for model dispatch, including
-- after cancellation. This table contains no credentials, context or output.
CREATE TABLE IF NOT EXISTS scheduler_model_dispatch_approvals (
  task_id TEXT PRIMARY KEY NOT NULL REFERENCES scheduler_tasks(task_id),
  claim_id TEXT NOT NULL REFERENCES claims(claim_id),
  role TEXT NOT NULL CHECK (role IN ('prover_worker', 'independent_reviewer')),
  repo TEXT NOT NULL CHECK (length(repo)>0),
  base_commit TEXT NOT NULL CHECK (length(base_commit) IN (40, 64) AND base_commit NOT GLOB '*[^0-9a-f]*'),
  model TEXT NOT NULL CHECK (length(model) BETWEEN 1 AND 100 AND model NOT GLOB '*[^A-Za-z0-9_.:-]*'),
  max_wall_seconds INTEGER NOT NULL CHECK (max_wall_seconds BETWEEN 1 AND 60),
  max_model_calls INTEGER NOT NULL CHECK (max_model_calls BETWEEN 1 AND 8),
  state TEXT NOT NULL CHECK (state IN ('APPROVED', 'DISPATCHED', 'CANCELLED', 'RECORDED')),
  event_seq INTEGER NOT NULL UNIQUE REFERENCES events(event_seq)
) STRICT;

CREATE TRIGGER IF NOT EXISTS model_dispatch_approval_no_replace
BEFORE INSERT ON scheduler_model_dispatch_approvals
WHEN NEW.state<>'APPROVED' OR EXISTS (
  SELECT 1 FROM scheduler_model_dispatch_approvals
  WHERE task_id=NEW.task_id OR event_seq=NEW.event_seq)
BEGIN SELECT RAISE(ABORT, 'model dispatch approval must be new'); END;

CREATE TRIGGER IF NOT EXISTS model_dispatch_approval_advance_only
BEFORE UPDATE ON scheduler_model_dispatch_approvals
WHEN NEW.task_id IS NOT OLD.task_id OR NEW.claim_id IS NOT OLD.claim_id
  OR NEW.role IS NOT OLD.role OR NEW.repo IS NOT OLD.repo
  OR NEW.base_commit IS NOT OLD.base_commit OR NEW.model IS NOT OLD.model
  OR NEW.max_wall_seconds IS NOT OLD.max_wall_seconds
  OR NEW.max_model_calls IS NOT OLD.max_model_calls OR NEW.event_seq<=OLD.event_seq
  OR EXISTS (SELECT 1 FROM scheduler_model_dispatch_approvals WHERE event_seq=NEW.event_seq)
  OR NOT ((OLD.state='APPROVED' AND NEW.state IN ('DISPATCHED', 'CANCELLED'))
          OR (OLD.state='DISPATCHED' AND NEW.state='RECORDED'))
BEGIN SELECT RAISE(ABORT, 'invalid model dispatch approval transition'); END;

CREATE TRIGGER IF NOT EXISTS model_dispatch_approval_no_delete
BEFORE DELETE ON scheduler_model_dispatch_approvals
BEGIN SELECT RAISE(ABORT, 'model dispatch approval history is immutable'); END;
