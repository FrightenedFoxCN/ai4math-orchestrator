//! Managed downstream regression alerts. Sealed observations do not confer trust.
use super::{Change, StateStore, event, tasks::now_ms};
use crate::retained::valid_id;
use ai4math_broker::RegressionFailure;
use ai4math_core::{Engine, validate_source_path};
use anyhow::{Result, ensure};
use rusqlite::{OptionalExtension, Transaction, params};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::path::Path;

pub(crate) const MELTDOWN: &str = "BROKER_REGRESSION_MELTDOWN";
pub(crate) const DISTINCT_CANDIDATES: i64 = 3;
pub(crate) const WINDOW_SECONDS: i64 = 15 * 60;
const WINDOW_MS: i64 = WINDOW_SECONDS * 1000;
const CONTEXT_LIMIT: i64 = 4096;

fn valid_module(engine: Engine, module: &str) -> bool {
    module.len() <= 256 && validate_source_path(engine, module).is_ok()
}

pub(crate) fn public_failed_module(module: &str) -> bool {
    [Engine::Lean4, Engine::Agda, Engine::Rocq]
        .into_iter()
        .any(|engine| valid_module(engine, module))
}

/// Only the typed public API can supply these scalars in production.
#[derive(Clone, Copy)]
struct FailureFields<'a> {
    base_commit: &'a str,
    engine: Engine,
    target_module: &'a str,
    target_decl: &'a str,
    failed_module: &'a str,
    candidate_sha256: &'a str,
    compiler_sha256: &'a str,
}

impl StateStore {
    /// Record a sealed downstream failure while the coordinator holds both the
    /// project and Git locks and has just checked that live main equals the
    /// failure's base. SQLite can validate the ledger binding but not Git HEAD.
    /// Returns a sequence only on the third distinct candidate in a 15-minute
    /// episode for the exact context. A retry never refreshes an episode's TTL.
    /// Once emitted, the context stays latched until the observed base changes;
    /// successful promotion advances that base atomically with finalization.
    pub fn record_managed_regression_failure(
        &mut self,
        repo: &str,
        claim_id: &str,
        failure: &RegressionFailure,
    ) -> Result<Option<i64>> {
        let fields = FailureFields {
            base_commit: failure.base_commit(),
            engine: failure.engine(),
            target_module: failure.target_module(),
            target_decl: failure.target_decl(),
            failed_module: failure.failed_module(),
            candidate_sha256: failure.candidate_sha256(),
            compiler_sha256: failure.compiler_sha256(),
        };
        self.write(|tx| record_failure_at(tx, repo, claim_id, fields, now_ms(tx)?))
    }
}

/// An accepted observation of a different base starts a new generation; a
/// return to a previously seen commit after such a change is a new generation.
/// A normal promotion calls this only when its Git update has been confirmed.
pub(super) fn observe_base(tx: &Transaction<'_>, base: &str) -> Result<()> {
    let changed = tx.execute(
        "INSERT INTO managed_regression_base(singleton,base_commit) VALUES(1,?1)
         ON CONFLICT(singleton) DO UPDATE SET base_commit=excluded.base_commit
         WHERE managed_regression_base.base_commit<>excluded.base_commit",
        [base],
    )?;
    if changed != 0 {
        tx.execute("DELETE FROM managed_regression_incidents", [])?;
    }
    Ok(())
}

