use ai4math_core::{
    BeliefPropagationConfig, CitationStatus, ConflictEdgeKind, KnowledgeSourceKind,
    KnowledgeStratum, ReproducibilityClass, ScientificTrust, TagAttenuationPolicy,
};
use ai4math_evidence::SealedReceipt;
use ai4math_store::{
    Change, EmbeddingProvider, InMemoryCosineIndex, NewClaim, NewConflictEdge, NewEpistemicNode,
    NewEpistemicPlan, NewPlan, PlanStatus, SemanticSyncStats, StateStore, VaultQuery, Wiki,
    WikiQuery,
};
use anyhow::Result;
use rusqlite::Connection;
use serde_json::{Value, json};
use std::fs;
use tempfile::tempdir;

struct StubSealedReceipt {
    domain_id: &'static str,
    backend_id: &'static str,
    level: &'static str,
    debts: Vec<String>,
    receipt_sha256: String,
}

// SAFETY: Test-only fixture inside integration test crate.
unsafe impl SealedReceipt for StubSealedReceipt {
    fn domain_id(&self) -> &str {
        self.domain_id
    }
    fn backend_id(&self) -> &str {
        self.backend_id
    }
    fn target_id(&self) -> &str {
        "target"
    }
    fn base_commit(&self) -> &str {
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    }
    fn spec_sha256(&self) -> &str {
        "1111111111111111111111111111111111111111111111111111111111111111"
    }
    fn candidate_sha256(&self) -> &str {
        "2222222222222222222222222222222222222222222222222222222222222222"
    }
    fn inputs_sha256(&self) -> &str {
        "3333333333333333333333333333333333333333333333333333333333333333"
    }
    fn verifier_sha256(&self) -> &str {
        "4444444444444444444444444444444444444444444444444444444444444444"
    }
    fn epistemic_level(&self) -> &str {
        self.level
    }
    fn epistemic_debts(&self) -> &[String] {
        &self.debts
    }
    fn issued_unix_seconds(&self) -> u64 {
        1_700_000_000
    }
    fn elapsed_seconds(&self) -> f64 {
        1.0
    }
    fn receipt_sha256(&self) -> &str {
        &self.receipt_sha256
    }
    fn raw_json(&self) -> Result<Value> {
        Ok(json!({"receipt_sha256": self.receipt_sha256}))
    }
}

