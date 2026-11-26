//! End-to-end integration tests connecting the domain-agnostic abstractions
//! (Step 1) and the anti-drift ProtocolSpec engine (Step 2) across
//! ai4math-core -> ai4math-evidence -> ai4math-broker -> ai4math-store.

use ai4math_broker::{
    Broker, ProtocolBackendRunner, ProtocolLane, ProtocolVerificationRequest, RawBackendOutput,
};
use ai4math_core::{
    BackendDescriptor, DomainPack, EpistemicAssessment, EpistemicDebt, EpistemicLevel,
    ItpDomainPack, ScientificTrust, aggregate_assessment_plan, best_assessment_plan,
};
use ai4math_evidence::{
    ArtifactTemplate, BlindedControlCase, BlindedControlPack, BlindedControlPolicy,
    CandidateSubmission, CompiledProtocolRun, CorrectionMethod, DiagnosticValue, MetricRule,
    NamedMetricRule, OrthogonalPolicy, ParamValue, ParsedDiagnostics, PreRegistrationSpec,
    ProtocolRunEvidence, ProtocolSmellPolicy, ProtocolSmellSignal, ProtocolSpec, ProtocolVerdict,
    SealedReceipt, SlotKind, SlotSpec, SlotValue, TextProfile,
};
use ai4math_store::{Change, StateStore};
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use tempfile::tempdir;

struct ScriptedRunner {
    backend: &'static str,
    sha: String,
    ceiling: ScientificTrust,
    ref_ceiling: Option<ScientificTrust>,
    debts: &'static [&'static str],
    eval: Box<dyn Fn(&CompiledProtocolRun) -> ParsedDiagnostics + Send + Sync>,
}

impl ProtocolBackendRunner for ScriptedRunner {
    fn backend_id(&self) -> &str {
        self.backend
    }
    fn verifier_sha256(&self) -> Result<String> {
        Ok(self.sha.clone())
    }
    fn max_positive_ceiling(&self) -> ScientificTrust {
        self.ceiling
    }
    fn max_refutation_ceiling(&self) -> Option<ScientificTrust> {
        self.ref_ceiling
    }
    fn default_debts(&self) -> &'static [&'static str] {
        self.debts
    }
    fn execute(&self, compiled: &CompiledProtocolRun) -> Result<RawBackendOutput> {
        let diag = (self.eval)(compiled);
        Ok(RawBackendOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: Some(0),
            elapsed_seconds: 0.02,
            structured_payload: Some(serde_json::to_value(diag)?),
        })
    }
    fn parse_diagnostics(&self, raw: &RawBackendOutput) -> Result<ParsedDiagnostics> {
        Ok(serde_json::from_value(
            raw.structured_payload
                .clone()
                .context("missing scripted payload")?,
        )?)
    }
}

fn commit() -> &'static str {
    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
}

fn dft_diag(scf: bool, imag: i64, irc: bool, barrier: f64) -> ParsedDiagnostics {
    ParsedDiagnostics {
        metrics: BTreeMap::from([
            ("scf_converged".into(), DiagnosticValue::Bool(scf)),
            ("imaginary_frequencies".into(), DiagnosticValue::Int(imag)),
            ("irc_endpoints_connected".into(), DiagnosticValue::Bool(irc)),
            ("barrier_kcal_mol".into(), DiagnosticValue::Float(barrier)),
        ]),
    }
}