fn record_failure_at(
    tx: &Transaction<'_>,
    repo: &str,
    claim_id: &str,
    f: FailureFields<'_>,
    now_ms: i64,
) -> Result<Option<i64>> {
    ensure!(
        repo.len() <= 4096 && Path::new(repo).is_absolute() && !repo.chars().any(char::is_control),
        "invalid managed regression repository"
    );
    ensure!(
        !claim_id.is_empty()
            && claim_id.len() <= 256
            && claim_id == claim_id.trim()
            && !claim_id.chars().any(char::is_control),
        "invalid managed regression Claim ID"
    );
    ensure!(
        matches!(f.base_commit.len(), 40 | 64)
            && f.base_commit
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            && valid_id(f.candidate_sha256)
            && valid_id(f.compiler_sha256),
        "invalid managed regression fingerprints"
    );
    ensure!(
        valid_module(f.engine, f.target_module)
            && valid_module(f.engine, f.failed_module)
            && !f.failed_module.eq_ignore_ascii_case(f.target_module)
            && !f.target_decl.is_empty()
            && f.target_decl.len() <= 256
            && !f
                .target_decl
                .chars()
                .any(|c| c.is_control() || c.is_whitespace() || c == '#'),
        "invalid managed regression target or downstream module"
    );
    ensure!(now_ms >= 0, "invalid scheduler time");

    let target = format!("{}#{}", f.target_module, f.target_decl);
    let eligible: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM claims c
            JOIN managed_repo r ON r.singleton=1 AND r.repo=?1
            WHERE c.claim_id=?2 AND c.engine=?3 AND c.module_path=?4 AND c.formal_target=?5
              AND c.stale=0 AND c.effective_stale=0
              AND c.local_trust_level NOT IN ('L2b','L5')
              AND c.effective_trust_level NOT IN ('L2b','L5')
              AND NOT EXISTS(SELECT 1 FROM promotion_intent)
              AND NOT EXISTS(SELECT 1 FROM pivot_resume_intent))",
        params![repo, claim_id, f.engine.as_str(), f.target_module, target],
        |r| r.get(0),
    )?;
    ensure!(
        eligible,
        "managed regression requires the bound repository, exact open nonstale Claim, and no pending promotion or Pivot"
    );

    let context = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&(
            "ai4math:managed-regression:v1",
            repo,
            f.base_commit,
            claim_id,
            f.engine.as_str(),
            f.target_module,
            f.target_decl,
            f.compiler_sha256,
            f.failed_module,
        ))?)
    );
    observe_base(tx, f.base_commit)?;
    // A pending episode is anchored to its first observation. Replays do not
    // extend it. At its deadline (or on rewind before its anchor), forget the
    // pending evidence, but never expire an emitted latch on the same base.
    tx.execute(
        "DELETE FROM managed_regression_incidents
         WHERE opened_at_ms<=?1 OR opened_at_ms>?2",
        params![now_ms - WINDOW_MS, now_ms],
    )?;
    let incident: Option<(Option<String>, Option<String>, Option<i64>)> = tx
        .query_row(
            "SELECT first_sha256,second_sha256,emitted_event_seq
         FROM managed_regression_incidents WHERE context_sha256=?1",
            [&context],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    match incident {
        Some((_, _, Some(_))) => return Ok(None),
        Some((ref first, ref second, None))
            if first.as_deref() == Some(f.candidate_sha256)
                || second.as_deref() == Some(f.candidate_sha256) =>
        {
            return Ok(None);
        }
        Some((_, None, None)) => {
            tx.execute(
                "UPDATE managed_regression_incidents SET second_sha256=?2 WHERE context_sha256=?1",
                params![context, f.candidate_sha256],
            )?;
            return Ok(None);
        }
        None => {
            let count: i64 = tx.query_row(
                "SELECT count(*) FROM managed_regression_incidents",
                [],
                |r| r.get(0),
            )?;
            // Fail closed, not by evicting latches (which could permit duplicate
            // alerts), nor by combining unrelated contexts under pressure.
            ensure!(
                count < CONTEXT_LIMIT,
                "managed regression context capacity reached"
            );
            tx.execute(
                "INSERT INTO managed_regression_incidents(context_sha256,opened_at_ms,first_sha256)
                 VALUES(?1,?2,?3)",
                params![context, now_ms, f.candidate_sha256],
            )?;
            return Ok(None);
        }
        Some((_, Some(_), None)) => {}
    }

    let seq = event(
        tx,
        Change {
            actor: "broker",
            git_commit: Some(f.base_commit),
        },
        MELTDOWN,
        "claim",
        claim_id,
        json!({"failed_module": f.failed_module, "distinct_candidates": DISTINCT_CANDIDATES, "window_seconds": WINDOW_SECONDS}),
    )?;
    tx.execute(
        "UPDATE managed_regression_incidents
         SET opened_at_ms=NULL,first_sha256=NULL,second_sha256=NULL,emitted_event_seq=?2
         WHERE context_sha256=?1",
        params![context, seq],
    )?;
    Ok(Some(seq))
}

#[cfg(test)]
mod tests;
