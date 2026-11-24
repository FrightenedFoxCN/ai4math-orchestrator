use super::last_seq;
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, named_params, types::ValueRef};

pub(super) const MAX_SOURCE_ROWS: u32 = 8192;
const MAX_PAYLOAD_BYTES: usize = 1024 * 1024;
const MAX_TOTAL_PAYLOAD_BYTES: usize = 8 * MAX_PAYLOAD_BYTES;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StateStore;
    use rusqlite::params;

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