#[test]
fn end_to_end_compchem_anti_drift_pipeline_from_broker_to_store() -> Result<()> {
    let dir = tempdir()?;
    let mut store = StateStore::open(dir.path().join("state.sqlite"))?;
    let broker = Broker::new();

    // Verify Step 1 built-in ITP pack still coexists cleanly.
    let itp = ItpDomainPack::new();
    assert_eq!(itp.domain_id(), "itp");
    assert_eq!(itp.backends().len(), 7);

    // Build blinded control pack FIRST (acyclic Merkle commitment).
    let controls = BlindedControlPack {
        version: 1,
        protocol_id: "catalysis_ts_v1".into(),
        positive_controls: vec![BlindedControlCase {
            control_id: "ctrl_known_fast_substrate".into(),
            slot_values: BTreeMap::from([(
                "substrate_smiles".into(),
                SlotValue::Text("CC=CC".into()),
            )]),
        }],
        negative_controls: vec![BlindedControlCase {
            control_id: "ctrl_inert_alkane_decoy".into(),
            slot_values: BTreeMap::from([(
                "substrate_smiles".into(),
                SlotValue::Text("CCCCCCCC".into()),
            )]),
        }],
    };
    let control_pack_sha256 = controls.fingerprint()?;

    let protocol = ProtocolSpec {
        version: 1,
        protocol_id: "catalysis_ts_v1".into(),
        domain_id: "compchem".into(),
        backend_id: "orca_wb97xd".into(),
        epistemic_ceiling: ScientificTrust::E4a,
        default_debts: vec!["implicit_solvent".into()],
        pinned_inputs: BTreeMap::from([("catalyst_geom".into(), "b".repeat(64))]),
        frozen_parameters: BTreeMap::from([
            ("functional".into(), ParamValue::Text("wB97X-D".into())),
            ("basis".into(), ParamValue::Text("def2-TZVP".into())),
        ]),
        slots: vec![SlotSpec {
            name: "substrate_smiles".into(),
            kind: SlotKind::Text {
                max_len: 128,
                profile: TextProfile::Smiles,
            },
        }],
        artifact_templates: vec![ArtifactTemplate {
            path: "orca/ts.inp".into(),
            template: "func={{param:functional}}\nbasis={{param:basis}}\nsmiles={{slot:substrate_smiles}}\n".into(),
        }],
        sanity_gates: vec![
            NamedMetricRule {
                rule_id: "scf_ok".into(),
                rule: MetricRule::EqualsBool {
                    metric: "scf_converged".into(),
                    expected: true,
                },
            },
            NamedMetricRule {
                rule_id: "single_imag_ts".into(),
                rule: MetricRule::EqualsInt {
                    metric: "imaginary_frequencies".into(),
                    expected: 1,
                },
            },
            NamedMetricRule {
                rule_id: "irc_connects".into(),
                rule: MetricRule::EqualsBool {
                    metric: "irc_endpoints_connected".into(),
                    expected: true,
                },
            },
        ],
        acceptance_criteria: vec![NamedMetricRule {
            rule_id: "barrier_below_18".into(),
            rule: MetricRule::MaxFloat {
                metric: "barrier_kcal_mol".into(),
                max: 18.0,
            },
        }],
        orthogonal_policy: OrthogonalPolicy::default(),
        control_policy: BlindedControlPolicy {
            control_pack_sha256: Some(control_pack_sha256),
            min_positive_controls: 1,
            min_negative_controls: 1,
        },
        preregistration_required: true,
    };
    let protocol_sha256 = protocol.protocol_sha256()?;

    let prereg = PreRegistrationSpec {
        version: 1,
        prereg_id: "PREREG-CAT-01".into(),
        hypothesis_family_id: "alkene_hydroamination".into(),
        protocol_sha256: protocol_sha256.clone(),
        primary_metric: "barrier_kcal_mol".into(),
        max_trials: 4,
        significance_alpha: Some(0.05),
        correction: CorrectionMethod::Bonferroni,
        stopping_rule: "max 4 DFT TS trials".into(),
    };
    let actor = Change {
        actor: "broker:compchem",
        git_commit: Some(commit()),
    };
    store.register_protocol_preregistration(actor, &prereg)?;

    let make_sub = |id: &str, smiles: &str| CandidateSubmission {
        version: 1,
        protocol_sha256: protocol_sha256.clone(),
        candidate_id: id.into(),
        slot_values: BTreeMap::from([("substrate_smiles".into(), SlotValue::Text(smiles.into()))]),
    };

    // Drift attempt 1: newline injection in SMILES to alter functional -> blocked!
    let injected = make_sub("drift_inject", "C=CC\nfunc=HF");
    let healthy_dft = ScriptedRunner {
        backend: "orca_wb97xd",
        sha: "1".repeat(64),
        ceiling: ScientificTrust::E4a,
        ref_ceiling: Some(ScientificTrust::E2a),
        debts: &["single_functional_dft"],
        eval: Box::new(|run| {
            let text = run.artifacts().get("orca/ts.inp").unwrap();
            if text.contains("smiles=CCCCCCCC") {
                dft_diag(true, 1, true, 36.0)
            } else if text.contains("smiles=C=C(F)F") {
                // Unphysical TS with 2 imaginary modes despite low barrier!
                dft_diag(true, 2, true, 9.0)
            } else if text.contains("smiles=C=C(C)(C)C") {
                // High barrier clean rejection.
                dft_diag(true, 1, true, 28.0)
            } else {
                dft_diag(true, 1, true, 13.5)
            }
        }),
    };
    assert!(
        broker
            .verify_protocol(ProtocolVerificationRequest {
                base_commit: commit(),
                protocol: &protocol,
                submission: &injected,
                blinded_controls: Some(&controls),
                runners: &[&healthy_dft],
                lane: ProtocolLane::Heavy,
            })
            .is_err()
    );

    // Drift attempt 2: ML surrogate (E1 ceiling) attempting to execute E4a protocol -> blocked!
    let surrogate = ScriptedRunner {
        backend: "ml_barrier_surrogate",
        sha: "2".repeat(64),
        ceiling: ScientificTrust::E1,
        ref_ceiling: Some(ScientificTrust::E2a),
        debts: &["surrogate_only"],
        eval: Box::new(|_| dft_diag(true, 1, true, 11.0)),
    };
    assert!(
        broker
            .verify_protocol(ProtocolVerificationRequest {
                base_commit: commit(),
                protocol: &protocol,
                submission: &make_sub("cand_a", "C=CC"),
                blinded_controls: Some(&controls),
                runners: &[&surrogate],
                lane: ProtocolLane::Heavy,
            })
            .is_err()
    );

    // Trial 1: Unphysical TS (2 imaginary modes) -> SanityGateFailed (E0).
    let r1 = broker.verify_protocol(ProtocolVerificationRequest {
        base_commit: commit(),
        protocol: &protocol,
        submission: &make_sub("cand_bad_ts", "C=C(F)F"),
        blinded_controls: Some(&controls),
        runners: &[&healthy_dft],
        lane: ProtocolLane::Heavy,
    })?;
    assert_eq!(r1.verdict(), ProtocolVerdict::SanityGateFailed);
    assert_eq!(r1.epistemic_level(), "E0");
    store.record_protocol_trial(actor, &prereg.prereg_id, &r1, Some(true))?;

    // Trial 2: Drifting runner passes the decoy -> ControlDriftDetected (E0), decoy hidden!
    let drifting_dft = ScriptedRunner {
        backend: "orca_wb97xd",
        sha: "3".repeat(64),
        ceiling: ScientificTrust::E4a,
        ref_ceiling: Some(ScientificTrust::E2a),
        debts: &["single_functional_dft"],
        eval: Box::new(|_| dft_diag(true, 1, true, 12.0)),
    };
    let r2 = broker.verify_protocol(ProtocolVerificationRequest {
        base_commit: commit(),
        protocol: &protocol,
        submission: &make_sub("cand_under_drift", "C=CC"),
        blinded_controls: Some(&controls),
        runners: &[&drifting_dft],
        lane: ProtocolLane::Heavy,
    })?;
    assert_eq!(r2.verdict(), ProtocolVerdict::ControlDriftDetected);
    assert!(!r2.raw_json()?.to_string().contains("CCCCCCCC"));
    store.record_protocol_trial(actor, &prereg.prereg_id, &r2, Some(true))?;

    // Trial 3: Clean single-model rejection -> E2a (soft prune only, NOT E2b!).
    let r3 = broker.verify_protocol(ProtocolVerificationRequest {
        base_commit: commit(),
        protocol: &protocol,
        submission: &make_sub("cand_high_barrier", "C=C(C)(C)C"),
        blinded_controls: Some(&controls),
        runners: &[&healthy_dft],
        lane: ProtocolLane::Heavy,
    })?;
    assert_eq!(r3.verdict(), ProtocolVerdict::Rejected);
    assert_eq!(r3.epistemic_level(), "E2a");
    store.record_protocol_trial(actor, &prereg.prereg_id, &r3, Some(true))?;

    // Trial 4: Clean pass with healthy controls -> E4a with mandatory debts!
    let r4 = broker.verify_protocol(ProtocolVerificationRequest {
        base_commit: commit(),
        protocol: &protocol,
        submission: &make_sub("cand_hit", "C=CC"),
        blinded_controls: Some(&controls),
        runners: &[&healthy_dft],
        lane: ProtocolLane::Heavy,
    })?;
    assert_eq!(r4.verdict(), ProtocolVerdict::Passed);
    assert_eq!(r4.epistemic_level(), "E4a");
    assert_eq!(
        r4.epistemic_debts(),
        &["implicit_solvent", "single_functional_dft"]
    );
    // Polymorphic SealedReceipt check.
    let sealed: &dyn SealedReceipt = &r4;
    assert_eq!(sealed.domain_id(), "compchem");
    assert_eq!(sealed.spec_sha256(), protocol_sha256);
    store.record_protocol_trial(actor, &prereg.prereg_id, &r4, Some(true))?;

    // Trial 5 exceeds pre-registered budget (max_trials = 4) -> rejected by store!
    let r5 = broker.verify_protocol(ProtocolVerificationRequest {
        base_commit: commit(),
        protocol: &protocol,
        submission: &make_sub("cand_over_budget", "C=CCC"),
        blinded_controls: Some(&controls),
        runners: &[&healthy_dft],
        lane: ProtocolLane::Heavy,
    })?;
    assert!(
        store
            .record_protocol_trial(actor, &prereg.prereg_id, &r5, Some(true))
            .is_err()
    );

    // Honest cumulative audit exposes all 4 trials, Bonferroni alpha, selection bias & ControlDrift!
    let audit = store
        .audit_protocol_preregistration(&prereg.prereg_id, &ProtocolSmellPolicy::default())?
        .expect("audit present");
    assert_eq!(audit.trials_consumed, 4);
    assert_eq!(audit.passed_trials, 1);
    assert_eq!(audit.rejected_trials, 1);
    assert_eq!(audit.sanity_failed_trials, 1);
    assert_eq!(audit.control_drift_trials, 1);
    assert!((audit.effective_alpha.unwrap() - 0.0125).abs() < 1e-12);
    assert!(audit.selection_bias_warning);
    assert!(audit.smell_signals.iter().any(|s| matches!(
        s,
        ProtocolSmellSignal::ControlDrift {
            control_drift_trials: 1
        }
    )));
    Ok(())
}

