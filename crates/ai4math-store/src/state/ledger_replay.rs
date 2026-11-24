use super::last_seq;
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, Transaction, named_params, types::ValueRef};

pub(super) const MAX_SOURCE_ROWS: u32 = 8192;
const MAX_PAYLOAD_BYTES: usize = 1024 * 1024;
const MAX_TOTAL_PAYLOAD_BYTES: usize = 8 * MAX_PAYLOAD_BYTES;
const MAX_GRAPH_SOURCE_ROWS: u32 = 32_768;
const MAX_GRAPH_ID_BYTES: usize = 4096;
const MAX_GRAPH_TYPE_BYTES: usize = 4096;
const MAX_GRAPH_TOTAL_PAYLOAD_BYTES: usize = 16 * MAX_PAYLOAD_BYTES;

pub(super) struct ReplayWindow {
    pub(super) through_event_seq: i64,
    pub(super) ledger_head_event_seq: i64,
    pub(super) applied_events: u32,
}

/// Scan one entity's ledger prefix in the same read transaction as the high-water query.
/// Count all selected source rows, but fetch bounded payloads only for applied events.
pub(super) fn replay_entity(
    db: &Connection,
    entity_type: &str,
    entity_id: &str,
    through_event_seq: Option<i64>,
    family: &str,
    ignored: Option<&str>,
    mut apply: impl FnMut(i64, &str, Option<&str>, &str) -> Result<()>,
) -> Result<ReplayWindow> {
    let label = match family {
        "CLAIM_" => "Claim",
        "PLAN_" => "ProofPlan",
        _ => bail!("unsupported ledger replay family {family}"),
    };
    let snapshot = db.unchecked_transaction()?;
    let ledger_head_event_seq = last_seq(&snapshot)?;
    let through_event_seq = through_event_seq.unwrap_or(ledger_head_event_seq);
    ensure!(
        (0..=ledger_head_event_seq).contains(&through_event_seq),
        "{label} replay cursor {through_event_seq} is outside [0, {ledger_head_event_seq}]"
    );

    let mut applied_events = 0;
    let mut total_payload_bytes = 0;
    let mut source_rows = 0;
    {
        let mut stmt = snapshot.prepare(
            "SELECT event_seq,event_type,
               CASE WHEN :family='CLAIM_' AND event_type='CLAIM_TAINTED'
                         AND event_type IS NOT :ignored
                         AND length(CAST(git_commit AS BLOB))<=:max_payload THEN git_commit END,
               CASE WHEN event_type GLOB (:family || '*') AND event_type IS NOT :ignored
                         AND length(CAST(payload_json AS BLOB))<=:max_payload THEN payload_json END
             FROM events INDEXED BY idx_events_entity
             WHERE entity_type=:entity_type AND entity_id=:entity_id AND event_seq<=:through_seq
             ORDER BY event_seq LIMIT :max_rows",
        )?;
        let mut rows = stmt.query(named_params! {
            ":family": family,
            ":ignored": ignored,
            ":max_payload": MAX_PAYLOAD_BYTES,
            ":entity_type": entity_type,
            ":entity_id": entity_id,
            ":through_seq": through_event_seq,
            ":max_rows": MAX_SOURCE_ROWS + 1,
        })?;
        while let Some(row) = rows.next()? {
            source_rows += 1;
            ensure!(
                source_rows <= MAX_SOURCE_ROWS,
                "{label} replay exceeds the {MAX_SOURCE_ROWS}-event cap on {label} source rows"
            );
            let seq: i64 = row.get(0)?;
            let kind = row.get_ref(1)?.as_str()?;
            if !kind.starts_with(family) || ignored == Some(kind) {
                continue;
            }
            let payload = row.get_ref(3)?;
            ensure!(
                !matches!(payload, ValueRef::Null),
                "{label} replay payload exceeds {MAX_PAYLOAD_BYTES} bytes at event {seq}"
            );
            let payload = payload.as_str()?;
            let commit = row.get_ref(2)?.as_str_or_null()?;
            total_payload_bytes += payload.len() + commit.map_or(0, str::len);
            ensure!(
                total_payload_bytes <= MAX_TOTAL_PAYLOAD_BYTES,
                "{label} replay exceeds {MAX_TOTAL_PAYLOAD_BYTES} total payload bytes"
            );
            apply(seq, kind, commit, payload)
                .with_context(|| format!("invalid {label} replay event {seq} ({kind})"))?;
            applied_events += 1;
        }
    }
    snapshot.commit()?;
    Ok(ReplayWindow {
        through_event_seq,
        ledger_head_event_seq,
        applied_events,
    })
}

