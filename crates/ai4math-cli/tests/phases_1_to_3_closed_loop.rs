//! End-to-end closed-loop test spanning Phases 1, 2, and 3 (ADR-0007 D31-D33):
//!
//! 4-stratum KnowledgeVault (LITERATURE + retracted filtering)
//!   -> Generalized Claim Graph (migration 9) + Dual-Channel Subjective Logic BP & VoI
//!   -> IdleVerificationScheduler (Human Queue + BP VoI + Zero-Starvation Watermarks + Idle Budget)
//!   -> Broker::verify_protocol sealed receipts
//!   -> Atomic replicates (node upgrade + belief jump) & contradicts (conflict_mass spike) closure!

use ai4math_broker::{
    Broker, BrokerLoad, GpuDeviceSpec, IdleAdmissionPolicy, IdleBudgetPolicy, IdleCandidate,
    IdleDispatchDecision, IdleSystemSnapshot, IdleVerificationScheduler, LocalComputeCapacity,
    LocalComputePool, ProtocolBackendRunner, ProtocolLane, ProtocolVerificationRequest,
    RawBackendOutput,
};
use ai4math_core::{
    BeliefPropagationConfig, ExecutionTargetKind, GpuKind, ReproducibilityClass, ScientificTrust,
    TagAttenuationPolicy,
};
use ai4math_evidence::{
    ArtifactTemplate, BlindedControlPolicy, CandidateSubmission, CompiledProtocolRun,
    DiagnosticValue, MetricRule, NamedMetricRule, OrthogonalPolicy, ParamValue, ParsedDiagnostics,
    ProtocolSpec, ProtocolVerdict, SlotKind, SlotSpec, SlotValue, TextProfile,
};
use ai4math_store::{
    Change, IdleTicketSource, IdleTicketStatus, NewEpistemicNode, NewEpistemicPlan,
    NewIdleVerificationTicket, StateStore, Wiki,
};
use anyhow::{Context, Result};
use std::{collections::BTreeMap, fs};
use tempfile::tempdir;

struct MockFepRunner {
    ddg_by_smiles: BTreeMap<String, f64>,
}

impl ProtocolBackendRunner for MockFepRunner {
    fn backend_id(&self) -> &str {
        "openmm_fep"
    }
    fn verifier_sha256(&self) -> Result<String> {
        Ok("9".repeat(64))
    }
    fn max_positive_ceiling(&self) -> ScientificTrust {
        ScientificTrust::E4a
    }
    fn max_refutation_ceiling(&self) -> Option<ScientificTrust> {
        Some(ScientificTrust::E2a)
    }
    fn default_debts(&self) -> &'static [&'static str] {
        &["classical_ff_only"]
    }
    fn execute(&self, compiled: &CompiledProtocolRun) -> Result<RawBackendOutput> {
        let cfg = compiled.artifacts().get("job.cfg").context("missing cfg")?;
        let ddg = self
            .ddg_by_smiles
            .iter()
            .find(|(smi, _)| cfg.contains(&format!("smiles={smi}")))
            .map_or(0.0, |(_, v)| *v);
        let diag = ParsedDiagnostics {
            metrics: BTreeMap::from([
                ("converged".into(), DiagnosticValue::Bool(true)),
                ("ddg_kcal_mol".into(), DiagnosticValue::Float(ddg)),
            ]),
        };
        Ok(RawBackendOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: Some(0),
            elapsed_seconds: 12.0,
            structured_payload: Some(serde_json::to_value(diag)?),
        })
    }
    fn parse_diagnostics(&self, raw: &RawBackendOutput) -> Result<ParsedDiagnostics> {
        Ok(serde_json::from_value(
            raw.structured_payload
                .clone()
                .context("missing mock payload")?,
        )?)
    }
}