#[test]
fn end_to_end_compbio_orthogonal_e4b_and_goodhart_collapse_detection() -> Result<()> {
    let dir = tempdir()?;
    let mut store = StateStore::open(dir.path().join("bio.sqlite"))?;
    let broker = Broker::new();

    // Demonstrate custom DomainPack & debt propagation for CompBio.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum BioBackend {
        SurrogateFold,
        ExplicitMd,
    }
    const BIO_BACKENDS: &[BackendDescriptor<ScientificTrust>] = &[
        BackendDescriptor {
            backend_id: "surrogate_fold",
            max_positive_ceiling: ScientificTrust::E1,
            max_refutation_ceiling: Some(ScientificTrust::E2a),
            default_debts: &["surrogate_only"],
            requires_orthogonal_pair: false,
        },
        BackendDescriptor {
            backend_id: "explicit_md",
            max_positive_ceiling: ScientificTrust::E4b,
            max_refutation_ceiling: Some(ScientificTrust::E2b),
            default_debts: &["fixed_protonation"],
            requires_orthogonal_pair: true,
        },
    ];
    struct BioPack;
    impl DomainPack for BioPack {
        type Level = ScientificTrust;
        type Backend = BioBackend;
        fn domain_id(&self) -> &'static str {
            "compbio"
        }
        fn display_name(&self) -> &'static str {
            "Computational Biology"
        }
        fn backends(&self) -> &'static [BackendDescriptor<Self::Level>] {
            BIO_BACKENDS
        }
        fn descriptor(&self, backend: Self::Backend) -> &'static BackendDescriptor<Self::Level> {
            match backend {
                BioBackend::SurrogateFold => &BIO_BACKENDS[0],
                BioBackend::ExplicitMd => &BIO_BACKENDS[1],
            }
        }
    }
    let bio = BioPack;
    assert!(
        bio.validate_backend_ceiling(BioBackend::SurrogateFold, ScientificTrust::E4a)
            .is_err()
    );
    assert!(
        bio.validate_backend_ceiling(BioBackend::ExplicitMd, ScientificTrust::E4b)
            .is_ok()
    );
    assert_eq!(
        bio.default_debts(BioBackend::ExplicitMd),
        &["fixed_protonation"]
    );

    let protocol = ProtocolSpec {
        version: 1,
        protocol_id: "binder_ortho_md_v1".into(),
        domain_id: "compbio".into(),
        backend_id: "ortho_md".into(),
        epistemic_ceiling: ScientificTrust::E4b,
        default_debts: vec!["in_silico_only".into()],
        pinned_inputs: BTreeMap::from([("target_receptor_pdb".into(), "c".repeat(64))]),
        frozen_parameters: BTreeMap::from([("sim_ns".into(), ParamValue::Float(100.0))]),
        slots: vec![SlotSpec {
            name: "binder_fasta".into(),
            kind: SlotKind::Text {
                max_len: 512,
                profile: TextProfile::Fasta,
            },
        }],
        artifact_templates: vec![ArtifactTemplate {
            path: "design/binder.fa".into(),
            template: ">candidate\n{{slot:binder_fasta}}\n# sim_ns={{param:sim_ns}}\n".into(),
        }],
        sanity_gates: vec![NamedMetricRule {
            rule_id: "no_clash".into(),
            rule: MetricRule::MaxFloat {
                metric: "clashscore".into(),
                max: 5.0,
            },
        }],
        acceptance_criteria: vec![NamedMetricRule {
            rule_id: "stable_rmsd".into(),
            rule: MetricRule::MaxFloat {
                metric: "backbone_rmsd_angstrom".into(),
                max: 2.0,
            },
        }],
        orthogonal_policy: OrthogonalPolicy {
            min_verifiers: 2,
            require_distinct_backends: true,
            metric_tolerances: BTreeMap::from([("backbone_rmsd_angstrom".into(), 0.5)]),
        },
        control_policy: BlindedControlPolicy::default(),
        preregistration_required: true,
    };
    let protocol_sha256 = protocol.protocol_sha256()?;
    let prereg = PreRegistrationSpec {
        version: 1,
        prereg_id: "PREREG-BIO-01".into(),
        hypothesis_family_id: "de_novo_binders".into(),
        protocol_sha256: protocol_sha256.clone(),
        primary_metric: "backbone_rmsd_angstrom".into(),
        max_trials: 10,
        significance_alpha: None,
        correction: CorrectionMethod::None,
        stopping_rule: "stop on Goodhart collapse or 10 trials".into(),
    };
    let actor = Change {
        actor: "broker:compbio",
        git_commit: Some(commit()),
    };
    store.register_protocol_preregistration(actor, &prereg)?;

    let amber = ScriptedRunner {
        backend: "openmm_amber19",
        sha: "a".repeat(64),
        ceiling: ScientificTrust::E4b,
        ref_ceiling: Some(ScientificTrust::E2b),
        debts: &["fixed_protonation"],
        eval: Box::new(|run| {
            let fa = run.artifacts().get("design/binder.fa").unwrap();
            let rmsd = if fa.contains("MKTAYIAK") { 1.2 } else { 4.8 };
            ParsedDiagnostics {
                metrics: BTreeMap::from([
                    ("clashscore".into(), DiagnosticValue::Float(1.5)),
                    (
                        "backbone_rmsd_angstrom".into(),
                        DiagnosticValue::Float(rmsd),
                    ),
                ]),
            }
        }),
    };
    let charmm = ScriptedRunner {
        backend: "openmm_charmm36m",
        sha: "b".repeat(64),
        ceiling: ScientificTrust::E4b,
        ref_ceiling: Some(ScientificTrust::E2b),
        debts: &["fixed_protonation"],
        eval: Box::new(|run| {
            let fa = run.artifacts().get("design/binder.fa").unwrap();
            let rmsd = if fa.contains("MKTAYIAK") { 1.4 } else { 5.0 };
            ParsedDiagnostics {
                metrics: BTreeMap::from([
                    ("clashscore".into(), DiagnosticValue::Float(1.8)),
                    (
                        "backbone_rmsd_angstrom".into(),
                        DiagnosticValue::Float(rmsd),
                    ),
                ]),
            }
        }),
    };

    // Passing the same runner twice MUST fail distinct-backend orthogonal policy.
    let sub_good = CandidateSubmission {
        version: 1,
        protocol_sha256: protocol_sha256.clone(),
        candidate_id: "seq_good".into(),
        slot_values: BTreeMap::from([("binder_fasta".into(), SlotValue::Text("MKTAYIAK".into()))]),
    };
    assert!(
        broker
            .verify_protocol(ProtocolVerificationRequest {
                base_commit: commit(),
                protocol: &protocol,
                submission: &sub_good,
                blinded_controls: None,
                runners: &[&amber, &amber],
                lane: ProtocolLane::Heavy,
            })
            .is_err()
    );

    // 4 surrogate-passed adversarial sequences unfold in BOTH force fields -> E2b hard falsification
    // and triggers SurrogateGoodhartCollapse smell signal!
    for (idx, seq) in ["AAAAAAAA", "GGGGGGGG", "VVVVVVVV", "LLLLLLLL"]
        .into_iter()
        .enumerate()
    {
        let sub = CandidateSubmission {
            version: 1,
            protocol_sha256: protocol_sha256.clone(),
            candidate_id: format!("surrogate_hit_{idx}"),
            slot_values: BTreeMap::from([("binder_fasta".into(), SlotValue::Text(seq.into()))]),
        };
        let run = broker.verify_protocol(ProtocolVerificationRequest {
            base_commit: commit(),
            protocol: &protocol,
            submission: &sub,
            blinded_controls: None,
            runners: &[&amber, &charmm],
            lane: ProtocolLane::Heavy,
        })?;
        assert_eq!(run.verdict(), ProtocolVerdict::Rejected);
        assert_eq!(run.epistemic_level(), "E2b");
        assert!(run.epistemic_level_typed().is_hard_refutation());
        store.record_protocol_trial(actor, &prereg.prereg_id, &run, Some(true))?;
    }

    let audit = store
        .audit_protocol_preregistration(&prereg.prereg_id, &ProtocolSmellPolicy::default())?
        .expect("audit present");
    assert!(audit.smell_signals.iter().any(|s| matches!(
        s,
        ProtocolSmellSignal::SurrogateGoodhartCollapse {
            surrogate_passed_trials: 4,
            physical_passed_trials: 0,
            ..
        }
    )));

    // True orthogonal hit reaches E4b and propagates debts cleanly in AND-OR graph!
    let hit = broker.verify_protocol(ProtocolVerificationRequest {
        base_commit: commit(),
        protocol: &protocol,
        submission: &sub_good,
        blinded_controls: None,
        runners: &[&amber, &charmm],
        lane: ProtocolLane::Heavy,
    })?;
    assert_eq!(hit.verdict(), ProtocolVerdict::Passed);
    assert_eq!(hit.epistemic_level(), "E4b");

    let hit_assessment = EpistemicAssessment::with_debts(
        hit.epistemic_level_typed(),
        hit.epistemic_debts()
            .iter()
            .map(|code| EpistemicDebt::new(code.clone(), hit.candidate_id(), "")),
    );
    let top = aggregate_assessment_plan(
        &EpistemicAssessment::new(ScientificTrust::E5),
        [hit_assessment.clone()],
    )
    .expect("valid plan");
    assert_eq!(top.level, ScientificTrust::E4b);
    assert_eq!(top.debts.len(), 2);
    assert_eq!(best_assessment_plan([top.clone()]), Some(top));
    Ok(())
}
