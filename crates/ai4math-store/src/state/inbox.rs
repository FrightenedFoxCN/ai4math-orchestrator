//! Agent-facing decision inbox writes.
//!
//! The inbox is a non-blocking queue for choices that must not silently change
//! mathematical state. A push records one immutable intent and its event; it
//! never resolves, dismisses, or advances a human checkpoint.

use super::{Change, IdempotencyConflict, NOW, StateStore, event, require_claim};
use crate::observation::InboxProjection;
use ai4math_core::contracts::{DecisionCategory, DecisionOption, DecisionPriority};
use anyhow::{Context, Result, ensure};
use rusqlite::{OptionalExtension, Transaction, params};
use serde_json::{Value, json};

pub struct NewDecision<'a> {
    pub id: &'a str,
    pub priority: DecisionPriority,
    pub category: DecisionCategory,
    pub summary: &'a str,
    pub leverage_score: f64,
    pub related_claims: &'a [&'a str],
    pub evidence_refs: &'a [&'a str],
    pub options: &'a [DecisionOption],
    pub details: Value,
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b':'))
}

fn bounded(value: &str, max: usize, field: &str) -> Result<()> {
    ensure!(
        !value.trim().is_empty() && value.len() <= max && !value.contains('\0'),
        "{field} must be nonblank, at most {max} UTF-8 bytes, and without NUL"
    );
    Ok(())
}

fn payload(decision: &NewDecision<'_>) -> Result<Value> {
    ensure!(
        decision.leverage_score.is_finite() && (0.0..=1.0).contains(&decision.leverage_score),
        "leverage_score must be finite and in 0..=1"
    );
    bounded(decision.summary, 4096, "summary")?;
    ensure!(
        decision.related_claims.len() <= 32,
        "at most 32 related claims"
    );
    ensure!(
        decision.evidence_refs.len() <= 64,
        "at most 64 evidence refs"
    );
    ensure!(
        (2..=8).contains(&decision.options.len()),
        "decision options must contain 2..=8 entries"
    );
    for claim in decision.related_claims {
        bounded(claim, 128, "related claim")?;
    }
    for reference in decision.evidence_refs {
        bounded(reference, 512, "evidence ref")?;
    }
    for option in decision.options {
        bounded(&option.label, 256, "option label")?;
        bounded(&option.tradeoff, 2048, "option tradeoff")?;
    }
    ensure!(
        decision.details.is_object(),
        "decision details must be a JSON object"
    );
    let bytes = serde_json::to_vec(&decision.details)?;
    ensure!(bytes.len() <= 64 * 1024, "decision details exceed 64 KiB");
    Ok(json!({
        "related_claims": decision.related_claims,
        "evidence_refs": decision.evidence_refs,
        "options": decision.options,
        "details": decision.details,
    }))
}

fn read_row(tx: &Transaction<'_>, id: &str) -> Result<Option<InboxProjection>> {
    Ok(tx
        .query_row(
            "SELECT inbox_id,priority,category,summary,leverage_score,created_at,payload_json \
             FROM decision_inbox WHERE inbox_id=?1",
            [id],
            |r| {
                Ok(InboxProjection {
                    inbox_id: r.get(0)?,
                    priority: r.get(1)?,
                    category: r.get(2)?,
                    summary: r.get(3)?,
                    leverage_score: r.get(4)?,
                    created_at: r.get(5)?,
                    payload: Some(
                        serde_json::from_str(&r.get::<_, String>(6)?)
                            .map_err(|_| rusqlite::Error::InvalidQuery)?,
                    ),
                    payload_truncated: false,
                })
            },
        )
        .optional()?)
}