#[test]
fn generalized_claim_graph_lifts_legacy_itp_enforces_domain_triggers_and_persists_bp() -> Result<()>
{
    let dir = tempdir()?;
    let db_path = dir.path().join("state.sqlite");
    let mut store = StateStore::open(&db_path)?;
    let actor = Change {
        actor: "orchestrator",
        git_commit: Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
    };

    // 1. Add a legacy ITP claim + plan to verify seamless lifting into unified BP.
    store.create_claim(
        NewClaim {
            id: "ITP-LEMMA",
            title: "Legacy ITP lemma",
            informal_statement: "Spectral gap positivity",
            engine: "lean4",
            formal_target: Some("Spec.lean#gap"),
            module_path: Some("Spec.lean"),
            critical: false,
        },
        actor,
    )?;
    store.create_claim(
        NewClaim {
            id: "ITP-GOAL",
            title: "Legacy ITP goal",
            informal_statement: "Main theorem",
            engine: "lean4",
            formal_target: Some("Main.lean#main"),
            module_path: Some("Main.lean"),
            critical: true,
        },
        actor,
    )?;
    store.create_plan(
        NewPlan {
            id: "ITP-PLAN",
            target: "ITP-GOAL",
            name: "Direct route",
            track: "infrastructure",
            status: PlanStatus::Active,
            dependencies: &[("ITP-LEMMA", "lemma")],
        },
        actor,
    )?;

    // 2. Domain <-> level mismatch is blocked both in Rust and by SQLite triggers!
    assert!(
        store
            .upsert_epistemic_node(
                actor,
                &NewEpistemicNode {
                    node_id: "BAD-MIX",
                    domain_id: "itp",
                    backend_id: "lean4",
                    title: "Invalid",
                    statement: "ITP cannot use E4a",
                    is_critical_path: false,
                    local_level: "E4a",
                    condition_tags: &[],
                    local_debts: &[],
                    reproducibility_class: None,
                    source_entry_id: None,
                },
            )
            .is_err()
    );
    assert!(
        store
            .upsert_epistemic_node(
                actor,
                &NewEpistemicNode {
                    node_id: "BAD-MIX-2",
                    domain_id: "compchem",
                    backend_id: "openmm_fep",
                    title: "Invalid",
                    statement: "CompChem cannot use L5",
                    is_critical_path: false,
                    local_level: "L5",
                    condition_tags: &[],
                    local_debts: &[],
                    reproducibility_class: None,
                    source_entry_id: None,
                },
            )
            .is_err()
    );

    // 3. Build a CompChem subgraph with a tagged literature node + critical root.
    store.upsert_epistemic_node(
        actor,
        &NewEpistemicNode {
            node_id: "CHEM-LIT-1",
            domain_id: "compchem",
            backend_id: "literature",
            title: "Literature barrier claim",
            statement: "Barrier is 14 kcal/mol (missing solvent condition)",
            is_critical_path: false,
            local_level: "E4a",
            condition_tags: &["unreproduced_literature", "incomplete_conditions"],
            local_debts: &[],
            reproducibility_class: Some(ReproducibilityClass::ReplicableByProtocol),
            source_entry_id: Some("LIT-001"),
        },
    )?;
    store.upsert_epistemic_node(
        actor,
        &NewEpistemicNode {
            node_id: "CHEM-ROOT",
            domain_id: "compchem",
            backend_id: "openmm_fep",
            title: "Catalyst viability",
            statement: "Reaction proceeds at room temperature",
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
            plan_id: "CHEM-PLAN-1",
            target_node_id: "CHEM-ROOT",
            name: "DFT barrier route",
            dependencies: &["CHEM-LIT-1"],
            active: true,
        },
    )?;

    let policy = TagAttenuationPolicy::default();
    let cfg = BeliefPropagationConfig::default();
    let snap1 = store.record_belief_snapshot(actor, &policy, &cfg)?;
    assert!(snap1.report.converged);
    // Legacy ITP claims AND new CompChem nodes both exist in unified report!
    assert!(snap1.report.nodes.contains_key("ITP-GOAL"));
    assert!(snap1.report.nodes.contains_key("CHEM-ROOT"));
    let before_belief = snap1.report.nodes["CHEM-ROOT"].propagated_opinion.belief;
    assert!(before_belief <= 0.45 + 1e-9);
    assert!(
        snap1
            .report
            .voi_rankings
            .iter()
            .any(|v| v.node_id == "CHEM-LIT-1" && v.belief_gain_on_critical_paths > 0.3)
    );

    // 4. Upgrade CHEM-LIT-1 via a broker-sealed E4b receipt clearing literature tags!
    let receipt = StubSealedReceipt {
        domain_id: "compchem",
        backend_id: "ortho_dft",
        level: ScientificTrust::E4b.as_str(),
        debts: vec![],
        receipt_sha256: "e".repeat(64),
    };
    let upgraded = store.upgrade_epistemic_node_with_receipt(
        actor,
        "CHEM-LIT-1",
        &receipt,
        &["unreproduced_literature", "incomplete_conditions"],
    )?;
    assert_eq!(upgraded.local_level, "E4b");
    assert!(upgraded.condition_tags.is_empty());

    let snap2 = store.record_belief_snapshot(actor, &policy, &cfg)?;
    let after_belief = snap2.report.nodes["CHEM-ROOT"].propagated_opinion.belief;
    assert!(after_belief > before_belief + 0.35);

    // 5. Add a contradicting node & edge -> conflict_mass rises cleanly.
    store.upsert_epistemic_node(
        actor,
        &NewEpistemicNode {
            node_id: "CHEM-CONTRA",
            domain_id: "compchem",
            backend_id: "wetlab_lims",
            title: "Contradicting assay",
            statement: "No turnover observed",
            is_critical_path: false,
            local_level: "E4a",
            condition_tags: &[],
            local_debts: &[],
            reproducibility_class: None,
            source_entry_id: None,
        },
    )?;
    store.record_epistemic_conflict_edge(
        actor,
        &NewConflictEdge {
            edge_id: "EDGE-C1",
            source_node_id: "CHEM-CONTRA",
            target_node_id: "CHEM-LIT-1",
            kind: ConflictEdgeKind::Contradicts,
            weight: 0.9,
            rationale: "contradicting negative control",
        },
        None,
    )?;
    let report3 = store.evaluate_belief_graph(&policy, &cfg)?;
    assert!(report3.nodes["CHEM-LIT-1"].conflict_mass > 0.4);

    // 6. Verify SQLite triggers block deleting belief snapshots and cross-domain raw updates.
    drop(store);
    let raw = Connection::open(&db_path)?;
    assert!(
        raw.execute("DELETE FROM epistemic_belief_snapshots", [])
            .is_err()
    );
    assert!(
        raw.execute(
            "UPDATE epistemic_nodes SET local_level='L5' WHERE node_id='CHEM-LIT-1'",
            []
        )
        .is_err()
    );
    Ok(())
}

