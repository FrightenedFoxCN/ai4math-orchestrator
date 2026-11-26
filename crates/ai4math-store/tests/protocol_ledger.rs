use ai4math_evidence::{
    BlindedControlSummary, CorrectionMethod, ParsedDiagnostics, PreRegistrationSpec,
    ProtocolRunEvidence, ProtocolSmellPolicy, ProtocolSmellSignal, ProtocolVerdict, SealedReceipt,
    VerifierReport,
};
use ai4math_store::{Change, StateStore};
use anyhow::Result;
use rusqlite::Connection;
use serde_json::{Value, json};
use tempfile::tempdir;

struct StubProtocolEvidence {
    protocol_sha256: String,
    candidate_id: String,
    receipt_sha256: String,
    verdict: ProtocolVerdict,
    epistemic_level: String,
    debts: Vec<String>,
    controls: BlindedControlSummary,
    diagnostics: ParsedDiagnostics,
    reports: Vec<VerifierReport>,
    failures: Vec<String>,
}

// SAFETY: Test-only fixture in an integration test; production callers receive
// sealed `VerifiedProtocolRun` instances exclusively from `ai4math-broker`.
unsafe impl SealedReceipt for StubProtocolEvidence {
    fn domain_id(&self) -> &str {
        "compchem"
    }
    fn backend_id(&self) -> &str {
        "openmm_fep"
    }
    fn target_id(&self) -> &str {
        &self.candidate_id
    }
    fn base_commit(&self) -> &str {
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    }
    fn spec_sha256(&self) -> &str {
        &self.protocol_sha256
    }
    fn candidate_sha256(&self) -> &str {
        "1111111111111111111111111111111111111111111111111111111111111111"
    }
    fn inputs_sha256(&self) -> &str {
        "2222222222222222222222222222222222222222222222222222222222222222"
    }
    fn verifier_sha256(&self) -> &str {
        "3333333333333333333333333333333333333333333333333333333333333333"
    }
    fn epistemic_level(&self) -> &str {
        &self.epistemic_level
    }
    fn epistemic_debts(&self) -> &[String] {
        &self.debts
    }
    fn issued_unix_seconds(&self) -> u64 {
        1_700_000_000
    }
    fn elapsed_seconds(&self) -> f64 {
        0.5
    }
    fn receipt_sha256(&self) -> &str {
        &self.receipt_sha256
    }
    fn raw_json(&self) -> Result<Value> {
        Ok(json!({
            "receipt_sha256": self.receipt_sha256,
            "verdict": self.verdict.as_str(),
            "candidate_id": self.candidate_id,
        }))
    }
}

unsafe impl ProtocolRunEvidence for StubProtocolEvidence {
    fn protocol_id(&self) -> &str {
        "compchem_fep_v1"
    }
    fn protocol_sha256(&self) -> &str {
        &self.protocol_sha256
    }
    fn candidate_id(&self) -> &str {
        &self.candidate_id
    }
    fn rendered_artifacts_sha256(&self) -> &str {
        "4444444444444444444444444444444444444444444444444444444444444444"
    }
    fn verdict(&self) -> ProtocolVerdict {
        self.verdict
    }
    fn control_summary(&self) -> &BlindedControlSummary {
        &self.controls
    }
    fn verifier_reports(&self) -> &[VerifierReport] {
        &self.reports
    }
    fn aggregated_diagnostics(&self) -> &ParsedDiagnostics {
        &self.diagnostics
    }
    fn failure_reasons(&self) -> &[String] {
        &self.failures
    }
}

fn stub_trial(
    protocol_sha256: &str,
    tag: char,
    verdict: ProtocolVerdict,
    level: &str,
) -> StubProtocolEvidence {
    StubProtocolEvidence {
        protocol_sha256: protocol_sha256.to_owned(),
        candidate_id: format!("cand_{tag}"),
        receipt_sha256: std::iter::repeat_n(tag, 64).collect(),
        verdict,
        epistemic_level: level.to_owned(),
        debts: vec!["classical_ff_only".into()],
        controls: BlindedControlSummary::not_configured(),
        diagnostics: ParsedDiagnostics::default(),
        reports: vec![],
        failures: vec![],
    }
}

