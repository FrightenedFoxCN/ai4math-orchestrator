use ai4math_core::{ComputeJobStatus, ExecutionTargetKind, GpuKind, GpuRequest, ResourceRequest};
use ai4math_evidence::{
    ArtifactTemplate, BlindedArrayBundleRequest, BlindedControlCase, BlindedControlPack,
    BlindedControlPolicy, BlindedControlSummary, CandidateSubmission, MetricRule, NamedMetricRule,
    OrthogonalPolicy, ParamValue, ParsedDiagnostics, ProtocolRunEvidence, ProtocolSpec,
    ProtocolVerdict, SealedReceipt, SlotKind, SlotSpec, SlotValue, TextProfile, VerifierReport,
    build_blinded_array_bundle,
};
use ai4math_store::{ActiveComputeQuota, Change, NewComputeJob, StateStore};
use anyhow::Result;
use rusqlite::Connection;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use tempfile::tempdir;

struct StubReceipt {
    protocol_sha256: String,
    candidate_sha256: String,
    receipt_sha256: String,
    controls: BlindedControlSummary,
    diag: ParsedDiagnostics,
    reports: Vec<VerifierReport>,
    failures: Vec<String>,
}

// SAFETY: Test-only fixture inside integration test crate.
unsafe impl SealedReceipt for StubReceipt {
    fn domain_id(&self) -> &str {
        "compchem"
    }
    fn backend_id(&self) -> &str {
        "openmm_fep"
    }
    fn target_id(&self) -> &str {
        "cand_1"
    }
    fn base_commit(&self) -> &str {
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    }
    fn spec_sha256(&self) -> &str {
        &self.protocol_sha256
    }
    fn candidate_sha256(&self) -> &str {
        &self.candidate_sha256
    }
    fn inputs_sha256(&self) -> &str {
        "1111111111111111111111111111111111111111111111111111111111111111"
    }
    fn verifier_sha256(&self) -> &str {
        "2222222222222222222222222222222222222222222222222222222222222222"
    }
    fn epistemic_level(&self) -> &str {
        "E4a"
    }
    fn epistemic_debts(&self) -> &[String] {
        &[]
    }
    fn issued_unix_seconds(&self) -> u64 {
        1_700_000_000
    }
    fn elapsed_seconds(&self) -> f64 {
        12.0
    }
    fn receipt_sha256(&self) -> &str {
        &self.receipt_sha256
    }
    fn raw_json(&self) -> Result<Value> {
        Ok(json!({"receipt_sha256": self.receipt_sha256}))
    }
}

unsafe impl ProtocolRunEvidence for StubReceipt {
    fn protocol_id(&self) -> &str {
        "fep_v1"
    }
    fn protocol_sha256(&self) -> &str {
        &self.protocol_sha256
    }
    fn candidate_id(&self) -> &str {
        "cand_1"
    }
    fn rendered_artifacts_sha256(&self) -> &str {
        "3333333333333333333333333333333333333333333333333333333333333333"
    }
    fn verdict(&self) -> ProtocolVerdict {
        ProtocolVerdict::Passed
    }
    fn control_summary(&self) -> &BlindedControlSummary {
        &self.controls
    }
    fn verifier_reports(&self) -> &[VerifierReport] {
        &self.reports
    }
    fn aggregated_diagnostics(&self) -> &ParsedDiagnostics {
        &self.diag
    }
    fn failure_reasons(&self) -> &[String] {
        &self.failures
    }
}