/// Scan the global ledger prefix in sequence order within the caller's read snapshot.
/// The cap counts every source row; only graph state events reach the reducer.
pub(super) fn replay_graph_rows_in_snapshot(
    snapshot: &Transaction<'_>,
    through_event_seq: Option<i64>,
    mut apply: impl FnMut(i64, &str, &str, &str, Option<&str>, &str) -> Result<()>,
) -> Result<ReplayWindow> {
    let ledger_head_event_seq = last_seq(snapshot)?;
    let through_event_seq = through_event_seq.unwrap_or(ledger_head_event_seq);
    ensure!(
        (0..=ledger_head_event_seq).contains(&through_event_seq),
        "graph replay cursor {through_event_seq} is outside [0, {ledger_head_event_seq}]"
    );

    let mut applied_events = 0;
    let mut total_payload_bytes = 0;
    let mut source_rows = 0;
    {
        // NOT INDEXED keeps this a rowid (event_seq) range, not a family-index scan.
        // In-family unknown kinds reach the reducer. Recognize exact current state
        // kinds on a wrong entity too, so a corrupted type cannot hide a mutation.
        let mut stmt = snapshot.prepare(
            "SELECT event_seq, expected_entity, entity_type=expected_entity,
               CASE WHEN entity_type=expected_entity AND octet_length(entity_id)<=:max_id
                    THEN entity_id END,
               CASE WHEN expected_entity IS NOT NULL AND octet_length(event_type)<=:max_type
                    THEN event_type END,
               CASE WHEN entity_type=expected_entity AND event_type='CLAIM_TAINTED'
                         AND octet_length(git_commit)<=:max_payload THEN git_commit END,
               CASE WHEN entity_type=expected_entity AND event_type='CLAIM_TAINTED'
                         AND octet_length(git_commit)>:max_payload THEN 1 ELSE 0 END,
               CASE WHEN entity_type=expected_entity AND octet_length(payload_json)<=:max_payload
                    THEN payload_json END
             FROM (
               SELECT event_seq, entity_type, entity_id, event_type, git_commit, payload_json,
                 CASE
                   WHEN (entity_type='claim' AND CAST(event_type AS TEXT) GLOB 'CLAIM_*'
                         AND event_type<>'CLAIM_REPROOF_REQUESTED')
                        OR CAST(event_type AS TEXT) IN (
                          'CLAIM_CREATED','CLAIM_TAINTED','CLAIM_TRUST_CHANGED',
                          'CLAIM_EFFECTIVE_CHANGED','CLAIM_ROUTES_EXHAUSTED','CLAIM_ROUTES_REOPENED'
                        ) THEN 'claim'
                   WHEN (entity_type='proof_plan' AND CAST(event_type AS TEXT) GLOB 'PLAN_*')
                        OR CAST(event_type AS TEXT) IN (
                          'PLAN_CREATED','PLAN_TRUST_CHANGED','PLAN_INVALIDATED',
                          'PLAN_ROUTE_BLOCKED','PLAN_ROUTE_UNBLOCKED','PLAN_ARCHIVED'
                        ) THEN 'proof_plan'
                 END AS expected_entity
               FROM events NOT INDEXED WHERE event_seq<=:through_seq
             )
             ORDER BY event_seq LIMIT :max_rows",
        )?;
        let mut rows = stmt.query(named_params! {
            ":max_id": MAX_GRAPH_ID_BYTES,
            ":max_type": MAX_GRAPH_TYPE_BYTES,
            ":max_payload": MAX_PAYLOAD_BYTES,
            ":through_seq": through_event_seq,
            ":max_rows": MAX_GRAPH_SOURCE_ROWS + 1,
        })?;
        while let Some(row) = rows.next()? {
            source_rows += 1;
            ensure!(
                source_rows <= MAX_GRAPH_SOURCE_ROWS,
                "graph replay exceeds the {MAX_GRAPH_SOURCE_ROWS}-event cap on global source rows"
            );
            let Some(entity_type) = row.get_ref(1)?.as_str_or_null()? else {
                continue;
            };
            let seq: i64 = row.get(0)?;
            // SQL bounds the stored bytes before projecting each field; enforce the
            // same cap after decoding in case the database is not encoded as UTF-8.
            let kind = row
                .get_ref(4)?
                .as_str_or_null()?
                .filter(|kind| kind.len() <= MAX_GRAPH_TYPE_BYTES)
                .with_context(|| {
                    format!("graph replay event type exceeds {MAX_GRAPH_TYPE_BYTES} bytes at event {seq}")
                })?;
            ensure!(
                row.get::<_, bool>(2)?,
                "invalid graph replay event {seq} ({kind}): expected entity type {entity_type}"
            );
            let id = row
                .get_ref(3)?
                .as_str_or_null()?
                .filter(|id| id.len() <= MAX_GRAPH_ID_BYTES)
                .with_context(|| {
                    format!(
                        "graph replay entity ID exceeds {MAX_GRAPH_ID_BYTES} bytes at event {seq}"
                    )
                })?;
            let commit = row.get_ref(5)?.as_str_or_null()?;
            ensure!(
                !row.get::<_, bool>(6)?
                    && commit.is_none_or(|commit| commit.len() <= MAX_PAYLOAD_BYTES),
                "graph replay taint commit exceeds {MAX_PAYLOAD_BYTES} bytes at event {seq}"
            );
            let payload = row
                .get_ref(7)?
                .as_str_or_null()?
                .filter(|payload| payload.len() <= MAX_PAYLOAD_BYTES)
                .with_context(|| {
                    format!("graph replay payload exceeds {MAX_PAYLOAD_BYTES} bytes at event {seq}")
                })?;
            total_payload_bytes += payload.len() + commit.map_or(0, str::len);
            ensure!(
                total_payload_bytes <= MAX_GRAPH_TOTAL_PAYLOAD_BYTES,
                "graph replay exceeds {MAX_GRAPH_TOTAL_PAYLOAD_BYTES} total payload bytes"
            );
            apply(seq, entity_type, id, kind, commit, payload)
                .with_context(|| format!("invalid graph replay event {seq} ({kind})"))?;
            applied_events += 1;
        }
    }
    Ok(ReplayWindow {
        through_event_seq,
        ledger_head_event_seq,
        applied_events,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StateStore;
    use rusqlite::params;

    fn replay_graph_rows(
        db: &Connection,
        through_event_seq: Option<i64>,
        apply: impl FnMut(i64, &str, &str, &str, Option<&str>, &str) -> Result<()>,
    ) -> Result<ReplayWindow> {
        let snapshot = db.unchecked_transaction()?;
        let window = replay_graph_rows_in_snapshot(&snapshot, through_event_seq, apply)?;
        snapshot.commit()?;
        Ok(window)
    }

    fn insert_graph_row(
        db: &Connection,
        entity: &str,
        id: &str,
        kind: &str,
        commit: Option<&str>,
        body: &str,
    ) -> Result<i64> {
        db.execute(
            "INSERT INTO events(occurred_at,event_type,entity_type,entity_id,git_commit,actor,payload_json)
             VALUES ('timestamp',?1,?2,?3,?4,'test',?5)",
            params![kind, entity, id, commit, body],
        )?;
        Ok(db.last_insert_rowid())
    }

    #[test]
    fn graph_replay_filters_and_forwards_state_events_in_order() -> Result<()> {
        let store = StateStore::open(":memory:")?;
        store.db.execute_batch(
            "INSERT INTO events(event_seq,occurred_at,event_type,entity_type,entity_id,git_commit,actor,payload_json)
             VALUES (1,'timestamp','CLAIM_REPROOF_REQUESTED','claim',zeroblob(5000),x'ff','test',zeroblob(2097152)),
                    (3,'timestamp','CLAIM_FUTURE','task',x'ff',x'ff','test',x'ff'),
                    (4,'timestamp','PLAN_FUTURE','proof_plan','plan',x'ff','test','plan body'),
                    (7,'timestamp','CLAIM_TAINTED','claim','claim','root','test','claim body'),
                    (8,'timestamp','CLAIM_CREATED','wrong_entity','claim',x'ff','test',x'ff');",
        )?;
        let mut seen = Vec::new();
        let window =
            replay_graph_rows(&store.db, Some(7), |seq, entity, id, kind, commit, body| {
                seen.push((
                    seq,
                    entity.to_owned(),
                    id.to_owned(),
                    kind.to_owned(),
                    commit.map(str::to_owned),
                    body.to_owned(),
                ));
                Ok(())
            })?;
        let seen: Vec<_> = seen
            .iter()
            .map(|(seq, entity, id, kind, commit, body)| {
                (
                    *seq,
                    entity.as_str(),
                    id.as_str(),
                    kind.as_str(),
                    commit.as_deref(),
                    body.as_str(),
                )
            })
            .collect();
        assert_eq!(
            seen,
            [
                (4, "proof_plan", "plan", "PLAN_FUTURE", None, "plan body"),
                (
                    7,
                    "claim",
                    "claim",
                    "CLAIM_TAINTED",
                    Some("root"),
                    "claim body"
                ),
            ]
        );
        assert_eq!(window.through_event_seq, 7);
        assert_eq!(window.ledger_head_event_seq, 8);
        assert_eq!(window.applied_events, 2);
        let gap = replay_graph_rows(&store.db, Some(6), |_, _, _, _, _, _| Ok(()))?;
        assert_eq!(gap.through_event_seq, 6);
        assert_eq!(gap.applied_events, 1);
        let empty =
            replay_graph_rows(&store.db, Some(0), |_, _, _, _, _, _| panic!("zero cursor"))?;
        assert_eq!(empty.applied_events, 0);
        for cursor in [-1, 9] {
            let error = replay_graph_rows(&store.db, Some(cursor), |_, _, _, _, _, _| {
                panic!("invalid cursor")
            })
            .err()
            .context("invalid cursor accepted")?;
            assert!(error.to_string().contains("outside [0, 8]"), "{error:#}");
        }
        let error = replay_graph_rows(&store.db, None, |_, _, _, _, _, _| Ok(()))
            .err()
            .context("incorrect entity accepted")?;
        assert!(
            error.to_string().contains("expected entity type claim"),
            "{error:#}"
        );
        Ok(())
    }

    #[test]
    fn graph_replay_counts_all_global_rows_and_checks_the_sentinel() -> Result<()> {
        let store = StateStore::open(":memory:")?;
        store.db.execute(
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<?1)
             INSERT INTO events(occurred_at,event_type,entity_type,entity_id,actor)
             SELECT 'timestamp','TASK_CREATED','task','task','test' FROM n",
            [MAX_GRAPH_SOURCE_ROWS],
        )?;
        let at_cap = replay_graph_rows(&store.db, None, |_, _, _, _, _, _| panic!("task event"))?;
        assert_eq!(
            at_cap.ledger_head_event_seq,
            i64::from(MAX_GRAPH_SOURCE_ROWS)
        );
        assert_eq!(at_cap.applied_events, 0);
        insert_graph_row(&store.db, "task", "task", "TASK_CREATED", None, "{}")?;
        let error = replay_graph_rows(&store.db, None, |_, _, _, _, _, _| panic!("task event"))
            .err()
            .context("source cap not enforced")?;
        assert!(
            error
                .to_string()
                .contains("32768-event cap on global source rows"),
            "{error:#}"
        );
        Ok(())
    }

    #[test]
    fn graph_replay_bounds_relevant_fields_and_aggregate_bytes() -> Result<()> {
        let store = StateStore::open(":memory:")?;
        let payload = "é".repeat(MAX_PAYLOAD_BYTES / 2);
        let id = "é".repeat(MAX_GRAPH_ID_BYTES / 2);
        let kind = format!("PLAN_{}x", "é".repeat((MAX_GRAPH_TYPE_BYTES - 6) / 2));
        assert_eq!(kind.len(), MAX_GRAPH_TYPE_BYTES);
        let taint = insert_graph_row(
            &store.db,
            "claim",
            &id,
            "CLAIM_TAINTED",
            Some("root"),
            &payload[..payload.len() - 4],
        )?;
        for _ in 0..15 {
            insert_graph_row(&store.db, "proof_plan", &id, &kind, None, &payload)?;
        }
        let window =
            replay_graph_rows(&store.db, None, |seq, _, row_id, row_kind, commit, body| {
                assert_eq!(row_id, id);
                if seq == taint {
                    assert_eq!(commit, Some("root"));
                    assert_eq!(body.len() + 4, MAX_PAYLOAD_BYTES);
                } else {
                    assert_eq!(row_kind, kind);
                    assert_eq!(commit, None);
                    assert_eq!(body.len(), MAX_PAYLOAD_BYTES);
                }
                Ok(())
            })?;
        assert_eq!(window.applied_events, 16);
        insert_graph_row(&store.db, "proof_plan", "plan", "PLAN_CREATED", None, "é")?;
        let error = replay_graph_rows(&store.db, None, |_, _, _, _, _, _| Ok(()))
            .err()
            .context("aggregate budget not enforced")?;
        assert!(
            error.to_string().contains("16777216 total payload bytes"),
            "{error:#}"
        );

        let large_id = format!("{id}é");
        let large_kind = format!("{kind}é");
        let large_body = format!("{payload}é");
        for (entity, id, kind, commit, payload, expected) in [
            (
                "proof_plan",
                "plan",
                large_kind.as_str(),
                None,
                "{}",
                "event type exceeds 4096 bytes",
            ),
            (
                "proof_plan",
                large_id.as_str(),
                "PLAN_CREATED",
                None,
                "{}",
                "entity ID exceeds 4096 bytes",
            ),
            (
                "proof_plan",
                "plan",
                "PLAN_CREATED",
                None,
                large_body.as_str(),
                "payload exceeds 1048576 bytes",
            ),
            (
                "claim",
                "claim",
                "CLAIM_TAINTED",
                Some(large_body.as_str()),
                "{}",
                "taint commit exceeds 1048576 bytes",
            ),
        ] {
            let store = StateStore::open(":memory:")?;
            insert_graph_row(&store.db, entity, id, kind, commit, payload)?;
            let error =
                replay_graph_rows(&store.db, None, |_, _, _, _, _, _| panic!("over-cap event"))
                    .err()
                    .context("field limit not enforced")?;
            assert!(error.to_string().contains(expected), "{error:#}");
        }
        Ok(())
    }

    #[test]
    fn graph_replay_uses_a_single_snapshot_with_a_concurrent_writer() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("state.sqlite");
        let writer = StateStore::open(&path)?;
        let initial = insert_graph_row(&writer.db, "claim", "c", "CLAIM_CREATED", None, "first")?;
        let head = insert_graph_row(
            &writer.db,
            "proof_plan",
            "p",
            "PLAN_CREATED",
            None,
            "second",
        )?;
        let reader = StateStore::open_readonly(&path)?;
        let journal_mode = reader
            .db
            .query_row("PRAGMA journal_mode", [], |row| row.get::<_, String>(0))?;
        assert_eq!(journal_mode, "wal");
        let mut appended = None;
        let window = replay_graph_rows(&reader.db, None, |seq, _, _, _, _, payload| {
            if seq == initial {
                appended = Some(insert_graph_row(
                    &writer.db,
                    "claim",
                    "c",
                    "CLAIM_TAINTED",
                    Some("root"),
                    "third",
                )?);
                assert!(!reader.db.is_autocommit());
                assert_eq!(last_seq(&reader.db)?, head);
                assert_eq!(last_seq(&writer.db)?, appended.unwrap());
            } else {
                assert_eq!(seq, head);
                assert_eq!(payload, "second");
            }
            Ok(())
        })?;
        assert_eq!(window.through_event_seq, head);
        assert_eq!(window.ledger_head_event_seq, head);
        assert_eq!(window.applied_events, 2);
        let new_window = replay_graph_rows(&reader.db, None, |seq, _, _, _, _, payload| {
            if Some(seq) == appended {
                assert_eq!(payload, "third");
            }
            Ok(())
        })?;
        assert_eq!(Some(new_window.ledger_head_event_seq), appended);
        assert_eq!(new_window.applied_events, 3);
        Ok(())
    }

    #[test]
    fn plan_replay_bounds_payload_bytes_without_reading_commit() -> Result<()> {
        let store = StateStore::open(":memory:")?;
        let payload = "é".repeat(MAX_PAYLOAD_BYTES / 2);
        store.db.execute(
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<8)
             INSERT INTO events(occurred_at,event_type,entity_type,entity_id,git_commit,actor,payload_json)
             SELECT 'timestamp','PLAN_CREATED','proof_plan','plan',x'ff','test',?1 FROM n",
            [&payload],
        )?;
        let oversized = format!("{payload}é");
        store.db.execute(
            "INSERT INTO events(occurred_at,event_type,entity_type,entity_id,actor,payload_json)
             VALUES ('timestamp','UNRELATED_EVENT','proof_plan','plan','test',?1)",
            [&oversized],
        )?;
        let at_cap = replay_entity(
            &store.db,
            "proof_plan",
            "plan",
            None,
            "PLAN_",
            None,
            |_, _, commit, body| {
                assert!(commit.is_none());
                assert_eq!(body.len(), MAX_PAYLOAD_BYTES);
                Ok(())
            },
        )?;
        assert_eq!(at_cap.applied_events, 8);

        for (id, body) in [("plan", "é"), ("oversize", &oversized)] {
            store.db.execute(
                "INSERT INTO events(occurred_at,event_type,entity_type,entity_id,actor,payload_json)
                 VALUES ('timestamp','PLAN_CREATED','proof_plan',?1,'test',?2)",
                params![id, body],
            )?;
            let error = replay_entity(
                &store.db,
                "proof_plan",
                id,
                None,
                "PLAN_",
                None,
                |_, _, _, _| Ok(()),
            )
            .err()
            .expect("payload exceeds replay byte budget");
            let expected = if id == "plan" {
                format!("exceeds {MAX_TOTAL_PAYLOAD_BYTES} total payload bytes")
            } else {
                let seq = store.db.last_insert_rowid();
                format!("payload exceeds {MAX_PAYLOAD_BYTES} bytes at event {seq}")
            };
            assert!(error.to_string().contains(&expected), "{error:#}");
        }
        Ok(())
    }
}