fn fep_protocol() -> ProtocolSpec {
    ProtocolSpec {
        version: 1,
        protocol_id: "openmm_fep_v1".into(),
        domain_id: "compchem".into(),
        backend_id: "openmm_fep".into(),
        epistemic_ceiling: ScientificTrust::E4a,
        default_debts: vec![],
        pinned_inputs: BTreeMap::new(),
        frozen_parameters: BTreeMap::from([("temp_k".into(), ParamValue::Float(300.0))]),
        slots: vec![SlotSpec {
            name: "smiles".into(),
            kind: SlotKind::Text {
                max_len: 128,
                profile: TextProfile::Smiles,
            },
        }],
        artifact_templates: vec![ArtifactTemplate {
            path: "job.cfg".into(),
            template: "temp={{param:temp_k}}\nsmiles={{slot:smiles}}\n".into(),
        }],
        sanity_gates: vec![NamedMetricRule {
            rule_id: "converged".into(),
            rule: MetricRule::EqualsBool {
                metric: "converged".into(),
                expected: true,
            },
        }],
        acceptance_criteria: vec![NamedMetricRule {
            rule_id: "strong_binder".into(),
            rule: MetricRule::MaxFloat {
                metric: "ddg_kcal_mol".into(),
                max: -1.5,
            },
        }],
        orthogonal_policy: OrthogonalPolicy::default(),
        control_policy: BlindedControlPolicy::default(),
        preregistration_required: false,
        resource_ceiling: None,
    }
}

