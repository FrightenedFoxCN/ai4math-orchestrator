use super::{StateStore, Trust, last_seq};
use anyhow::{Context, Result, bail, ensure};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

const MAX_SOURCE_ROWS: u32 = 8192;
const MAX_PAYLOAD_BYTES: usize = 1024 * 1024;
const MAX_TOTAL_PAYLOAD_BYTES: usize = 8 * MAX_PAYLOAD_BYTES;

/// Historical Claim projection from one consistent ledger snapshot, not verified proof evidence.
#[derive(Debug, Serialize)]
pub struct ClaimReplay {
    pub claim_id: String,
    pub through_event_seq: i64,
    pub ledger_head_event_seq: i64,
    /// Number of Claim state events applied, excluding advisory and unrelated events.
    pub applied_events: u32,
    pub claim: Option<ReplayedClaim>,
}

#[derive(Debug, Serialize)]
pub struct ReplayedClaim {
    pub claim_id: String,
    pub title: String,
    pub engine: String,
    pub informal_statement: String,
    pub formal_target: Option<String>,
    pub module_path: Option<String>,
    pub critical: bool,
    pub local_trust: Trust,
    pub effective_trust: Trust,
    pub stale: bool,
    pub effective_stale: bool,
    pub prior_trust: Option<Trust>,
    pub taint_root_commit: Option<String>,
    pub routes_exhausted: bool,
    /// Last state event for this Claim, not the snapshot head or a receipt sequence.
    pub last_event_seq: i64,
}

