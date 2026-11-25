-- Permanent, single-use Owner Archive Breaker authorization bindings (migration 6).
-- Keep the exact prepared intent even after its in-flight row is deleted. The
-- result is the ordinary recorded_pivots receipt, keyed by the same ADR.
CREATE TABLE owner_archive_requests (
  request_id TEXT NOT NULL PRIMARY KEY CHECK(
    length(request_id) BETWEEN 1 AND 128 AND request_id NOT GLOB '*[^A-Za-z0-9_-]*'
  ),
  message_sha256 TEXT NOT NULL CHECK(length(message_sha256)=64 AND message_sha256 NOT GLOB '*[^0-9a-f]*'),
  public_key_fingerprint TEXT NOT NULL CHECK(
    length(public_key_fingerprint)=64 AND public_key_fingerprint NOT GLOB '*[^0-9a-f]*'
  ),
  adr_id TEXT NOT NULL UNIQUE CHECK(
    length(adr_id)=74 AND substr(adr_id,1,10)='ADR-PIVOT-' AND substr(adr_id,11) NOT GLOB '*[^0-9a-f]*'
  ),
  plan_json TEXT NOT NULL CHECK(json_extract(plan_json,'$.action') IS 'ARCHIVE_BREAKER'),
  new_commit TEXT NOT NULL CHECK(length(new_commit) IN (40,64) AND new_commit NOT GLOB '*[^0-9a-f]*'),
  status TEXT NOT NULL DEFAULT 'PREPARED' CHECK(status IN ('PREPARED','APPLIED','ABANDONED'))
) STRICT;
CREATE TRIGGER owner_archive_requests_no_delete BEFORE DELETE ON owner_archive_requests
BEGIN SELECT RAISE(ABORT, 'Owner archive requests are permanent'); END;
-- Also reject REPLACE even when recursive triggers are disabled.
CREATE TRIGGER owner_archive_requests_prepare BEFORE INSERT ON owner_archive_requests
WHEN NEW.status <> 'PREPARED'
  OR EXISTS (SELECT 1 FROM owner_archive_requests WHERE request_id=NEW.request_id OR adr_id=NEW.adr_id)
  OR EXISTS (SELECT 1 FROM recorded_pivots WHERE adr_id=NEW.adr_id)
  OR NOT EXISTS (SELECT 1 FROM pivot_resume_intent WHERE singleton=1 AND adr_id=NEW.adr_id
                 AND plan_json=NEW.plan_json AND new_commit=NEW.new_commit)
BEGIN SELECT RAISE(ABORT, 'invalid or duplicate Owner archive request'); END;
CREATE TRIGGER owner_archive_requests_transition BEFORE UPDATE ON owner_archive_requests
WHEN OLD.request_id <> NEW.request_id OR OLD.message_sha256 <> NEW.message_sha256
  OR OLD.public_key_fingerprint <> NEW.public_key_fingerprint OR OLD.adr_id <> NEW.adr_id
  OR OLD.plan_json <> NEW.plan_json OR OLD.new_commit <> NEW.new_commit
  OR OLD.status <> 'PREPARED' OR NEW.status NOT IN ('APPLIED','ABANDONED')
  OR NOT EXISTS (SELECT 1 FROM pivot_resume_intent WHERE singleton=1 AND adr_id=NEW.adr_id
                 AND plan_json=NEW.plan_json AND new_commit=NEW.new_commit)
  OR (NEW.status='APPLIED' AND NOT EXISTS (SELECT 1 FROM recorded_pivots WHERE adr_id=NEW.adr_id
                                         AND plan_json=NEW.plan_json AND git_commit=NEW.new_commit))
  OR (NEW.status='ABANDONED' AND EXISTS (SELECT 1 FROM recorded_pivots WHERE adr_id=NEW.adr_id))
BEGIN SELECT RAISE(ABORT, 'invalid Owner archive request transition'); END;
