//! End-to-end integration tests for multi-dimensional GPU resource scheduling,
//! unified `ComputeBackend` / `SlurmTransport` execution, blinded Job Array
//! unblinding, daemon restart recovery, and circuit-breaker batch `scancel`.

use ai4math_broker::{
    AsyncProtocolSubmitRequest, Broker, GpuDeviceSpec, LocalComputeBackend, LocalComputeCapacity,
    LocalComputePool, LocalTaskExecutor, PendingProtocolJob, SlurmComputeBackend,
    SlurmContainerRuntime, SlurmExecutionProfile, SlurmTransport, SubmittedComputeJob,
    UNATTESTED_REMOTE_HPC_DEBT,
};
use ai4math_core::{
    ComputeJobStatus, ExecutionTargetKind, GpuKind, GpuRequest, ResourceRequest, ScientificTrust,
    SlurmClusterPolicy,
};
use ai4math_evidence::{
    ArtifactTemplate, BlindedControlCase, BlindedControlPack, BlindedControlPolicy,
    CandidateSubmission, ClusterMountEntry, CorrectionMethod, DiagnosticValue, FetchedJobOutputs,
    MetricRule, NamedMetricRule, OrthogonalPolicy, ParamValue, ParsedDiagnostics,
    PreRegistrationSpec, ProtocolSmellPolicy, ProtocolSpec, ProtocolVerdict, SealedReceipt,
    SlotKind, SlotSpec, SlotValue, SlurmFlag, SlurmJobId, SlurmJobObservation, SlurmJobState,
    TextProfile,
};
use ai4math_store::{Change, NewComputeJob, StateStore};
use anyhow::{Context, Result};
use std::{
    collections::{BTreeMap, HashMap},
    fs,
    path::Path,
    sync::{Arc, Mutex, atomic::AtomicBool},
};
use tempfile::tempdir;

fn fep_protocol_and_controls() -> Result<(ProtocolSpec, BlindedControlPack)> {
    let controls = BlindedControlPack {
        version: 1,
        protocol_id: "openmm_fep_gpu_v1".into(),
        positive_controls: vec![BlindedControlCase {
            control_id: "hidden_pos_binder".into(),
            slot_values: BTreeMap::from([(
                "ligand_smiles".into(),
                SlotValue::Text("c1ccc(O)cc1".into()),
            )]),
        }],
        negative_controls: vec![BlindedControlCase {
            control_id: "hidden_neg_decoy".into(),
            slot_values: BTreeMap::from([(
                "ligand_smiles".into(),
                SlotValue::Text("CCCCCCCC".into()),
            )]),
        }],
    };
    let protocol = ProtocolSpec {
        version: 1,
        protocol_id: "openmm_fep_gpu_v1".into(),
        domain_id: "compchem".into(),
        backend_id: "openmm_fep".into(),
        epistemic_ceiling: ScientificTrust::E4a,
        default_debts: vec!["classical_ff_only".into()],
        pinned_inputs: BTreeMap::from([("receptor_pdb".into(), "a".repeat(64))]),
        frozen_parameters: BTreeMap::from([("temp_k".into(), ParamValue::Float(300.0))]),
        slots: vec![SlotSpec {
            name: "ligand_smiles".into(),
            kind: SlotKind::Text {
                max_len: 128,
                profile: TextProfile::Smiles,
            },
        }],
        artifact_templates: vec![ArtifactTemplate {
            path: "fep.cfg".into(),
            template: "temp={{param:temp_k}}\nsmiles={{slot:ligand_smiles}}\n".into(),
        }],
        sanity_gates: vec![
            NamedMetricRule {
                rule_id: "overlap_ok".into(),
                rule: MetricRule::RangeFloat {
                    metric: "min_overlap".into(),
                    min: 0.03,
                    max: 1.0,
                },
            },
            NamedMetricRule {
                rule_id: "no_lincs".into(),
                rule: MetricRule::EqualsInt {
                    metric: "lincs_warnings".into(),
                    expected: 0,
                },
            },
        ],
        acceptance_criteria: vec![NamedMetricRule {
            rule_id: "ddg_hit".into(),
            rule: MetricRule::MaxFloat {
                metric: "ddg_kcal_mol".into(),
                max: -1.5,
            },
        }],
        orthogonal_policy: OrthogonalPolicy::default(),
        control_policy: BlindedControlPolicy {
            control_pack_sha256: Some(controls.fingerprint()?),
            min_positive_controls: 1,
            min_negative_controls: 1,
        },
        preregistration_required: true,
        resource_ceiling: Some(ResourceRequest {
            cpu_cores: 8,
            memory_mib: 32_768,
            gpu: Some(GpuRequest {
                count: 1,
                min_vram_mib: 24_576,
                kind: GpuKind::Cuda,
                exclusive: true,
            }),
            scratch_mib: 10_240,
            max_wall_seconds: 7_200,
        }),
    };
    Ok((protocol, controls))
}