impl StateStore {
    /// Replays the selected Claim's retained state events through an inclusive sequence, or
    /// through the snapshot head if omitted. An intermediate cursor may separate local and
    /// effective changes from the same write transaction. Reads no live projection tables.
    ///
    /// Rejects incomplete or inconsistent Claim histories rather than projecting a partial
    /// result. At most 8192 source rows for this Claim (including ignored rows), 1 MiB per
    /// applied event payload, and 8 MiB across all applied event payloads are allowed.
    /// This is not a global ledger integrity check or a replay of receipts, promotion
    /// authority, or the proof graph (e.g. `PROMOTION_COMMITTED` is ignored).
    pub fn replay_claim(
        &self,
        claim_id: &str,
        through_event_seq: Option<i64>,
    ) -> Result<ClaimReplay> {
        let snapshot = self.db.unchecked_transaction()?;
        let ledger_head_event_seq = last_seq(&snapshot)?;
        let through_event_seq = through_event_seq.unwrap_or(ledger_head_event_seq);
        ensure!(
            (0..=ledger_head_event_seq).contains(&through_event_seq),
            "Claim replay cursor {through_event_seq} is outside [0, {ledger_head_event_seq}]"
        );
        let mut claim = None;
        let mut applied_events = 0;
        let mut total_payload_bytes = 0;
        let mut source_rows = 0;
        {
            let mut stmt = snapshot.prepare(
                "SELECT event_seq,event_type,
                   CASE WHEN event_type='CLAIM_TAINTED' AND length(CAST(git_commit AS BLOB))<=?4
                        THEN git_commit END,
                   CASE WHEN event_type GLOB 'CLAIM_*' AND event_type<>'CLAIM_REPROOF_REQUESTED'
                             AND length(CAST(payload_json AS BLOB))<=?4 THEN payload_json END
                 FROM events INDEXED BY idx_events_entity
                 WHERE entity_type='claim' AND entity_id=?1 AND event_seq<=?2
                 ORDER BY event_seq LIMIT ?3",
            )?;
            let mut rows = stmt.query(params![
                claim_id,
                through_event_seq,
                MAX_SOURCE_ROWS + 1,
                MAX_PAYLOAD_BYTES,
            ])?;
            while let Some(row) = rows.next()? {
                source_rows += 1;
                ensure!(
                    source_rows <= MAX_SOURCE_ROWS,
                    "Claim replay exceeds the {MAX_SOURCE_ROWS}-event cap on Claim source rows"
                );
                let seq: i64 = row.get(0)?;
                let kind = row.get_ref(1)?.as_str()?;
                if !kind.starts_with("CLAIM_") || kind == "CLAIM_REPROOF_REQUESTED" {
                    continue;
                }
                let payload = row.get_ref(3)?;
                ensure!(
                    !matches!(payload, rusqlite::types::ValueRef::Null),
                    "Claim replay payload exceeds {MAX_PAYLOAD_BYTES} bytes at event {seq}"
                );
                let payload = payload.as_str()?;
                let commit = if kind == "CLAIM_TAINTED" {
                    row.get_ref(2)?.as_str_or_null()?
                } else {
                    None
                };
                total_payload_bytes += payload.len() + commit.map_or(0, str::len);
                ensure!(
                    total_payload_bytes <= MAX_TOTAL_PAYLOAD_BYTES,
                    "Claim replay exceeds {MAX_TOTAL_PAYLOAD_BYTES} total payload bytes"
                );
                apply(&mut claim, claim_id, seq, kind, commit, payload)
                    .with_context(|| format!("invalid Claim replay event {seq} ({kind})"))?;
                applied_events += 1;
            }
        }
        snapshot.commit()?;
        Ok(ClaimReplay {
            claim_id: claim_id.to_owned(),
            through_event_seq,
            ledger_head_event_seq,
            applied_events,
            claim,
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Created {
    title: String,
    engine: String,
    informal_statement: String,
    // The event writer includes the keys, using explicit null when absent.
    #[serde(deserialize_with = "Deserialize::deserialize")]
    formal_target: Option<String>,
    #[serde(deserialize_with = "Deserialize::deserialize")]
    module_path: Option<String>,
    critical: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Tainted {
    prior_trust: Trust,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TrustChanged {
    previous_local: Trust,
    local: Trust,
    was_stale: bool,
    // Validate the event shape without interpreting the detail as proof evidence.
    #[serde(rename = "detail")]
    _detail: Map<String, Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EffectiveChanged {
    previous: Trust,
    effective: Trust,
    effective_stale: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RoutesExhausted {
    refuted_claim: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RoutesReopened {}

fn apply(
    state: &mut Option<ReplayedClaim>,
    id: &str,
    seq: i64,
    kind: &str,
    git_commit: Option<&str>,
    payload: &str,
) -> Result<()> {
    ensure!(seq > 0, "Claim event sequence must be positive");
    // Serde structs can also deserialize positionally; the ledger contract is JSON objects.
    ensure!(
        payload.trim_start().starts_with('{'),
        "expected Claim event payload object"
    );
    if kind == "CLAIM_CREATED" {
        ensure!(state.is_none(), "duplicate CLAIM_CREATED");
        let p: Created = serde_json::from_str(payload)?;
        ensure!(
            matches!(
                p.engine.as_str(),
                "lean4" | "agda" | "rocq" | "smt" | "atp" | "cas" | "informal"
            ),
            "invalid Claim engine"
        );
        *state = Some(ReplayedClaim {
            claim_id: id.to_owned(),
            title: p.title,
            engine: p.engine,
            informal_statement: p.informal_statement,
            formal_target: p.formal_target,
            module_path: p.module_path,
            critical: p.critical,
            local_trust: Trust::L0,
            effective_trust: Trust::L0,
            stale: false,
            effective_stale: false,
            prior_trust: None,
            taint_root_commit: None,
            routes_exhausted: false,
            last_event_seq: seq,
        });
        return Ok(());
    }
    let claim = state.as_mut().context("missing preceding CLAIM_CREATED")?;
    match kind {
        "CLAIM_TAINTED" => {
            let p: Tainted = serde_json::from_str(payload)?;
            let commit = git_commit.context("missing taint root commit")?;
            ensure!(
                claim.local_trust != Trust::L2b
                    && p.prior_trust == claim.prior_trust.unwrap_or(claim.local_trust),
                "illegal taint predecessor"
            );
            claim.stale = true;
            claim.prior_trust = Some(p.prior_trust);
            claim.taint_root_commit = Some(commit.to_owned());
        }
        "CLAIM_TRUST_CHANGED" => {
            let p: TrustChanged = serde_json::from_str(payload)?;
            ensure!(
                claim.local_trust != Trust::L2b
                    && p.previous_local == claim.local_trust
                    && p.was_stale == claim.stale,
                "illegal local trust predecessor"
            );
            claim.local_trust = p.local;
            claim.stale = false;
            claim.prior_trust = None;
            claim.taint_root_commit = None;
            if p.local == Trust::L2b {
                // Route refresh deletes exhaustion on terminal refutation without emitting
                // CLAIM_ROUTES_REOPENED: the Claim was refuted, not reopened.
                claim.routes_exhausted = false;
            }
        }
        "CLAIM_EFFECTIVE_CHANGED" => {
            let p: EffectiveChanged = serde_json::from_str(payload)?;
            ensure!(
                p.previous == claim.effective_trust,
                "wrong previous effective trust"
            );
            ensure!(
                (p.effective, p.effective_stale) != (claim.effective_trust, claim.effective_stale),
                "effective change did not change trust or staleness"
            );
            claim.effective_trust = p.effective;
            claim.effective_stale = p.effective_stale;
        }
        "CLAIM_ROUTES_EXHAUSTED" => {
            let p: RoutesExhausted = serde_json::from_str(payload)?;
            ensure!(
                !claim.routes_exhausted && claim.local_trust != Trust::L2b,
                "illegal route exhaustion predecessor"
            );
            ensure!(
                p.refuted_claim != id && !p.refuted_claim.trim().is_empty(),
                "invalid route exhaustion cause"
            );
            claim.routes_exhausted = true;
        }
        "CLAIM_ROUTES_REOPENED" => {
            let _: RoutesReopened = serde_json::from_str(payload)?;
            ensure!(claim.routes_exhausted, "only exhausted routes may reopen");
            claim.routes_exhausted = false;
        }
        _ => bail!("unsupported Claim state event {kind}"),
    }
    claim.last_event_seq = seq;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Change;

    const CREATED: &str = r#"{"title":"Claim","engine":"lean4","informal_statement":"True","formal_target":null,"module_path":null,"critical":false}"#;

    #[test]
    fn decode_requires_unambiguous_complete_creation() -> Result<()> {
        for malformed in [
            CREATED.replace("\"formal_target\":null,", ""),
            CREATED.replace("\"module_path\":null,", ""),
            CREATED.replace("\"critical\":false", "\"critical\":false,\"critical\":true"),
            r#"["Claim","lean4","True",null,null,false]"#.to_owned(),
        ] {
            assert!(apply(&mut None, "claim", 1, "CLAIM_CREATED", None, &malformed).is_err());
        }
        let mut state = None;
        apply(&mut state, "claim", 1, "CLAIM_CREATED", None, CREATED)?;
        assert!(apply(&mut state, "claim", 2, "CLAIM_CREATED", None, CREATED).is_err());
        Ok(())
    }

    #[test]
    fn local_refutation_clears_exhaustion_without_inventing_effective_event() -> Result<()> {
        let mut state = None;
        apply(&mut state, "claim", 1, "CLAIM_CREATED", None, CREATED)?;
        apply(
            &mut state,
            "claim",
            2,
            "CLAIM_ROUTES_EXHAUSTED",
            None,
            r#"{"refuted_claim":"prior"}"#,
        )?;
        apply(
            &mut state,
            "claim",
            3,
            "CLAIM_TRUST_CHANGED",
            None,
            r#"{"previous_local":"L0","local":"L2b","was_stale":false,"detail":{"wiki_id":"witness"}}"#,
        )?;
        let claim = state.as_ref().unwrap();
        assert!(!claim.routes_exhausted);
        assert_eq!(claim.local_trust, Trust::L2b);
        assert_eq!(claim.effective_trust, Trust::L0);
        assert!(apply(&mut state, "claim", 4, "CLAIM_ROUTES_REOPENED", None, "{}").is_err());
        Ok(())
    }

    #[test]
    fn cap_includes_ignored_claim_rows_without_counting_them_as_applied() -> Result<()> {
        let mut store = StateStore::open(":memory:")?;
        let change = Change {
            actor: "test",
            git_commit: None,
        };
        let created = store.append_event(
            change,
            "CLAIM_CREATED",
            "claim",
            "claim",
            serde_json::from_str(CREATED)?,
        )?;
        store.db.execute(
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<?1)
             INSERT INTO events(occurred_at,event_type,entity_type,entity_id,actor)
             SELECT 'timestamp','CLAIM_REPROOF_REQUESTED','claim','claim','test' FROM n",
            [MAX_SOURCE_ROWS],
        )?;
        let at_cap = store.replay_claim("claim", Some(i64::from(MAX_SOURCE_ROWS)))?;
        assert_eq!(at_cap.applied_events, 1);
        assert_eq!(at_cap.claim.unwrap().last_event_seq, created);
        assert!(
            store
                .replay_claim("claim", None)
                .unwrap_err()
                .to_string()
                .contains("8192")
        );
        Ok(())
    }
}