#[test]
fn phases_1_to_3_self_hardening_literature_bp_idle_verification_closed_loop() -> Result<()> {
    let dir = tempdir()?;
    let db_path = dir.path().join("state.sqlite");
    let wiki_root = dir.path().join(".mathwiki");
    for sub in ["core", "literature"] {
        fs::create_dir_all(wiki_root.join(sub))?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&wiki_root, fs::Permissions::from_mode(0o700))?;
    }

    // Populate LITERATURE vault with:
    //  1) LIT-A: referenced by active critical Claim Graph -> enters via BP VoI auto-queue
    //  2) LIT-HUMAN: NOT referenced by critical path yet -> submitted explicitly via Human Queue
    //  3) LIT-RETRACTED: retracted paper -> excluded from automatable list!
    fs::write(
        wiki_root.join("literature/LIT-A.md"),
        r#"---
id: LIT-A
stratum: literature
domain_id: compchem
trust_level: E4a
engine: literature
status: active
reproducibility_class: replicable_by_protocol
citation_status: active
condition_tags: [incomplete_conditions]
title: Reported ligand A affinity
informal_gloss: Paper reports ligand A binds with ddG = -2.4 kcal/mol (pH omitted)
facets:
  smiles: ["CC(=O)Oc1ccccc1"]
---
"#,
    )?;
    fs::write(
        wiki_root.join("literature/LIT-HUMAN.md"),
        r#"---
id: LIT-HUMAN
stratum: literature
domain_id: compchem
trust_level: E4a
engine: literature
status: active
reproducibility_class: replicable_by_protocol
citation_status: active
title: Human-requested literature check B
informal_gloss: Paper claims alkane scaffold B binds strongly
facets:
  smiles: ["CCCCCCCC"]
---
"#,
    )?;
    fs::write(
        wiki_root.join("literature/LIT-RETRACTED.md"),
        r#"---
id: LIT-RETRACTED
stratum: literature
domain_id: compchem
trust_level: E0
engine: literature
status: retracted
reproducibility_class: replicable_by_protocol
citation_status: retracted
title: Retracted study
---
"#,
    )?;

    let mut wiki = Wiki::open(wiki_root.join(".index.sqlite"))?;
    assert_eq!(wiki.rebuild(&wiki_root)?, 3);
    let automatable = wiki.list_automatable_literature()?;
    assert_eq!(automatable.len(), 2);
    assert!(automatable.iter().all(|e| e.entry_id != "LIT-RETRACTED"));

    // Build generalized Claim Graph (migration 9) with critical root depending on LIT-A.
    let mut store = StateStore::open(&db_path)?;
    let actor = Change {
        actor: "idle-orchestrator",
        git_commit: Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
    };
    store.upsert_epistemic_node(
        actor,
        &NewEpistemicNode {
            node_id: "NODE-LIT-A",
            domain_id: "compchem",
            backend_id: "literature",
            title: "Literature ligand A",
            statement: "Ligand A binds with ddG < -1.5 kcal/mol",
            is_critical_path: false,
            local_level: "E4a",
            condition_tags: &["unreproduced_literature", "incomplete_conditions"],
            local_debts: &[],
            reproducibility_class: Some(ReproducibilityClass::ReplicableByProtocol),
            source_entry_id: Some("LIT-A"),
        },
    )?;
    store.upsert_epistemic_node(
        actor,
        &NewEpistemicNode {
            node_id: "NODE-ROOT",
            domain_id: "compchem",
            backend_id: "openmm_fep",
            title: "Lead series efficacy",
            statement: "Series A yields viable inhibitor",
            is_critical_path: true,
            local_level: "E0",
            condition_tags: &[],
            local_debts: &[],
            reproducibility_class: None,
            source_entry_id: None,
        },
    )?;
    store.upsert_epistemic_plan(
        actor,
        &NewEpistemicPlan {
            plan_id: "PLAN-A",
            target_node_id: "NODE-ROOT",
            name: "Lit A route",
            dependencies: &["NODE-LIT-A"],
            active: true,
        },
    )?;
    // Also register NODE-LIT-HUMAN (non-critical, not yet linked to ROOT) to observe contradiction!
    store.upsert_epistemic_node(
        actor,
        &NewEpistemicNode {
            node_id: "NODE-LIT-HUMAN",
            domain_id: "compchem",
            backend_id: "literature",
            title: "Human-queued ligand B",
            statement: "Ligand B binds strongly",
            is_critical_path: false,
            local_level: "E4a",
            condition_tags: &["unreproduced_literature"],
            local_debts: &[],
            reproducibility_class: Some(ReproducibilityClass::ReplicableByProtocol),
            source_entry_id: Some("LIT-HUMAN"),
        },
    )?;

    // Run BP -> VoI ranks NODE-LIT-A #1 -> auto-sync into idle verification queue!
    let atten = TagAttenuationPolicy::default();
    let bp_cfg = BeliefPropagationConfig::default();
    let snap_before = store.record_belief_snapshot(actor, &atten, &bp_cfg)?;
    let belief_before = snap_before.report.nodes["NODE-ROOT"]
        .propagated_opinion
        .belief;
    assert!(belief_before <= 0.45 + 1e-9);

    let targets = BTreeMap::from([("compchem".to_owned(), "openmm_fep_v1".to_owned())]);
    let auto_tickets =
        store.sync_voi_idle_queue(actor, &snap_before.report, &automatable, &targets, 0.05)?;
    assert_eq!(auto_tickets.len(), 1);
    assert_eq!(auto_tickets[0].entry_id, "LIT-A");
    assert_eq!(auto_tickets[0].source, IdleTicketSource::BpVoi);

    // Human explicitly enqueues LIT-HUMAN into the idle queue!
    let human_ticket = store.enqueue_idle_verification(
        actor,
        &NewIdleVerificationTicket {
            ticket_id: "idle-human-lit-b",
            source: IdleTicketSource::Human,
            entry_id: "LIT-HUMAN",
            epistemic_node_id: Some("NODE-LIT-HUMAN"),
            domain_id: "compchem",
            reproducibility_class: ReproducibilityClass::ReplicableByProtocol,
            target_protocol_or_engine: "openmm_fep_v1",
            priority_score: 0.5,
            human_note: Some("Double-check alkane scaffold claim during overnight idle window"),
        },
    )?;
    assert_eq!(human_ticket.source, IdleTicketSource::Human);

    // Build IdleVerificationScheduler with tight budget (25s wall limit) to test budget cap too!
    let scheduler = IdleVerificationScheduler::new(IdleAdmissionPolicy {
        budget: IdleBudgetPolicy {
            max_total_jobs: 10,
            max_total_wall_seconds: 25.0,
            max_total_gpu_seconds: 25.0,
        },
        ..IdleAdmissionPolicy::default()
    });
    let pool = LocalComputePool::new(LocalComputeCapacity {
        total_cpu_cores: 16,
        total_memory_mib: 65_536,
        total_scratch_mib: 100_000,
        gpus: vec![GpuDeviceSpec {
            index: 0,
            kind: GpuKind::Cuda,
            total_vram_mib: 24_576,
            unified_memory: false,
        }],
    })?;

    let to_candidates = |tickets: &[ai4math_store::RetainedIdleTicket]| -> Vec<IdleCandidate> {
        tickets
            .iter()
            .map(|t| IdleCandidate {
                ticket_id: t.ticket_id.clone(),
                is_human_queued: t.source == IdleTicketSource::Human,
                priority_score: t.priority_score,
                reproducibility_class: t.reproducibility_class,
                target_kind: ExecutionTargetKind::LocalSandbox,
                requires_gpu: true,
            })
            .collect()
    };

    let make_snapshot = |heavy_running: usize,
                         usage: ai4math_store::IdleUsageTotals|
     -> Result<IdleSystemSnapshot> {
        Ok(IdleSystemSnapshot {
            broker_load: BrokerLoad {
                fast_busy: 0,
                fast_capacity: 4,
                lean_advisory_busy: 0,
                lean_advisory_capacity: 4,
                heavy_running,
                heavy_queued: 0,
            },
            host_memory_green: true,
            local_pool: Some(pool.snapshot()?),
            slurm_active_jobs: 0,
            slurm_max_jobs: 4,
            slurm_active_gpus: 0,
            slurm_max_gpus: 4,
            idle_jobs_executed: usage.executed_tickets,
            idle_wall_seconds_used: usage.total_wall_seconds,
            idle_gpu_seconds_used: usage.total_gpu_seconds,
        })
    };

    // Foreground Heavy busy -> scheduler MUST DEFER!
    let queued = store.list_queued_idle_tickets()?;
    assert_eq!(queued.len(), 2);
    assert!(matches!(
        scheduler.select_next(
            &make_snapshot(1, store.idle_usage_totals()?)?,
            &to_candidates(&queued)
        ),
        IdleDispatchDecision::Defer { .. }
    ));

    // Host idle -> Human queue ticket admitted FIRST!
    let first_pick = scheduler.select_next(
        &make_snapshot(0, store.idle_usage_totals()?)?,
        &to_candidates(&queued),
    );
    assert!(matches!(
        first_pick,
        IdleDispatchDecision::Admit { ref ticket_id, .. } if ticket_id == "idle-human-lit-b"
    ));

    // Execute Human ticket (LIT-HUMAN) via real Broker::verify_protocol -> REJECTED (contradicts paper!).
    let broker = Broker::new();
    let protocol = fep_protocol();
    let proto_sha = protocol.protocol_sha256()?;
    let runner = MockFepRunner {
        ddg_by_smiles: BTreeMap::from([
            ("CC(=O)Oc1ccccc1".into(), -2.5), // LIT-A replicates!
            ("CCCCCCCC".into(), 0.8),         // LIT-HUMAN fails to bind -> contradicts!
        ]),
    };

    store.mark_idle_ticket_dispatched(actor, "idle-human-lit-b", None)?;
    let receipt_b = broker.verify_protocol(ProtocolVerificationRequest {
        base_commit: &"a".repeat(40),
        protocol: &protocol,
        submission: &CandidateSubmission {
            version: 1,
            protocol_sha256: proto_sha.clone(),
            candidate_id: "idle_check_lit_b".into(),
            slot_values: BTreeMap::from([("smiles".into(), SlotValue::Text("CCCCCCCC".into()))]),
        },
        blinded_controls: None,
        runners: &[&runner],
        lane: ProtocolLane::Heavy,
    })?;
    assert_eq!(receipt_b.verdict(), ProtocolVerdict::Rejected);

    // Record local counter-evidence node + close ticket as contradicted -> auto-inserts Contradicts edge!
    store.upsert_epistemic_node(
        actor,
        &NewEpistemicNode {
            node_id: "COUNTER-B",
            domain_id: "compchem",
            backend_id: "openmm_fep",
            title: "Local FEP refutes B",
            statement: "Observed ddG = +0.8 kcal/mol",
            is_critical_path: false,
            local_level: "E4a",
            condition_tags: &[],
            local_debts: &[],
            reproducibility_class: None,
            source_entry_id: None,
        },
    )?;
    let closed_b = store.complete_idle_ticket_contradicted(
        actor,
        "idle-human-lit-b",
        "COUNTER-B",
        &receipt_b,
        "local FEP observed +0.8 kcal/mol",
        12.0,
        12.0,
    )?;
    assert_eq!(closed_b.status, IdleTicketStatus::Contradicted);

    // Next idle slot admits the BP VoI ticket (LIT-A)!
    let queued2 = store.list_queued_idle_tickets()?;
    assert_eq!(queued2.len(), 1);
    let second_pick = scheduler.select_next(
        &make_snapshot(0, store.idle_usage_totals()?)?,
        &to_candidates(&queued2),
    );
    assert!(matches!(
        second_pick,
        IdleDispatchDecision::Admit { ref ticket_id, .. } if ticket_id == "idle-voi-LIT-A"
    ));

    store.mark_idle_ticket_dispatched(actor, "idle-voi-LIT-A", None)?;
    let receipt_a = broker.verify_protocol(ProtocolVerificationRequest {
        base_commit: &"a".repeat(40),
        protocol: &protocol,
        submission: &CandidateSubmission {
            version: 1,
            protocol_sha256: proto_sha,
            candidate_id: "idle_check_lit_a".into(),
            slot_values: BTreeMap::from([(
                "smiles".into(),
                SlotValue::Text("CC(=O)Oc1ccccc1".into()),
            )]),
        },
        blinded_controls: None,
        runners: &[&runner],
        lane: ProtocolLane::Heavy,
    })?;
    assert_eq!(receipt_a.verdict(), ProtocolVerdict::Passed);

    let closed_a =
        store.complete_idle_ticket_replicated(actor, "idle-voi-LIT-A", &receipt_a, 14.0, 14.0)?;
    assert_eq!(closed_a.status, IdleTicketStatus::Replicated);

    // Re-run BP after closed loop:
    //  1) NODE-ROOT belief jumped sharply after LIT-A replicated & cleared condition tags!
    //  2) NODE-LIT-HUMAN now carries explicit conflict_mass & disbelief from COUNTER-B!
    let snap_after = store.record_belief_snapshot(actor, &atten, &bp_cfg)?;
    let belief_after = snap_after.report.nodes["NODE-ROOT"]
        .propagated_opinion
        .belief;
    assert!(
        belief_after > belief_before + 0.20,
        "expected belief jump: before={belief_before}, after={belief_after}"
    );
    assert!(
        !snap_after.report.nodes["NODE-ROOT"]
            .accumulated_debts
            .iter()
            .any(|d| d.code == "unreproduced_literature" || d.code == "incomplete_conditions")
    );
    assert!(snap_after.report.nodes["NODE-LIT-HUMAN"].conflict_mass > 0.15);
    assert!(
        snap_after.report.nodes["NODE-LIT-HUMAN"]
            .propagated_opinion
            .disbelief
            > 0.25
    );

    // Idle budget (12 + 14 = 26s > 25s limit) is now exhausted -> scheduler blocks new idle tickets!
    let extra = store.enqueue_idle_verification(
        actor,
        &NewIdleVerificationTicket {
            ticket_id: "idle-human-extra",
            source: IdleTicketSource::Human,
            entry_id: "LIT-EXTRA",
            epistemic_node_id: None,
            domain_id: "compchem",
            reproducibility_class: ReproducibilityClass::ReplicableByProtocol,
            target_protocol_or_engine: "openmm_fep_v1",
            priority_score: 1.0,
            human_note: None,
        },
    )?;
    assert!(matches!(
        scheduler.select_next(
            &make_snapshot(0, store.idle_usage_totals()?)?,
            &to_candidates(&[extra])
        ),
        IdleDispatchDecision::Defer { ref reasons }
            if reasons.iter().any(|r| r.contains("wall-second budget exhausted"))
    ));

    // Verify readonly audit readback of closed tickets and usage totals.
    drop(store);
    let ro = StateStore::open_readonly(&db_path)?;
    let totals = ro.idle_usage_totals()?;
    assert_eq!(totals.executed_tickets, 2);
    assert!((totals.total_wall_seconds - 26.0).abs() < 1e-9);
    assert_eq!(
        ro.read_idle_ticket("idle-voi-LIT-A")?.unwrap().status,
        IdleTicketStatus::Replicated
    );
    assert_eq!(
        ro.read_idle_ticket("idle-human-lit-b")?.unwrap().status,
        IdleTicketStatus::Contradicted
    );
    Ok(())
}