#[test]
fn preregistration_and_trials_are_immutable_bounded_and_prevent_phacking() -> Result<()> {
    let dir = tempdir()?;
    let db_path = dir.path().join("state.sqlite");
    let mut store = StateStore::open(&db_path)?;
    let protocol_sha = "a".repeat(64);
    let prereg = PreRegistrationSpec {
        version: 1,
        prereg_id: "PREREG-FEP-1".into(),
        hypothesis_family_id: "series_alpha".into(),
        protocol_sha256: protocol_sha.clone(),
        primary_metric: "ddg_kcal_mol".into(),
        max_trials: 3,
        significance_alpha: Some(0.06),
        correction: CorrectionMethod::Bonferroni,
        stopping_rule: "stop at 3 trials".into(),
    };
    let actor = Change {
        actor: "broker",
        git_commit: Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
    };

    let first = store.register_protocol_preregistration(actor, &prereg)?;
    // Idempotent replay succeeds.
    let replay = store.register_protocol_preregistration(actor, &prereg)?;
    assert_eq!(first, replay);

    // Conflicting re-registration with higher budget fails closed.
    let mut inflated = prereg.clone();
    inflated.max_trials = 50;
    assert!(
        store
            .register_protocol_preregistration(actor, &inflated)
            .is_err()
    );

    // Wrong protocol_sha256 trial is rejected.
    let wrong_proto = stub_trial(&"f".repeat(64), '0', ProtocolVerdict::Passed, "E4a");
    assert!(
        store
            .record_protocol_trial(actor, &prereg.prereg_id, &wrong_proto, Some(true))
            .is_err()
    );

    // Record 3 trials (2 sanity failures, 1 pass).
    let t1 = stub_trial(&protocol_sha, '1', ProtocolVerdict::SanityGateFailed, "E0");
    let t2 = stub_trial(&protocol_sha, '2', ProtocolVerdict::SanityGateFailed, "E0");
    let t3 = stub_trial(&protocol_sha, '3', ProtocolVerdict::Passed, "E4a");

    let r1 = store.record_protocol_trial(actor, &prereg.prereg_id, &t1, Some(true))?;
    assert_eq!(r1.trial_index, 1);
    // Exact duplicate replay returns existing row without incrementing index.
    let r1_dup = store.record_protocol_trial(actor, &prereg.prereg_id, &t1, Some(true))?;
    assert_eq!(r1_dup.trial_index, 1);

    let r2 = store.record_protocol_trial(actor, &prereg.prereg_id, &t2, Some(true))?;
    assert_eq!(r2.trial_index, 2);
    let r3 = store.record_protocol_trial(actor, &prereg.prereg_id, &t3, Some(true))?;
    assert_eq!(r3.trial_index, 3);

    // Trial 4 exceeds max_trials=3 and is hard-rejected.
    let t4 = stub_trial(&protocol_sha, '4', ProtocolVerdict::Passed, "E4a");
    assert!(
        store
            .record_protocol_trial(actor, &prereg.prereg_id, &t4, Some(true))
            .is_err()
    );

    // Direct SQLite tampering (deleting or updating failed trials or pre-reg) is blocked by triggers!
    drop(store);
    let raw = Connection::open(&db_path)?;
    assert!(
        raw.execute("DELETE FROM protocol_trials WHERE trial_index=1", [])
            .is_err()
    );
    assert!(
        raw.execute(
            "UPDATE protocol_trials SET verdict='passed' WHERE trial_index=1",
            []
        )
        .is_err()
    );
    assert!(
        raw.execute(
            "UPDATE protocol_preregistrations SET max_trials=999 WHERE prereg_id='PREREG-FEP-1'",
            []
        )
        .is_err()
    );
    assert!(
        raw.execute(
            "DELETE FROM protocol_preregistrations WHERE prereg_id='PREREG-FEP-1'",
            []
        )
        .is_err()
    );
    drop(raw);

    // Readonly audit returns honest cumulative counts, Bonferroni alpha, and smell signals.
    let ro = StateStore::open_readonly(&db_path)?;
    let audit = ro
        .audit_protocol_preregistration(&prereg.prereg_id, &ProtocolSmellPolicy::default())?
        .expect("audit present");
    assert_eq!(audit.trials_consumed, 3);
    assert_eq!(audit.passed_trials, 1);
    assert_eq!(audit.sanity_failed_trials, 2);
    assert!((audit.effective_alpha.unwrap() - 0.02).abs() < 1e-12);
    assert!(audit.selection_bias_warning);
    assert!(audit.smell_signals.iter().any(|s| matches!(
        s,
        ProtocolSmellSignal::SystematicSanityFailure {
            sanity_failed_trials: 2,
            total_trials: 3,
            ..
        }
    )));
    Ok(())
}

#[test]
fn readonly_legacy_database_without_migration_7_reads_empty_without_error() -> Result<()> {
    let dir = tempdir()?;
    let db_path = dir.path().join("legacy.sqlite");
    let store = StateStore::open(&db_path)?;
    drop(store);

    let raw = Connection::open(&db_path)?;
    raw.execute_batch(
        "DROP TABLE protocol_trials;
         DROP TABLE protocol_preregistrations;
         DELETE FROM state_migrations WHERE version=7;",
    )?;
    drop(raw);

    let ro = StateStore::open_readonly(&db_path)?;
    assert!(ro.read_protocol_preregistration("PREREG-1")?.is_none());
    assert!(ro.list_protocol_trials("PREREG-1")?.is_empty());
    assert!(
        ro.audit_protocol_preregistration("PREREG-1", &ProtocolSmellPolicy::default())?
            .is_none()
    );

    // Reopening writable cleanly installs migration 7.
    drop(ro);
    let _rw = StateStore::open(&db_path)?;
    let raw = Connection::open(&db_path)?;
    let installed: bool = raw.query_row(
        "SELECT EXISTS(SELECT 1 FROM state_migrations WHERE version=7)",
        [],
        |r| r.get(0),
    )?;
    assert!(installed);
    Ok(())
}
