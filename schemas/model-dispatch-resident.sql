-- State migration 5. Scope/order index for bounded ready and restart scans.
CREATE INDEX IF NOT EXISTS idx_model_dispatch_scope_state_event
ON scheduler_model_dispatch_approvals(repo,model,state,event_seq);

-- Preserve v4's immutable inputs and strictly forward event/state transitions.
-- Running cancellation additionally requires a revoked lease and no durable
-- outcome. Both changes are made in a single Store writer transaction.
DROP TRIGGER model_dispatch_approval_advance_only;
CREATE TRIGGER model_dispatch_approval_advance_only
BEFORE UPDATE ON scheduler_model_dispatch_approvals
WHEN NEW.task_id IS NOT OLD.task_id OR NEW.claim_id IS NOT OLD.claim_id
  OR NEW.role IS NOT OLD.role OR NEW.repo IS NOT OLD.repo
  OR NEW.base_commit IS NOT OLD.base_commit OR NEW.model IS NOT OLD.model
  OR NEW.max_wall_seconds IS NOT OLD.max_wall_seconds
  OR NEW.max_model_calls IS NOT OLD.max_model_calls OR NEW.event_seq<=OLD.event_seq
  OR EXISTS (SELECT 1 FROM scheduler_model_dispatch_approvals WHERE event_seq=NEW.event_seq)
  OR NOT ((OLD.state='APPROVED' AND NEW.state IN ('DISPATCHED', 'CANCELLED'))
          OR (OLD.state='DISPATCHED' AND NEW.state='RECORDED')
          OR (OLD.state='DISPATCHED' AND NEW.state='CANCELLED' AND EXISTS (
              SELECT 1 FROM scheduler_tasks t WHERE t.task_id=OLD.task_id
                AND t.completed_at_ms IS NULL AND t.lease_expires_at_ms IS NULL
                AND NOT EXISTS (SELECT 1 FROM scheduler_model_task_candidates m WHERE m.task_seq=t.queue_seq)
                AND NOT EXISTS (SELECT 1 FROM scheduler_task_candidates n WHERE n.task_seq=t.queue_seq))))
BEGIN SELECT RAISE(ABORT, 'invalid model dispatch approval transition'); END;