impl StateStore {
    /// Append an immutable decision request. Exact replay is idempotent; reuse
    /// of an ID for different content is refused.
    pub fn push_decision(
        &mut self,
        decision: NewDecision<'_>,
        change: Change<'_>,
    ) -> Result<InboxProjection> {
        ensure!(valid_id(decision.id), "invalid decision inbox ID");
        let body = payload(&decision)?;
        self.write(|tx| {
            for claim in decision.related_claims {
                require_claim(tx, claim)?;
            }
            if let Some(stored) = read_row(tx, decision.id)? {
                if stored.priority != decision.priority.as_str()
                    || stored.category != decision.category.as_str()
                    || stored.summary != decision.summary
                    || stored.leverage_score != decision.leverage_score
                    || stored.payload.as_ref() != Some(&body)
                {
                    return Err(IdempotencyConflict.into());
                }
                return Ok(stored);
            }
            let seq = event(
                tx,
                change,
                "INBOX_CREATED",
                "inbox",
                decision.id,
                json!({
                    "priority": decision.priority,
                    "category": decision.category,
                    "summary": decision.summary,
                    "leverage_score": decision.leverage_score,
                    "payload": body,
                }),
            )?;
            tx.execute(
                &format!(
                    "INSERT INTO decision_inbox(inbox_id,created_at,priority,category,leverage_score,summary,payload_json,status)\
                     VALUES(?1,{NOW},?2,?3,?4,?5,?6,'PENDING')"
                ),
                params![
                    decision.id,
                    decision.priority.as_str(),
                    decision.category.as_str(),
                    decision.leverage_score,
                    decision.summary,
                    body.to_string(),
                ],
            )?;
            ensure!(seq > 0, "decision event sequence missing");
            read_row(tx, decision.id)?.context("pushed decision disappeared")
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ai4math_core::contracts::DecisionCategory;
    use std::path::Path;

    fn store(path: &Path) -> Result<StateStore> {
        let mut store = StateStore::open(path.join("state.sqlite"))?;
        store.create_claim(
            super::super::NewClaim {
                id: "C1",
                title: "Claim",
                informal_statement: "Statement",
                engine: "lean4",
                formal_target: None,
                module_path: None,
                critical: false,
            },
            Change {
                actor: "test",
                git_commit: None,
            },
        )?;
        Ok(store)
    }

    fn decision<'a>(
        id: &'a str,
        summary: &'a str,
        options: &'a [DecisionOption],
    ) -> NewDecision<'a> {
        NewDecision {
            id,
            priority: DecisionPriority::P1,
            category: DecisionCategory::HardBottleneckEscalation,
            summary,
            leverage_score: 0.5,
            related_claims: &["C1"],
            evidence_refs: &["event:1"],
            options,
            details: json!({"reason":"budget"}),
        }
    }

    #[test]
    fn push_is_idempotent_and_conflicts_fail_closed() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut store = store(dir.path())?;
        let change = Change {
            actor: "agent:chief",
            git_commit: None,
        };
        let options = [
            DecisionOption {
                label: "Continue".into(),
                tradeoff: "Spend more budget".into(),
                recommended: Some(true),
            },
            DecisionOption {
                label: "Pivot".into(),
                tradeoff: "Archive this route".into(),
                recommended: Some(false),
            },
        ];
        let first = store.push_decision(decision("DEC-1", "Choose route", &options), change)?;
        let replay = store.push_decision(decision("DEC-1", "Choose route", &options), change)?;
        assert_eq!(first, replay);
        assert!(
            store
                .push_decision(decision("DEC-1", "Different", &options), change)
                .is_err()
        );
        assert_eq!(store.inbox_page(8)?.items.len(), 1);
        Ok(())
    }

    #[test]
    fn push_rejects_unknown_claim_and_invalid_score() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut store = store(dir.path())?;
        let change = Change {
            actor: "agent:chief",
            git_commit: None,
        };
        let options = [
            DecisionOption {
                label: "Continue".into(),
                tradeoff: "Spend more budget".into(),
                recommended: Some(true),
            },
            DecisionOption {
                label: "Pivot".into(),
                tradeoff: "Archive this route".into(),
                recommended: Some(false),
            },
        ];
        let mut unknown = decision("DEC-2", "Unknown", &options);
        unknown.related_claims = &["missing"];
        assert!(store.push_decision(unknown, change).is_err());
        let mut score = decision("DEC-3", "Score", &options);
        score.leverage_score = f64::NAN;
        assert!(store.push_decision(score, change).is_err());
        Ok(())
    }
}