#[test]
fn compute_jobs_enforce_monotonic_lifecycle_quota_and_immutability() -> Result<()> {
    let dir = tempdir()?;
    let db_path = dir.path().join("state.sqlite");
    let mut store = StateStore::open(&db_path)?;

    let controls = BlindedControlPack {
        version: 1,
        protocol_id: "fep_v1".into(),
        positive_controls: vec![BlindedControlCase {
            control_id: "pos_1".into(),
            slot_values: BTreeMap::from([("smiles".into(), SlotValue::Text("c1ccccc1".into()))]),
        }],
        negative_controls: vec![],
    };
    let protocol = ProtocolSpec {
        version: 1,
        protocol_id: "fep_v1".into(),
        domain_id: "compchem".into(),
        backend_id: "openmm_fep".into(),
        epistemic_ceiling: ai4math_core::ScientificTrust::E4a,
        default_debts: vec![],
        pinned_inputs: BTreeMap::new(),
        frozen_parameters: BTreeMap::from([("temp".into(), ParamValue::Float(300.0))]),
        slots: vec![SlotSpec {
            name: "smiles".into(),
            kind: SlotKind::Text {
                max_len: 128,
                profile: TextProfile::Smiles,
            },
        }],
        artifact_templates: vec![ArtifactTemplate {
            path: "job.cfg".into(),
            template: "t={{param:temp}}\ns={{slot:smiles}}\n".into(),
        }],
        sanity_gates: vec![NamedMetricRule {
            rule_id: "ok".into(),
            rule: MetricRule::EqualsBool {
                metric: "ok".into(),
                expected: true,
            },
        }],
        acceptance_criteria: vec![NamedMetricRule {
            rule_id: "ddg".into(),
            rule: MetricRule::MaxFloat {
                metric: "ddg".into(),
                max: -1.0,
            },
        }],
        orthogonal_policy: OrthogonalPolicy::default(),
        control_policy: BlindedControlPolicy {
            control_pack_sha256: Some(controls.fingerprint()?),
            min_positive_controls: 1,
            min_negative_controls: 0,
        },
        preregistration_required: false,
        resource_ceiling: None,
    };
    let proto_sha = protocol.protocol_sha256()?;
    let sub = CandidateSubmission {
        version: 1,
        protocol_sha256: proto_sha.clone(),
        candidate_id: "cand_1".into(),
        slot_values: BTreeMap::from([("smiles".into(), SlotValue::Text("CCO".into()))]),
    };
    let compiled_cand = protocol.compile_submission(&sub)?;
    let compiled_ctrl = controls.compile_for(&protocol)?;
    let resources = ResourceRequest {
        cpu_cores: 8,
        memory_mib: 32_768,
        gpu: Some(GpuRequest {
            count: 1,
            min_vram_mib: 24_576,
            kind: GpuKind::Cuda,
            exclusive: true,
        }),
        scratch_mib: 4096,
        max_wall_seconds: 3600,
    };
    let (bundle, plan) = build_blinded_array_bundle(BlindedArrayBundleRequest {
        job_id: "job_slurm_42",
        base_commit: &"a".repeat(40),
        protocol: &protocol,
        candidate: &compiled_cand,
        controls: Some(&compiled_ctrl),
        resources: resources.clone(),
        container_image_sha256: Some("b".repeat(64)),
        cluster_mounts: vec![],
        output_file_per_task: "result.json",
        shuffle_nonce: &[9u8; 32],
    })?;
    let bundle_sha = bundle.bundle_sha256()?;
    let actor = Change {
        actor: "broker:slurm",
        git_commit: Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
    };

    let saved = store.record_compute_job_submitted(
        actor,
        &NewComputeJob {
            job_id: "job_slurm_42",
            prereg_id: None,
            protocol_id: &protocol.protocol_id,
            protocol_sha256: &proto_sha,
            base_commit: &"a".repeat(40),
            candidate_id: "cand_1",
            candidate_sha256: compiled_cand.candidate_sha256(),
            target_kind: ExecutionTargetKind::Slurm,
            cluster_or_host_id: "hpc_a100",
            external_job_id: Some("90001"),
            staging_ref: "/scratch/job_slurm_42",
            bundle_sha256: &bundle_sha,
            resources: &resources,
            blinded_plan: &plan,
            branch_tag: Some("branch_lead_opt"),
            initial_status: ComputeJobStatus::Queued,
        },
    )?;
    assert_eq!(saved.array_len, 2);
    assert_eq!(saved.requested_gpus_total, 2);
    assert_eq!(
        store.active_compute_quota("hpc_a100")?,
        ActiveComputeQuota {
            active_jobs: 1,
            active_gpus: 2,
        }
    );
    assert_eq!(
        store
            .list_active_compute_jobs(Some(ExecutionTargetKind::Slurm), Some("branch_lead_opt"))?
            .len(),
        1
    );

    // Transition queued -> running -> completed with sealed receipt.
    store.update_compute_job_status(
        actor,
        "job_slurm_42",
        ComputeJobStatus::Running,
        Some("gpu-node-01"),
        None,
    )?;

    // Mismatched candidate_sha256 receipt is rejected!
    let wrong_receipt = StubReceipt {
        protocol_sha256: proto_sha.clone(),
        candidate_sha256: "f".repeat(64),
        receipt_sha256: "c".repeat(64),
        controls: BlindedControlSummary::not_configured(),
        diag: ParsedDiagnostics::default(),
        reports: vec![],
        failures: vec![],
    };
    assert!(
        store
            .update_compute_job_status(
                actor,
                "job_slurm_42",
                ComputeJobStatus::Completed,
                None,
                Some(&wrong_receipt),
            )
            .is_err()
    );

    let good_receipt = StubReceipt {
        protocol_sha256: proto_sha,
        candidate_sha256: compiled_cand.candidate_sha256().to_owned(),
        receipt_sha256: "e".repeat(64),
        controls: BlindedControlSummary::not_configured(),
        diag: ParsedDiagnostics::default(),
        reports: vec![],
        failures: vec![],
    };
    let done = store.update_compute_job_status(
        actor,
        "job_slurm_42",
        ComputeJobStatus::Completed,
        Some("sealed"),
        Some(&good_receipt),
    )?;
    assert_eq!(done.status, ComputeJobStatus::Completed);
    assert_eq!(
        done.sealed_receipt_sha256.as_deref(),
        Some("e".repeat(64).as_str())
    );
    assert_eq!(
        store.active_compute_quota("hpc_a100")?,
        ActiveComputeQuota::default()
    );

    // Non-monotonic rollback Completed -> Running is blocked by SQLite trigger!
    assert!(
        store
            .update_compute_job_status(
                actor,
                "job_slurm_42",
                ComputeJobStatus::Running,
                None,
                None,
            )
            .is_err()
    );

    // Direct SQLite DELETE or blinded_plan tampering is blocked by triggers!
    drop(store);
    let raw = Connection::open(&db_path)?;
    assert!(
        raw.execute("DELETE FROM compute_jobs WHERE job_id='job_slurm_42'", [])
            .is_err()
    );
    assert!(
        raw.execute(
            "UPDATE compute_jobs SET blinded_plan_json='{}' WHERE job_id='job_slurm_42'",
            []
        )
        .is_err()
    );
    Ok(())
}

#[test]
fn readonly_legacy_database_without_migration_8_reads_empty_cleanly() -> Result<()> {
    let dir = tempdir()?;
    let db_path = dir.path().join("legacy8.sqlite");
    let store = StateStore::open(&db_path)?;
    drop(store);

    let raw = Connection::open(&db_path)?;
    raw.execute_batch("DROP TABLE compute_jobs; DELETE FROM state_migrations WHERE version=8;")?;
    drop(raw);

    let ro = StateStore::open_readonly(&db_path)?;
    assert!(ro.read_compute_job("job_1")?.is_none());
    assert!(ro.list_active_compute_jobs(None, None)?.is_empty());
    assert_eq!(
        ro.active_compute_quota("hpc")?,
        ActiveComputeQuota::default()
    );
    Ok(())
}
