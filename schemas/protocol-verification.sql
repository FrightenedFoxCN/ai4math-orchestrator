-- Immutable pre-registrations and append-only protocol verification trial ledger (migration 7).
-- Enforces cumulative trial budgets, protocol_sha256 binding, and zero deletion/rewriting
-- of failed trials to block silent p-hacking and survivorship bias.
CREATE TABLE protocol_preregistrations (
  prereg_id TEXT NOT NULL PRIMARY KEY CHECK(
    length(prereg_id) BETWEEN 1 AND 128 AND prereg_id NOT GLOB '*[^A-Za-z0-9_.-]*'
  ),
  prereg_sha256 TEXT NOT NULL UNIQUE CHECK(
    length(prereg_sha256)=64 AND prereg_sha256 NOT GLOB '*[^0-9a-f]*'
  ),
  hypothesis_family_id TEXT NOT NULL CHECK(
    length(hypothesis_family_id) BETWEEN 1 AND 128 AND hypothesis_family_id NOT GLOB '*[^A-Za-z0-9_.-]*'
  ),
  protocol_sha256 TEXT NOT NULL CHECK(
    length(protocol_sha256)=64 AND protocol_sha256 NOT GLOB '*[^0-9a-f]*'
  ),
  primary_metric TEXT NOT NULL CHECK(
    length(primary_metric) BETWEEN 1 AND 128 AND primary_metric NOT GLOB '*[^A-Za-z0-9_.-]*'
  ),
  max_trials INTEGER NOT NULL CHECK(max_trials BETWEEN 1 AND 100000),
  significance_alpha REAL CHECK(
    significance_alpha IS NULL OR (significance_alpha > 0.0 AND significance_alpha < 1.0)
  ),
  correction TEXT NOT NULL CHECK(correction IN ('none','bonferroni')),
  stopping_rule TEXT NOT NULL CHECK(length(stopping_rule) BETWEEN 1 AND 1024),
  spec_json TEXT NOT NULL,
  event_seq INTEGER NOT NULL REFERENCES events(event_seq),
  created_at TEXT NOT NULL
) STRICT;

CREATE TRIGGER protocol_preregistrations_no_update BEFORE UPDATE ON protocol_preregistrations
BEGIN SELECT RAISE(ABORT, 'protocol pre-registrations are immutable'); END;

CREATE TRIGGER protocol_preregistrations_no_delete BEFORE DELETE ON protocol_preregistrations
BEGIN SELECT RAISE(ABORT, 'protocol pre-registrations cannot be deleted'); END;

CREATE TABLE protocol_trials (
  prereg_id TEXT NOT NULL REFERENCES protocol_preregistrations(prereg_id),
  trial_index INTEGER NOT NULL CHECK(trial_index >= 1),
  receipt_sha256 TEXT NOT NULL UNIQUE CHECK(
    length(receipt_sha256)=64 AND receipt_sha256 NOT GLOB '*[^0-9a-f]*'
  ),
  domain_id TEXT NOT NULL CHECK(length(domain_id) BETWEEN 1 AND 64),
  backend_id TEXT NOT NULL CHECK(length(backend_id) BETWEEN 1 AND 64),
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
  rendered_artifacts_sha256 TEXT NOT NULL CHECK(
    length(rendered_artifacts_sha256)=64 AND rendered_artifacts_sha256 NOT GLOB '*[^0-9a-f]*'
  ),
  verifier_sha256 TEXT NOT NULL CHECK(
    length(verifier_sha256)=64 AND verifier_sha256 NOT GLOB '*[^0-9a-f]*'
  ),
  verdict TEXT NOT NULL CHECK(
    verdict IN ('passed','rejected','sanity_gate_failed','control_drift_detected','disagreement','verifier_error')
  ),
  epistemic_level TEXT NOT NULL CHECK(length(epistemic_level) BETWEEN 2 AND 16),
  epistemic_debts_json TEXT NOT NULL,
  control_summary_json TEXT NOT NULL,
  surrogate_passed INTEGER CHECK(surrogate_passed IS NULL OR surrogate_passed IN (0, 1)),
  raw_receipt_json TEXT NOT NULL,
  event_seq INTEGER NOT NULL REFERENCES events(event_seq),
  recorded_at TEXT NOT NULL,
  PRIMARY KEY (prereg_id, trial_index)
) STRICT;

CREATE INDEX idx_protocol_trials_protocol
  ON protocol_trials (protocol_sha256, verdict);

CREATE TRIGGER protocol_trials_no_update BEFORE UPDATE ON protocol_trials
BEGIN SELECT RAISE(ABORT, 'protocol trials are append-only'); END;

CREATE TRIGGER protocol_trials_no_delete BEFORE DELETE ON protocol_trials
BEGIN SELECT RAISE(ABORT, 'protocol trials cannot be deleted'); END;

CREATE TRIGGER protocol_trials_enforce_prereg BEFORE INSERT ON protocol_trials
WHEN NEW.protocol_sha256 <> (
    SELECT protocol_sha256 FROM protocol_preregistrations WHERE prereg_id = NEW.prereg_id
  )
  OR NEW.trial_index > (
    SELECT max_trials FROM protocol_preregistrations WHERE prereg_id = NEW.prereg_id
  )
  OR NEW.trial_index <> (
    SELECT COALESCE(MAX(trial_index), 0) + 1 FROM protocol_trials WHERE prereg_id = NEW.prereg_id
  )
BEGIN SELECT RAISE(ABORT, 'protocol trial violates pre-registration binding, sequence, or budget'); END;
