//! Durable pre-registration ledger and append-only protocol verification trials.
//!
//! Only broker-sealed `ProtocolRunEvidence` values can be persisted as trials;
//! caller JSON cannot fabricate trial rows, and SQLite triggers prevent deleting
//! or rewriting failed runs to hide multiplicity (anti-p-hacking enforcement).

use super::{Change, NOW, StateStore, event};
use crate::retained::valid_id;
use ai4math_evidence::{
    BlindedControlSummary, PreRegistrationSpec, ProtocolRunEvidence, ProtocolSmellPolicy,
    ProtocolVerdict, TrialAuditSummary, TrialSample, compute_trial_audit,
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::Serialize;
use serde_json::{Value, json};

const PREREG_RECORDED: &str = "PROTOCOL_PREREGISTRATION_RECORDED";
const TRIAL_RECORDED: &str = "PROTOCOL_TRIAL_RECORDED";

/// Durable readback of a committed pre-registration.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RetainedPreRegistration {
    pub prereg_sha256: String,
    pub spec: PreRegistrationSpec,
    pub event_seq: i64,
    pub created_at: String,
}

/// Durable readback of one append-only protocol trial (never a sealed broker handle).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RetainedProtocolTrial {
    pub prereg_id: String,
    pub trial_index: u32,
    pub receipt_sha256: String,
    pub domain_id: String,
    pub backend_id: String,
    pub protocol_id: String,
    pub protocol_sha256: String,
    pub base_commit: String,
    pub candidate_id: String,
    pub candidate_sha256: String,
    pub rendered_artifacts_sha256: String,
    pub verifier_sha256: String,
    pub verdict: ProtocolVerdict,
    pub epistemic_level: String,
    pub epistemic_debts: Vec<String>,
    pub control_summary: BlindedControlSummary,
    pub surrogate_passed: Option<bool>,
    pub raw_receipt: Value,
    pub event_seq: i64,
    pub recorded_at: String,
}

fn has_migration_7(db: &Connection) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM state_migrations WHERE version=7)",
        [],
        |r| r.get(0),
    )?)
}

fn read_prereg_row(db: &Connection, prereg_id: &str) -> Result<Option<RetainedPreRegistration>> {
    let row: Option<(String, String, i64, String)> = db
        .query_row(
            "SELECT prereg_sha256, spec_json, event_seq, created_at
             FROM protocol_preregistrations WHERE prereg_id=?1",
            [prereg_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    let Some((prereg_sha256, spec_json, event_seq, created_at)) = row else {
        return Ok(None);
    };
    let spec = PreRegistrationSpec::parse(spec_json.as_bytes())
        .context("stored pre-registration JSON failed validation")?;
    ensure!(
        spec.fingerprint()? == prereg_sha256,
        "stored pre-registration digest mismatch"
    );
    Ok(Some(RetainedPreRegistration {
        prereg_sha256,
        spec,
        event_seq,
        created_at,
    }))
}

fn read_trial_rows(db: &Connection, prereg_id: &str) -> Result<Vec<RetainedProtocolTrial>> {
    let mut stmt = db.prepare(
        "SELECT prereg_id, trial_index, receipt_sha256, domain_id, backend_id, protocol_id,
                protocol_sha256, base_commit, candidate_id, candidate_sha256,
                rendered_artifacts_sha256, verifier_sha256, verdict, epistemic_level,
                epistemic_debts_json, control_summary_json, surrogate_passed,
                raw_receipt_json, event_seq, recorded_at
         FROM protocol_trials WHERE prereg_id=?1 ORDER BY trial_index ASC",
    )?;
    let rows = stmt.query_map([prereg_id], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, String>(5)?,
            r.get::<_, String>(6)?,
            r.get::<_, String>(7)?,
            r.get::<_, String>(8)?,
            r.get::<_, String>(9)?,
            r.get::<_, String>(10)?,
            r.get::<_, String>(11)?,
            r.get::<_, String>(12)?,
            r.get::<_, String>(13)?,
            r.get::<_, String>(14)?,
            r.get::<_, String>(15)?,
            r.get::<_, Option<i64>>(16)?,
            r.get::<_, String>(17)?,
            r.get::<_, i64>(18)?,
            r.get::<_, String>(19)?,
        ))
    })?;
    let mut out = Vec::new();
    for item in rows {
        let (
            prereg_id,
            trial_index,
            receipt_sha256,
            domain_id,
            backend_id,
            protocol_id,
            protocol_sha256,
            base_commit,
            candidate_id,
            candidate_sha256,
            rendered_artifacts_sha256,
            verifier_sha256,
            verdict_str,
            epistemic_level,
            debts_json,
            control_json,
            surrogate_int,
            raw_json,
            event_seq,
            recorded_at,
        ) = item?;
        let verdict: ProtocolVerdict = verdict_str
            .parse()
            .map_err(|e| anyhow::anyhow!("stored trial verdict invalid: {e}"))?;
        let epistemic_debts: Vec<String> = serde_json::from_str(&debts_json)?;
        let control_summary: BlindedControlSummary = serde_json::from_str(&control_json)?;
        let raw_receipt: Value = serde_json::from_str(&raw_json)?;
        out.push(RetainedProtocolTrial {
            prereg_id,
            trial_index: u32::try_from(trial_index)?,
            receipt_sha256,
            domain_id,
            backend_id,
            protocol_id,
            protocol_sha256,
            base_commit,
            candidate_id,
            candidate_sha256,
            rendered_artifacts_sha256,
            verifier_sha256,
            verdict,
            epistemic_level,
            epistemic_debts,
            control_summary,
            surrogate_passed: surrogate_int.map(|v| v != 0),
            raw_receipt,
            event_seq,
            recorded_at,
        });
    }
    Ok(out)
}