struct ToyEmbeddingProvider;
impl EmbeddingProvider for ToyEmbeddingProvider {
    fn model_id(&self) -> &str {
        "toy-3d-v1"
    }
    fn dim(&self) -> usize {
        3
    }
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        Ok(texts
            .iter()
            .map(|t| {
                let low = t.to_ascii_lowercase();
                let x = if low.contains("kinase")
                    || low.contains("binding")
                    || low.contains("ligand")
                {
                    1.0
                } else {
                    0.0
                };
                let y = if low.contains("spectral") || low.contains("laplacian") {
                    1.0
                } else {
                    0.0
                };
                let z = if low.contains("catalyst") || low.contains("barrier") {
                    1.0
                } else {
                    0.0
                };
                if x == 0.0 && y == 0.0 && z == 0.0 {
                    vec![0.1, 0.1, 0.1]
                } else {
                    vec![x, y, z]
                }
            })
            .collect())
    }
}

#[test]
fn four_stratum_vault_supports_literature_facets_shadow_hint_conflicts_and_vector_rrf() -> Result<()>
{
    let dir = tempdir()?;
    let root = dir.path().join(".mathwiki");
    for sub in ["core", "sediment", "graveyard", "literature"] {
        fs::create_dir_all(root.join(sub))?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
    }

    // 1. ITP CORE entry
    fs::write(
        root.join("core/COR-SPEC.md"),
        r#"---
id: COR-SPEC
stratum: core
trust_level: L5
engine: lean4
status: active
title: Spectral gap bound
informal_gloss: Normalized Laplacian spectral gap positivity for connected graphs
symbols:
  conclusion_head: LT.lt
  hypothesis_heads: [Connected]
---
"#,
    )?;

    // 2. CompChem CORE entry with domain facets
    fs::write(
        root.join("core/COR-CHEM.md"),
        r#"---
id: COR-CHEM
stratum: core
domain_id: compchem
trust_level: E4a
engine: openmm_fep
status: active
title: EGFR kinase inhibitor affinity
informal_gloss: Explicit-solvent FEP binding free energy for EGFR kinase ligand
facets:
  uniprot_id: [P00533]
  metric: [ddg_kcal_mol]
  inchikey: [BSYNRYMUTXBXSQ-UHFFFAOYSA-N]
---
"#,
    )?;

    // 3. Automatable LITERATURE entry (with incomplete_conditions tag, auto-tagged unreproduced_literature)
    fs::write(
        root.join("literature/LIT-SPEC.md"),
        r#"---
id: LIT-SPEC
stratum: literature
domain_id: itp
trust_level: L0
engine: literature
status: active
reproducibility_class: formalizable_in_itp
citation_status: active
condition_tags: [missing_standing_assumptions]
title: Cheeger spectral gap inequality
informal_gloss: Literature theorem linking spectral gap to conductance
facets:
  doi: ["10.1000/cheeger.1970"]
---
"#,
    )?;

    // 4. CompChem LITERATURE entry sharing (uniprot_id, metric) facet with COR-CHEM + explicit contradicts edge
    fs::write(
        root.join("literature/LIT-CHEM.md"),
        r#"---
id: LIT-CHEM
stratum: literature
domain_id: compchem
trust_level: E0
engine: literature
status: active
reproducibility_class: replicable_by_protocol
citation_status: active
condition_tags: [incomplete_conditions]
title: Reported EGFR affinity measurement
informal_gloss: Wet-lab measurement for EGFR kinase binding affinity without pH
facets:
  uniprot_id: [P00533]
  metric: [ddg_kcal_mol]
  doi: ["10.1000/egfr.2024"]
edges:
  - dst_id: COR-CHEM
    edge_type: contradicts
---
"#,
    )?;

    // 5. Retracted LITERATURE entry (must be hidden by default and excluded from automatable list!)
    fs::write(
        root.join("literature/LIT-RETRACTED.md"),
        r#"---
id: LIT-RETRACTED
stratum: literature
domain_id: compchem
trust_level: E0
engine: literature
status: retracted
reproducibility_class: replicable_by_protocol
citation_status: retracted
title: Retracted EGFR claim
informal_gloss: Retracted kinase binding claim
facets:
  uniprot_id: [P00533]
  metric: [ddg_kcal_mol]
---
"#,
    )?;

    let mut wiki = Wiki::open(root.join(".index.sqlite"))?;
    let count = wiki.rebuild(&root)?;
    assert_eq!(count, 5);

    // A. Legacy ITP Wiki::search stays 100% compatible and surfaces 3rd-channel literature ShadowHint!
    let legacy = wiki.search(WikiQuery::new("spectral gap"))?;
    assert_eq!(legacy.hits.len(), 1);
    assert_eq!(legacy.hits[0].summary.id, "COR-SPEC");
    let hint = legacy.shadow_hint.expect("literature shadow hint present");
    assert_eq!(hint.literature_count, 1);
    assert_eq!(hint.top_literature_id.as_deref(), Some("LIT-SPEC"));

    // B. Generalized VaultQuery searches CompChem facets and hides retracted literature by default.
    let chem_lit = wiki.search_vault(VaultQuery {
        facets: &[("uniprot_id", "P00533"), ("metric", "ddg_kcal_mol")],
        ..VaultQuery::new("", KnowledgeStratum::Literature)
    })?;
    assert_eq!(chem_lit.hits.len(), 1);
    assert_eq!(chem_lit.hits[0].id, "LIT-CHEM");
    assert!(
        chem_lit.hits[0]
            .condition_tags
            .contains(&"unreproduced_literature".to_owned())
    );
    assert!(
        chem_lit.hits[0]
            .condition_tags
            .contains(&"incomplete_conditions".to_owned())
    );
    assert_eq!(
        chem_lit.hits[0].source_kind,
        KnowledgeSourceKind::LiteratureExtracted
    );

    // Explicit include_retracted surfaces the retracted card with CitationStatus::Retracted.
    let with_retracted = wiki.search_vault(VaultQuery {
        facets: &[("uniprot_id", "P00533")],
        include_retracted: true,
        ..VaultQuery::new("", KnowledgeStratum::Literature)
    })?;
    assert_eq!(with_retracted.hits.len(), 2);
    assert!(
        with_retracted.hits.iter().any(
            |h| h.id == "LIT-RETRACTED" && h.citation_status == Some(CitationStatus::Retracted)
        )
    );

    // C. Conflict clusters surface BOTH the explicit contradicts edge and the shared facet cluster!
    let clusters = wiki.find_conflict_clusters(&["uniprot_id", "metric"])?;
    assert!(
        clusters
            .iter()
            .any(|c| c.reason == "explicit_contradicts_edge"
                && c.entry_ids == vec!["LIT-CHEM", "COR-CHEM"])
    );
    assert!(
        clusters
            .iter()
            .any(|c| c.reason == "shared_facet_tuple_candidates"
                && c.entry_ids == vec!["COR-CHEM", "LIT-CHEM"])
    );

    // D. Automatable literature excludes retracted entry!
    let automatable = wiki.list_automatable_literature()?;
    let auto_ids: Vec<&str> = automatable.iter().map(|e| e.entry_id.as_str()).collect();
    assert_eq!(auto_ids, vec!["LIT-CHEM", "LIT-SPEC"]);

    // E. SemanticIndex sync uses SQLite vector cache + 3-way RRF hybrid search!
    let embedder = ToyEmbeddingProvider;
    let mut sem_index = InMemoryCosineIndex::new(3)?;
    let first_sync = wiki.sync_semantic_index(&embedder, &mut sem_index)?;
    assert_eq!(
        first_sync,
        SemanticSyncStats {
            total_entries: 5,
            cache_hits: 0,
            embedded_fresh: 5,
        }
    );
    // Second sync hits 100% cache!
    let second_sync = wiki.sync_semantic_index(&embedder, &mut sem_index)?;
    assert_eq!(
        second_sync,
        SemanticSyncStats {
            total_entries: 5,
            cache_hits: 5,
            embedded_fresh: 0,
        }
    );
    // Query with term "ligand" (appears in COR-CHEM) via hybrid search retrieves COR-CHEM cleanly.
    let hybrid = wiki.search_vault_hybrid(
        VaultQuery::new("kinase binding", KnowledgeStratum::Core),
        &embedder,
        &sem_index,
    )?;
    assert!(!hybrid.hits.is_empty());
    assert_eq!(hybrid.hits[0].id, "COR-CHEM");

    // F. Anti-literature-laundering: LITERATURE card claiming E5 fails rebuild atomically!
    fs::write(
        root.join("literature/LIT-LAUNDER.md"),
        r#"---
id: LIT-LAUNDER
stratum: literature
domain_id: compchem
trust_level: E5
engine: literature
status: active
title: Laundered claim
---
"#,
    )?;
    assert!(wiki.rebuild(&root).is_err());
    Ok(())
}