fn eval_smiles_to_json(cfg_text: &str) -> String {
    let ddg = if cfg_text.contains("smiles=CCCCCCCC") {
        0.4 // decoy cleanly rejected
    } else if cfg_text.contains("smiles=c1ccc(O)cc1") {
        -2.3 // positive control passes
    } else {
        -2.8 // candidate passes
    };
    serde_json::to_string(&ParsedDiagnostics {
        metrics: BTreeMap::from([
            ("min_overlap".into(), DiagnosticValue::Float(0.14)),
            ("lincs_warnings".into(), DiagnosticValue::Int(0)),
            ("ddg_kcal_mol".into(), DiagnosticValue::Float(ddg)),
        ]),
    })
    .unwrap()
}

fn parse_diag_bytes(bytes: &[u8]) -> Result<ParsedDiagnostics> {
    Ok(serde_json::from_slice(bytes)?)
}

#[test]
fn local_gpu_backend_binds_cuda_visible_devices_unblinds_and_releases_lease() -> Result<()> {
    let dir = tempdir()?;
    let mut store = StateStore::open(dir.path().join("local.sqlite"))?;
    let broker = Broker::new();
    let (protocol, controls) = fep_protocol_and_controls()?;
    let proto_sha = protocol.protocol_sha256()?;

    let observed_cuda_envs: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    struct LocalFepExec {
        observed_cuda_envs: Arc<Mutex<Vec<String>>>,
    }
    impl LocalTaskExecutor for LocalFepExec {
        fn execute_task(
            &self,
            task_dir: &Path,
            _idx: u32,
            env_vars: &[(&'static str, String)],
            _cancelled: &AtomicBool,
        ) -> Result<i32> {
            let cuda = env_vars
                .iter()
                .find(|(k, _)| *k == "CUDA_VISIBLE_DEVICES")
                .map(|(_, v)| v.clone())
                .unwrap_or_default();
            self.observed_cuda_envs.lock().unwrap().push(cuda);
            let cfg = fs::read_to_string(task_dir.join("fep.cfg"))?;
            fs::write(task_dir.join("result.json"), eval_smiles_to_json(&cfg))?;
            Ok(0)
        }
    }

    let pool = LocalComputePool::new(LocalComputeCapacity {
        total_cpu_cores: 16,
        total_memory_mib: 65_536,
        total_scratch_mib: 100_000,
        gpus: vec![
            GpuDeviceSpec {
                index: 0,
                kind: GpuKind::Cuda,
                total_vram_mib: 24_576,
                unified_memory: false,
            },
            GpuDeviceSpec {
                index: 1,
                kind: GpuKind::Cuda,
                total_vram_mib: 24_576,
                unified_memory: false,
            },
        ],
    })?;
    let backend = LocalComputeBackend::new(
        "workstation_gpu_01",
        Arc::clone(&pool),
        dir.path().join("local-jobs"),
        Arc::new(LocalFepExec {
            observed_cuda_envs: Arc::clone(&observed_cuda_envs),
        }),
    )?;

    let sub = CandidateSubmission {
        version: 1,
        protocol_sha256: proto_sha.clone(),
        candidate_id: "cand_local_gpu".into(),
        slot_values: BTreeMap::from([(
            "ligand_smiles".into(),
            SlotValue::Text("CC(=O)Oc1ccccc1".into()),
        )]),
    };
    let pending = broker.submit_protocol_job(
        &backend,
        AsyncProtocolSubmitRequest {
            job_id: "job_local_gpu_1",
            base_commit: &"b".repeat(40),
            protocol: &protocol,
            submission: &sub,
            blinded_controls: Some(&controls),
            resources: protocol.resource_ceiling.clone().unwrap(),
            verifier_sha256: &"c".repeat(64),
            backend_max_positive_ceiling: ScientificTrust::E4a,
            backend_max_refutation_ceiling: Some(ScientificTrust::E2a),
            backend_default_debts: &["fixed_protonation"],
            container_image_sha256: None,
            cluster_mounts: vec![],
            output_file_per_task: "result.json",
            shuffle_nonce: Some([11u8; 32]),
        },
    )?;

    // GPU card 0 was bound and injected as CUDA_VISIBLE_DEVICES=0 across all 3 array tasks.
    assert_eq!(pending.submitted.allocated_gpu_indices, vec![0]);
    assert_eq!(*observed_cuda_envs.lock().unwrap(), vec!["0", "0", "0"]);
    // Lease was released cleanly after local execution finished.
    let snap = pool.snapshot()?;
    assert_eq!(snap.active_leases, 0);
    assert_eq!(snap.used_cpu_cores, 0);
    assert_eq!(snap.gpus[0].free_vram_mib, 24_576);

    let receipt = broker.finalize_protocol_job(
        &backend,
        &protocol,
        &pending,
        parse_diag_bytes,
        1024 * 1024,
    )?;
    assert_eq!(receipt.verdict(), ProtocolVerdict::Passed);
    assert_eq!(receipt.epistemic_level(), "E4a");
    assert!(receipt.control_summary().all_controls_passed);
    // Local sandbox does NOT carry unattested_remote_hpc debt.
    assert!(
        !receipt
            .epistemic_debts()
            .iter()
            .any(|d| d == UNATTESTED_REMOTE_HPC_DEBT)
    );
    assert_eq!(
        receipt.epistemic_debts(),
        &["classical_ff_only", "fixed_protonation"]
    );

    // Record into StateStore compute_jobs ledger cleanly.
    let actor = Change {
        actor: "broker:local",
        git_commit: Some(&"b".repeat(40)),
    };
    store.record_compute_job_submitted(
        actor,
        &NewComputeJob {
            job_id: &pending.job_id,
            prereg_id: None,
            protocol_id: &pending.protocol_id,
            protocol_sha256: &pending.protocol_sha256,
            base_commit: &pending.base_commit,
            candidate_id: &pending.candidate_id,
            candidate_sha256: &pending.candidate_sha256,
            target_kind: pending.submitted.target_kind,
            cluster_or_host_id: &pending.submitted.cluster_or_host_id,
            external_job_id: pending.submitted.external_job_id.as_deref(),
            staging_ref: &pending.submitted.staging_ref,
            bundle_sha256: &pending.submitted.bundle_sha256,
            resources: &pending.resources,
            blinded_plan: &pending.blinded_plan,
            branch_tag: Some("local_gpu"),
            initial_status: ComputeJobStatus::Running,
        },
    )?;
    let done = store.update_compute_job_status(
        actor,
        &pending.job_id,
        ComputeJobStatus::Completed,
        None,
        Some(&receipt),
    )?;
    assert_eq!(
        done.sealed_receipt_sha256.as_deref(),
        Some(receipt.receipt_sha256())
    );
    Ok(())
}

#[derive(Default)]
struct MockClusterState {
    next_id: u64,
    staged: HashMap<String, BTreeMap<String, String>>,
    states: HashMap<String, Vec<SlurmJobObservation>>,
    outputs: HashMap<String, BTreeMap<String, Vec<u8>>>,
    cancelled_ids: Vec<String>,
}

#[derive(Clone, Default)]
struct StatefulMockSlurmTransport {
    state: Arc<Mutex<MockClusterState>>,
}

impl StatefulMockSlurmTransport {
    fn new() -> Self {
        let st = MockClusterState {
            next_id: 80_000,
            ..Default::default()
        };
        Self {
            state: Arc::new(Mutex::new(st)),
        }
    }

    fn complete_job(&self, slurm_id: &str, staging_ref: &str, oom_task: Option<u32>) -> Result<()> {
        let mut st = self.state.lock().unwrap();
        let files = st
            .staged
            .get(staging_ref)
            .cloned()
            .context("missing staged cluster dir")?;
        let mut task_cfgs: Vec<(u32, String)> = files
            .iter()
            .filter_map(|(path, content)| {
                let rest = path.strip_prefix("tasks/")?;
                let (idx_str, fname) = rest.split_once('/')?;
                (fname == "fep.cfg").then(|| (idx_str.parse::<u32>().ok(), content.clone()))
            })
            .filter_map(|(i, c)| i.map(|idx| (idx, c)))
            .collect();
        task_cfgs.sort_by_key(|(i, _)| *i);

        let mut obs = Vec::new();
        let mut outs = BTreeMap::new();
        for (idx, cfg) in task_cfgs {
            if oom_task == Some(idx) {
                obs.push(SlurmJobObservation {
                    job_id: SlurmJobId::new(slurm_id, None)?,
                    array_task_id: Some(idx),
                    state: SlurmJobState::OutOfMemory,
                    exit_code: Some(137),
                    elapsed_seconds: 15.0,
                    allocated_tres: Some("cpu=8,gres/gpu=1".into()),
                    node_list: Some("a100-02".into()),
                });
            } else {
                obs.push(SlurmJobObservation {
                    job_id: SlurmJobId::new(slurm_id, None)?,
                    array_task_id: Some(idx),
                    state: SlurmJobState::Completed,
                    exit_code: Some(0),
                    elapsed_seconds: 42.0,
                    allocated_tres: Some("cpu=8,gres/gpu=1".into()),
                    node_list: Some("a100-01".into()),
                });
                outs.insert(
                    format!("tasks/{idx}/result.json"),
                    eval_smiles_to_json(&cfg).into_bytes(),
                );
            }
        }
        st.states.insert(slurm_id.to_owned(), obs);
        st.outputs.insert(staging_ref.to_owned(), outs);
        Ok(())
    }
}

impl SlurmTransport for StatefulMockSlurmTransport {
    fn stage_bundle(&self, dir: &str, files: &BTreeMap<String, String>) -> Result<String> {
        let mut st = self.state.lock().unwrap();
        let remote = format!("/scratch/ai4math/{dir}");
        st.staged.insert(remote.clone(), files.clone());
        Ok(remote)
    }

    fn submit_batch(
        &self,
        _staging_ref: &str,
        _script: &str,
        _flags: &[SlurmFlag],
    ) -> Result<SlurmJobId> {
        let mut st = self.state.lock().unwrap();
        st.next_id += 1;
        let id = st.next_id.to_string();
        let sid = SlurmJobId::new(&id, None)?;
        st.states.insert(
            id.clone(),
            vec![SlurmJobObservation {
                job_id: sid.clone(),
                array_task_id: None,
                state: SlurmJobState::Pending,
                exit_code: None,
                elapsed_seconds: 0.0,
                allocated_tres: None,
                node_list: None,
            }],
        );
        Ok(sid)
    }

    fn query_status(&self, job_ids: &[SlurmJobId]) -> Result<Vec<SlurmJobObservation>> {
        let st = self.state.lock().unwrap();
        let mut out = Vec::new();
        for id in job_ids {
            if let Some(obs) = st.states.get(id.as_str()) {
                out.extend(obs.clone());
            }
        }
        Ok(out)
    }

    fn fetch_outputs(
        &self,
        staging_ref: &str,
        allowlist: &[String],
        max_total_bytes: u64,
    ) -> Result<FetchedJobOutputs> {
        let st = self.state.lock().unwrap();
        let stored = st.outputs.get(staging_ref).cloned().unwrap_or_default();
        let mut files = BTreeMap::new();
        let mut total_bytes = 0u64;
        for rel in allowlist {
            if let Some(bytes) = stored.get(rel) {
                total_bytes += bytes.len() as u64;
                files.insert(rel.clone(), bytes.clone());
            }
        }
        let fetched = FetchedJobOutputs { files, total_bytes };
        fetched.validate(allowlist, max_total_bytes)?;
        Ok(fetched)
    }

    fn cancel_jobs(&self, job_ids: &[SlurmJobId]) -> Result<()> {
        let mut st = self.state.lock().unwrap();
        for id in job_ids {
            st.cancelled_ids.push(id.as_str().to_owned());
        }
        Ok(())
    }
}

fn slurm_policy_and_profile() -> (SlurmClusterPolicy, SlurmExecutionProfile) {
    let policy = SlurmClusterPolicy {
        cluster_id: "hpc_a100_prod".into(),
        allowed_partitions: vec!["gpu-a100".into()],
        allowed_accounts: vec!["compchem".into()],
        allowed_qos: vec!["normal".into()],
        per_job_ceiling: ResourceRequest {
            cpu_cores: 16,
            memory_mib: 65_536,
            gpu: Some(GpuRequest {
                count: 1,
                min_vram_mib: 40_960,
                kind: GpuKind::Cuda,
                exclusive: true,
            }),
            scratch_mib: 50_000,
            max_wall_seconds: 7_200,
        },
        max_active_jobs: 4,
        max_total_active_gpus: 6,
        require_container_digest: true,
        max_fetch_bytes: 2 * 1024 * 1024,
    };
    let profile = SlurmExecutionProfile {
        partition: "gpu-a100".into(),
        account: Some("compchem".into()),
        qos: Some("normal".into()),
        constraint: None,
        gpu_gres_kind: Some("a100".into()),
        runtime: SlurmContainerRuntime::Apptainer {
            sif_remote_path: "/containers/openmm_8.1.sif".into(),
            expected_sif_sha256: "d".repeat(64),
        },
        entrypoint_args: vec!["/opt/run_fep.py".into(), "fep.cfg".into()],
    };
    (policy, profile)
}

#[test]
fn slurm_blinded_job_array_survives_restart_unblinds_locally_and_adds_hpc_debt() -> Result<()> {
    let dir = tempdir()?;
    let db_path = dir.path().join("slurm.sqlite");
    let mut store = StateStore::open(&db_path)?;
    let broker = Broker::new();
    let (protocol, controls) = fep_protocol_and_controls()?;
    let proto_sha = protocol.protocol_sha256()?;
    let prereg = PreRegistrationSpec {
        version: 1,
        prereg_id: "PREREG-SLURM-1".into(),
        hypothesis_family_id: "fep_series_a".into(),
        protocol_sha256: proto_sha.clone(),
        primary_metric: "ddg_kcal_mol".into(),
        max_trials: 10,
        significance_alpha: Some(0.05),
        correction: CorrectionMethod::Bonferroni,
        stopping_rule: "stop at 10 trials".into(),
    };
    let actor = Change {
        actor: "broker:slurm",
        git_commit: Some(&"b".repeat(40)),
    };
    store.register_protocol_preregistration(actor, &prereg)?;

    let transport = StatefulMockSlurmTransport::new();
    let (policy, profile) = slurm_policy_and_profile();
    let backend = SlurmComputeBackend::new(policy.clone(), profile.clone(), transport.clone())?;

    let sub = CandidateSubmission {
        version: 1,
        protocol_sha256: proto_sha.clone(),
        candidate_id: "cand_slurm_01".into(),
        slot_values: BTreeMap::from([(
            "ligand_smiles".into(),
            SlotValue::Text("CC(=O)Oc1ccccc1".into()),
        )]),
    };
    let pending = broker.submit_protocol_job(
        &backend,
        AsyncProtocolSubmitRequest {
            job_id: "job_slurm_01",
            base_commit: &"b".repeat(40),
            protocol: &protocol,
            submission: &sub,
            blinded_controls: Some(&controls),
            resources: protocol.resource_ceiling.clone().unwrap(),
            verifier_sha256: &"d".repeat(64),
            backend_max_positive_ceiling: ScientificTrust::E4a,
            backend_max_refutation_ceiling: Some(ScientificTrust::E2a),
            backend_default_debts: &["fixed_protonation"],
            container_image_sha256: Some("d".repeat(64)),
            cluster_mounts: vec![ClusterMountEntry {
                input_name: "receptor_pdb".into(),
                remote_ro_path: "/datasets/pdb/target.pdb".into(),
                expected_sha256: "a".repeat(64),
                container_mount_path: "/inputs/receptor.pdb".into(),
            }],
            output_file_per_task: "result.json",
            shuffle_nonce: Some([99u8; 32]),
        },
    )?;

    // Cluster staged bundle MUST NOT contain control names or candidate ID!
    {
        let st = transport.state.lock().unwrap();
        let staged = st.staged.get(&pending.submitted.staging_ref).unwrap();
        let sbatch = staged.get("job.sbatch").unwrap();
        assert!(sbatch.contains("#SBATCH --array=0-2"));
        assert!(sbatch.contains("sha256sum -c manifest.sha256"));
        assert!(sbatch.contains("--bind '/datasets/pdb/target.pdb:/inputs/receptor.pdb:ro'"));
        let all_staged = serde_json::to_string(staged)?;
        assert!(!all_staged.contains("hidden_pos_binder"));
        assert!(!all_staged.contains("hidden_neg_decoy"));
        assert!(!all_staged.contains("cand_slurm_01"));
    }

    // Persist in-flight job to SQLite, then simulate full daemon restart!
    store.record_compute_job_submitted(
        actor,
        &NewComputeJob {
            job_id: &pending.job_id,
            prereg_id: Some(&prereg.prereg_id),
            protocol_id: &pending.protocol_id,
            protocol_sha256: &pending.protocol_sha256,
            base_commit: &pending.base_commit,
            candidate_id: &pending.candidate_id,
            candidate_sha256: &pending.candidate_sha256,
            target_kind: pending.submitted.target_kind,
            cluster_or_host_id: &pending.submitted.cluster_or_host_id,
            external_job_id: pending.submitted.external_job_id.as_deref(),
            staging_ref: &pending.submitted.staging_ref,
            bundle_sha256: &pending.submitted.bundle_sha256,
            resources: &pending.resources,
            blinded_plan: &pending.blinded_plan,
            branch_tag: Some("branch_fep_a"),
            initial_status: ComputeJobStatus::Queued,
        },
    )?;
    let slurm_id = pending.submitted.external_job_id.clone().unwrap();
    let staging_ref = pending.submitted.staging_ref.clone();
    drop(backend);
    drop(store);

    // === Simulated daemon restart ===
    let mut recovered_store = StateStore::open(&db_path)?;
    let quota = recovered_store.active_compute_quota("hpc_a100_prod")?;
    assert_eq!(quota.active_jobs, 1);
    assert_eq!(quota.active_gpus, 3); // 3 array tasks x 1 GPU

    let recovered_backend = SlurmComputeBackend::new(policy, profile, transport.clone())?;
    recovered_backend.set_reconciled_quota(quota.active_jobs, quota.active_gpus)?;

    let active_rows =
        recovered_store.list_active_compute_jobs(Some(ExecutionTargetKind::Slurm), None)?;
    assert_eq!(active_rows.len(), 1);
    let row = &active_rows[0];
    recovered_backend.register_recovered_job(
        &row.job_id,
        row.array_len,
        row.requested_gpus_total,
    )?;

    // Cluster finishes all 3 array tasks cleanly.
    transport.complete_job(&slurm_id, &staging_ref, None)?;

    // Rebuild PendingProtocolJob from durable store row & finalize locally!
    let recovered_pending = PendingProtocolJob {
        job_id: row.job_id.clone(),
        protocol_id: row.protocol_id.clone(),
        protocol_sha256: row.protocol_sha256.clone(),
        base_commit: row.base_commit.clone(),
        candidate_id: row.candidate_id.clone(),
        candidate_sha256: row.candidate_sha256.clone(),
        rendered_artifacts_sha256: pending.rendered_artifacts_sha256.clone(),
        pinned_inputs_sha256: pending.pinned_inputs_sha256.clone(),
        verifier_sha256: pending.verifier_sha256.clone(),
        backend_max_positive_ceiling: ScientificTrust::E4a,
        backend_max_refutation_ceiling: Some(ScientificTrust::E2a),
        backend_default_debts: vec!["fixed_protonation".into()],
        output_file_per_task: "result.json".into(),
        output_allowlist: pending.output_allowlist.clone(),
        resources: row.resources.clone(),
        container_image_sha256: Some("d".repeat(64)),
        submitted: SubmittedComputeJob {
            job_id: row.job_id.clone(),
            target_kind: row.target_kind,
            cluster_or_host_id: row.cluster_or_host_id.clone(),
            external_job_id: row.external_job_id.clone(),
            staging_ref: row.staging_ref.clone(),
            bundle_sha256: row.bundle_sha256.clone(),
            allocated_gpu_indices: vec![],
        },
        blinded_plan: row.blinded_plan.clone(),
    };

    let receipt = broker.finalize_protocol_job(
        &recovered_backend,
        &protocol,
        &recovered_pending,
        parse_diag_bytes,
        1024 * 1024,
    )?;
    assert_eq!(receipt.verdict(), ProtocolVerdict::Passed);
    assert_eq!(receipt.epistemic_level(), "E4a");
    assert!(receipt.control_summary().all_controls_passed);
    // Remote SLURM execution automatically appends unattested_remote_hpc debt!
    assert!(
        receipt
            .epistemic_debts()
            .iter()
            .any(|d| d == UNATTESTED_REMOTE_HPC_DEBT)
    );

    recovered_store.record_protocol_trial(actor, &prereg.prereg_id, &receipt, Some(true))?;
    recovered_store.update_compute_job_status(
        actor,
        &row.job_id,
        ComputeJobStatus::Completed,
        Some("finalized after restart"),
        Some(&receipt),
    )?;
    let audit = recovered_store
        .audit_protocol_preregistration(&prereg.prereg_id, &ProtocolSmellPolicy::default())?
        .unwrap();
    assert_eq!(audit.trials_consumed, 1);
    assert_eq!(audit.passed_trials, 1);
    assert_eq!(
        recovered_store
            .active_compute_quota("hpc_a100_prod")?
            .active_jobs,
        0
    );
    Ok(())
}

#[test]
fn slurm_breaker_trip_cancels_only_target_branch_and_oom_seals_verifier_error() -> Result<()> {
    let dir = tempdir()?;
    let mut store = StateStore::open(dir.path().join("breaker.sqlite"))?;
    let broker = Broker::new();
    let (protocol, controls) = fep_protocol_and_controls()?;
    let proto_sha = protocol.protocol_sha256()?;
    let transport = StatefulMockSlurmTransport::new();
    let (mut policy, profile) = slurm_policy_and_profile();
    policy.max_total_active_gpus = 12;
    let backend = SlurmComputeBackend::new(policy, profile, transport.clone())?;
    let actor = Change {
        actor: "broker:slurm",
        git_commit: Some(&"b".repeat(40)),
    };

    let mut submit_branch = |job_id: &str,
                             cand_id: &str,
                             branch: &str|
     -> Result<PendingProtocolJob> {
        let sub = CandidateSubmission {
            version: 1,
            protocol_sha256: proto_sha.clone(),
            candidate_id: cand_id.into(),
            slot_values: BTreeMap::from([("ligand_smiles".into(), SlotValue::Text("CCO".into()))]),
        };
        let pending = broker.submit_protocol_job(
            &backend,
            AsyncProtocolSubmitRequest {
                job_id,
                base_commit: &"b".repeat(40),
                protocol: &protocol,
                submission: &sub,
                blinded_controls: Some(&controls),
                resources: protocol.resource_ceiling.clone().unwrap(),
                verifier_sha256: &"d".repeat(64),
                backend_max_positive_ceiling: ScientificTrust::E4a,
                backend_max_refutation_ceiling: Some(ScientificTrust::E2a),
                backend_default_debts: &[],
                container_image_sha256: Some("d".repeat(64)),
                cluster_mounts: vec![],
                output_file_per_task: "result.json",
                shuffle_nonce: Some([3u8; 32]),
            },
        )?;
        store.record_compute_job_submitted(
            actor,
            &NewComputeJob {
                job_id: &pending.job_id,
                prereg_id: None,
                protocol_id: &pending.protocol_id,
                protocol_sha256: &pending.protocol_sha256,
                base_commit: &pending.base_commit,
                candidate_id: &pending.candidate_id,
                candidate_sha256: &pending.candidate_sha256,
                target_kind: pending.submitted.target_kind,
                cluster_or_host_id: &pending.submitted.cluster_or_host_id,
                external_job_id: pending.submitted.external_job_id.as_deref(),
                staging_ref: &pending.submitted.staging_ref,
                bundle_sha256: &pending.submitted.bundle_sha256,
                resources: &pending.resources,
                blinded_plan: &pending.blinded_plan,
                branch_tag: Some(branch),
                initial_status: ComputeJobStatus::Queued,
            },
        )?;
        Ok(pending)
    };

    let j1 = submit_branch("job_b1_a", "cand_1", "branch_smelly")?;
    let j2 = submit_branch("job_b1_b", "cand_2", "branch_smelly")?;
    let j3 = submit_branch("job_other", "cand_3", "branch_healthy")?;

    // Breaker trips on `branch_smelly` -> cancel ONLY its two SLURM jobs!
    let to_cancel_rows =
        store.list_active_compute_jobs(Some(ExecutionTargetKind::Slurm), Some("branch_smelly"))?;
    assert_eq!(to_cancel_rows.len(), 2);
    let handles: Vec<SubmittedComputeJob> = to_cancel_rows
        .iter()
        .map(|r| SubmittedComputeJob {
            job_id: r.job_id.clone(),
            target_kind: r.target_kind,
            cluster_or_host_id: r.cluster_or_host_id.clone(),
            external_job_id: r.external_job_id.clone(),
            staging_ref: r.staging_ref.clone(),
            bundle_sha256: r.bundle_sha256.clone(),
            allocated_gpu_indices: vec![],
        })
        .collect();
    let refs: Vec<&SubmittedComputeJob> = handles.iter().collect();
    backend.cancel_batch(&refs)?;
    for h in &handles {
        store.update_compute_job_status(
            actor,
            &h.job_id,
            ComputeJobStatus::Cancelled,
            Some("circuit breaker tripped"),
            None,
        )?;
    }

    let cancelled = transport.state.lock().unwrap().cancelled_ids.clone();
    assert_eq!(cancelled.len(), 2);
    assert!(cancelled.contains(j1.submitted.external_job_id.as_ref().unwrap()));
    assert!(cancelled.contains(j2.submitted.external_job_id.as_ref().unwrap()));
    assert!(!cancelled.contains(j3.submitted.external_job_id.as_ref().unwrap()));
    assert_eq!(store.active_compute_quota("hpc_a100_prod")?.active_jobs, 1);

    // Meanwhile j3 hits OOM on array task 1 -> seals VerifierError + E0 (never Passed!).
    transport.complete_job(
        j3.submitted.external_job_id.as_deref().unwrap(),
        &j3.submitted.staging_ref,
        Some(1),
    )?;
    let oom_receipt =
        broker.finalize_protocol_job(&backend, &protocol, &j3, parse_diag_bytes, 1024 * 1024)?;
    assert_eq!(oom_receipt.verdict(), ProtocolVerdict::VerifierError);
    assert_eq!(oom_receipt.epistemic_level(), "E0");
    assert!(!oom_receipt.control_summary().all_controls_passed);
    Ok(())
}