impl StateStore {
    /// Persist an immutable pre-registration commitment for a hypothesis family.
    /// Exact duplicate re-registrations return the existing row idempotently;
    /// conflicting specifications with the same `prereg_id` fail closed.
    pub fn register_protocol_preregistration(
        &mut self,
        change: Change<'_>,
        spec: &PreRegistrationSpec,
    ) -> Result<RetainedPreRegistration> {
        spec.validate()?;
        let prereg_sha256 = spec.fingerprint()?;
        let spec_json = serde_json::to_string(spec)?;

        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(existing) = read_prereg_row(&tx, &spec.prereg_id)? {
            ensure!(
                existing.prereg_sha256 == prereg_sha256,
                "pre-registration {:?} already exists with a different specification digest",
                spec.prereg_id
            );
            tx.commit()?;
            return Ok(existing);
        }
        let seq = event(
            &tx,
            change,
            PREREG_RECORDED,
            "protocol_preregistration",
            &spec.prereg_id,
            json!({
                "prereg_sha256": prereg_sha256,
                "hypothesis_family_id": spec.hypothesis_family_id,
                "protocol_sha256": spec.protocol_sha256,
                "primary_metric": spec.primary_metric,
                "max_trials": spec.max_trials,
                "correction": spec.correction.as_str(),
            }),
        )?;
        tx.execute(
            &format!(
                "INSERT INTO protocol_preregistrations(
                    prereg_id, prereg_sha256, hypothesis_family_id, protocol_sha256,
                    primary_metric, max_trials, significance_alpha, correction,
                    stopping_rule, spec_json, event_seq, created_at
                 ) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,{NOW})"
            ),
            params![
                spec.prereg_id,
                prereg_sha256,
                spec.hypothesis_family_id,
                spec.protocol_sha256,
                spec.primary_metric,
                i64::from(spec.max_trials),
                spec.significance_alpha,
                spec.correction.as_str(),
                spec.stopping_rule,
                spec_json,
                seq,
            ],
        )?;
        let saved = read_prereg_row(&tx, &spec.prereg_id)?
            .context("persisted pre-registration missing immediately after insert")?;
        tx.commit()?;
        Ok(saved)
    }

    /// Append one broker-sealed protocol verification trial to the immutable
    /// trial ledger for `prereg_id`. Rejects cross-protocol evidence and enforces
    /// the pre-registered `max_trials` cap.
    pub fn record_protocol_trial(
        &mut self,
        change: Change<'_>,
        prereg_id: &str,
        evidence: &dyn ProtocolRunEvidence,
        surrogate_passed: Option<bool>,
    ) -> Result<RetainedProtocolTrial> {
        ensure!(
            valid_id(evidence.receipt_sha256())
                && valid_id(evidence.protocol_sha256())
                && valid_id(evidence.candidate_sha256())
                && valid_id(evidence.rendered_artifacts_sha256())
                && valid_id(evidence.verifier_sha256()),
            "sealed protocol evidence contains malformed SHA-256 bindings"
        );
        let debts_json = serde_json::to_string(evidence.epistemic_debts())?;
        let control_json = serde_json::to_string(evidence.control_summary())?;
        let raw_receipt = evidence.raw_json()?;
        let raw_receipt_json = serde_json::to_string(&raw_receipt)?;

        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let prereg = read_prereg_row(&tx, prereg_id)?
            .with_context(|| format!("unknown protocol pre-registration {prereg_id:?}"))?;
        ensure!(
            evidence.protocol_sha256() == prereg.spec.protocol_sha256,
            "sealed trial protocol_sha256 does not match pre-registered protocol"
        );

        // Idempotent replay check for the exact same sealed receipt under this prereg_id.
        if let Some(existing_prereg) = tx
            .query_row(
                "SELECT prereg_id FROM protocol_trials WHERE receipt_sha256=?1",
                [evidence.receipt_sha256()],
                |r| r.get::<_, String>(0),
            )
            .optional()?
        {
            ensure!(
                existing_prereg == prereg_id,
                "receipt {} is already recorded under a different pre-registration {:?}",
                evidence.receipt_sha256(),
                existing_prereg
            );
            let existing = read_trial_rows(&tx, prereg_id)?
                .into_iter()
                .find(|t| t.receipt_sha256 == evidence.receipt_sha256())
                .context("existing trial row disappeared")?;
            tx.commit()?;
            return Ok(existing);
        }

        let current_count: i64 = tx.query_row(
            "SELECT COALESCE(MAX(trial_index), 0) FROM protocol_trials WHERE prereg_id=?1",
            [prereg_id],
            |r| r.get(0),
        )?;
        let next_index = current_count + 1;
        ensure!(
            next_index <= i64::from(prereg.spec.max_trials),
            "pre-registered trial budget exhausted ({}/{}) for {:?}",
            current_count,
            prereg.spec.max_trials,
            prereg_id
        );

        let seq = event(
            &tx,
            change,
            TRIAL_RECORDED,
            "protocol_trial",
            evidence.receipt_sha256(),
            json!({
                "prereg_id": prereg_id,
                "trial_index": next_index,
                "protocol_id": evidence.protocol_id(),
                "protocol_sha256": evidence.protocol_sha256(),
                "candidate_id": evidence.candidate_id(),
                "verdict": evidence.verdict().as_str(),
                "epistemic_level": evidence.epistemic_level(),
                "all_controls_passed": evidence.control_summary().all_controls_passed,
                "surrogate_passed": surrogate_passed,
            }),
        )?;

        tx.execute(
            &format!(
                "INSERT INTO protocol_trials(
                    prereg_id, trial_index, receipt_sha256, domain_id, backend_id,
                    protocol_id, protocol_sha256, base_commit, candidate_id,
                    candidate_sha256, rendered_artifacts_sha256, verifier_sha256,
                    verdict, epistemic_level, epistemic_debts_json, control_summary_json,
                    surrogate_passed, raw_receipt_json, event_seq, recorded_at
                 ) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,{NOW})"
            ),
            params![
                prereg_id,
                next_index,
                evidence.receipt_sha256(),
                evidence.domain_id(),
                evidence.backend_id(),
                evidence.protocol_id(),
                evidence.protocol_sha256(),
                evidence.base_commit(),
                evidence.candidate_id(),
                evidence.candidate_sha256(),
                evidence.rendered_artifacts_sha256(),
                evidence.verifier_sha256(),
                evidence.verdict().as_str(),
                evidence.epistemic_level(),
                debts_json,
                control_json,
                surrogate_passed.map(i64::from),
                raw_receipt_json,
                seq,
            ],
        )?;

        let saved = read_trial_rows(&tx, prereg_id)?
            .into_iter()
            .find(|t| t.receipt_sha256 == evidence.receipt_sha256())
            .context("persisted trial row missing immediately after insert")?;
        tx.commit()?;
        Ok(saved)
    }

    pub fn read_protocol_preregistration(
        &self,
        prereg_id: &str,
    ) -> Result<Option<RetainedPreRegistration>> {
        if !has_migration_7(&self.db)? {
            return Ok(None);
        }
        read_prereg_row(&self.db, prereg_id)
    }

    pub fn list_protocol_trials(&self, prereg_id: &str) -> Result<Vec<RetainedProtocolTrial>> {
        if !has_migration_7(&self.db)? {
            return Ok(Vec::new());
        }
        read_trial_rows(&self.db, prereg_id)
    }

    /// Compute an honest cumulative trial audit (multiplicity-adjusted alpha,
    /// selection-bias flag, and protocol-smell / Goodhart collapse signals) in
    /// one consistent SQLite read snapshot.
    pub fn audit_protocol_preregistration(
        &self,
        prereg_id: &str,
        policy: &ProtocolSmellPolicy,
    ) -> Result<Option<TrialAuditSummary>> {
        if !has_migration_7(&self.db)? {
            return Ok(None);
        }
        let tx = self.db.unchecked_transaction()?;
        let Some(prereg) = read_prereg_row(&tx, prereg_id)? else {
            return Ok(None);
        };
        let trials = read_trial_rows(&tx, prereg_id)?;
        let samples: Vec<TrialSample> = trials
            .iter()
            .map(|t| TrialSample {
                verdict: t.verdict,
                surrogate_passed: t.surrogate_passed,
            })
            .collect();
        Ok(Some(compute_trial_audit(&prereg.spec, &samples, policy)))
    }
}
