use ai4math_store::{
    ClaimEngine, SedimentArtifact, SedimentInput, Trust, Wiki, WikiManagedCoreEntry,
    WikiManagedGeneration, WikiQuery, WikiStratum, render_sediment,
};
use anyhow::Result;
use rusqlite::{Connection, params};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, fs, path::Path};

const GENERATION: WikiManagedGeneration<'_> = WikiManagedGeneration {
    head: "123456789abcdef0123456789abcdef012345678",
    event_seq: 42,
};
const NEXT: WikiManagedGeneration<'_> = WikiManagedGeneration {
    head: "0123456789abcdef0123456789abcdef01234567",
    event_seq: 43,
};
const ORIGIN: &str = "CLM-origin";
const TARGET: &str = "CLM-destination";
const PROMOTED: &str = "23456789abcdef0123456789abcdef0123456789";

fn markdown(root: &Path, stratum: &str, file: &str, contents: &str) -> Result<()> {
    let directory = root.join(stratum);
    fs::create_dir_all(&directory)?;
    fs::write(directory.join(file), contents)?;
    Ok(())
}

fn core_markdown(id: &str, title: &str, extra: &str) -> String {
    format!(
        "---
id: {id}
stratum: core
trust_level: L5
engine: lean4
status: active
title: {title}
{extra}---
"
    )
}

fn archive(root: &Path) -> Result<SedimentArtifact> {
    let source = "theorem archivedBodyMarker : True := True.intro
";
    let artifact = render_sediment(SedimentInput {
        claim_id: ORIGIN,
        engine: "lean4",
        verified_at_commit: "historical-commit",
        source_module: "Historical.Toric",
        target_decl: "Toric",
        source,
        source_sha256: &format!("{:x}", Sha256::digest(source)),
        dependency_hashes: &BTreeMap::new(),
        dependency_order: &[],
    })?;
    markdown(root, "sediment", "archive.md", artifact.markdown())?;
    Ok(artifact)
}

fn projected(artifact: &SedimentArtifact, digest_digit: &str) -> WikiManagedCoreEntry {
    WikiManagedCoreEntry {
        id: format!("CORE-ADAPT-{}", digest_digit.repeat(64)),
        archive_id: artifact.id().into(),
        origin_claim: ORIGIN.into(),
        claim_id: TARGET.into(),
        title: "Toric managedprojection".into(),
        informal_gloss: "Adaptation metadata only".into(),
        verified_at_commit: PROMOTED.into(),
    }
}

#[test]
fn exact_generation_only_exposes_metadata_and_incident_edges() -> Result<()> {
    let root = tempfile::tempdir()?;
    let root = root.path();
    let artifact = archive(root)?;
    let managed = projected(&artifact, "a");
    markdown(
        root,
        "core",
        "ordinary.md",
        &core_markdown("COR-live", "Toric base", ""),
    )?;
    let db = root.join("wiki.sqlite");
    let mut writer = Wiki::open(&db)?;
    assert_eq!(
        writer.rebuild_with_managed_core(root, GENERATION, std::slice::from_ref(&managed))?,
        3
    );
    let reader = Wiki::open_readonly(&db)?;
    let query = WikiQuery::new("Toric");
    let generic = reader.search(query)?;
    assert_eq!(generic.hits.len(), 1);
    assert_eq!(generic.hits[0].summary.id, "COR-live");
    let generic_hint = serde_json::to_value(generic.shadow_hint.as_ref().unwrap())?;
    assert_eq!(generic_hint["sediment_count"], 1);
    let visible = reader.search_at(query, GENERATION)?;
    assert_eq!(visible.hits.len(), 2);
    let hit = visible
        .hits
        .iter()
        .find(|hit| hit.summary.id == managed.id)
        .unwrap();
    assert_eq!(hit.summary.engine, ClaimEngine::Lean4);
    assert_eq!(hit.summary.verified_at_commit.as_deref(), Some(PROMOTED));
    assert_eq!(hit.stratum, WikiStratum::Core);
    assert_eq!(hit.trust_level, Trust::L5);
    assert_eq!(hit.status, "active");
    assert_eq!(hit.informal_gloss, managed.informal_gloss);
    assert!(hit.formal_signature.is_none());
    assert!(!root.join(&hit.file_path).exists());
    assert_eq!(
        serde_json::to_value(visible.shadow_hint.as_ref().unwrap())?,
        generic_hint
    );
    assert!(
        reader
            .search_at(WikiQuery::new("archivedBodyMarker"), GENERATION)?
            .hits
            .is_empty()
    );

    for generation in [
        WikiManagedGeneration {
            event_seq: GENERATION.event_seq + 1,
            ..GENERATION
        },
        WikiManagedGeneration {
            head: NEXT.head,
            ..GENERATION
        },
    ] {
        assert_eq!(
            serde_json::to_value(reader.search_at(query, generation)?)?,
            serde_json::to_value(&generic)?
        );
        assert!(
            reader
                .provenance_at(&managed.id, 2, 10, generation)?
                .edges
                .is_empty()
        );
        assert_eq!(
            reader
                .provenance_at(artifact.id(), 2, 10, generation)?
                .edges
                .len(),
            1
        );
        assert!(
            reader
                .provenance_at(TARGET, 2, 10, generation)?
                .edges
                .is_empty()
        );
    }
    assert!(
        reader
            .search(WikiQuery::new("managedprojection"))?
            .hits
            .is_empty()
    );
    assert!(reader.provenance(&managed.id, 2, 10)?.edges.is_empty());
    assert_eq!(reader.provenance(artifact.id(), 2, 10)?.edges.len(), 1);
    assert!(!reader.provenance(artifact.id(), 1, 1)?.truncated);
    assert!(reader.provenance(TARGET, 2, 10)?.edges.is_empty());

    let graph = reader.provenance_at(artifact.id(), 2, 10, GENERATION)?;
    assert!(graph.edges.iter().any(|edge| {
        edge.src_id == artifact.id()
            && edge.dst_id == managed.id
            && edge.src_stratum == WikiStratum::Sediment
            && edge.dst_stratum == Some(WikiStratum::Core)
            && edge.edge_type == "superseded_by"
            && edge.depth == 1
    }));
    assert!(graph.edges.iter().any(|edge| {
        edge.src_id == managed.id
            && edge.dst_id == TARGET
            && edge.src_stratum == WikiStratum::Core
            && edge.dst_stratum.is_none()
            && edge.edge_type == "born_from_claim"
            && edge.depth == 2
    }));
    assert!(
        reader
            .provenance_at(artifact.id(), 1, 1, GENERATION)?
            .truncated
    );

    assert_eq!(writer.rebuild(root)?, 2);
    assert_eq!(reader.search_at(query, GENERATION)?.hits.len(), 1);
    assert_eq!(
        reader
            .provenance_at(artifact.id(), 2, 10, GENERATION)?
            .edges
            .len(),
        1
    );
    let probe = Connection::open(db)?;
    for table in ["managed_core_generation", "managed_core_fts"] {
        let count: u32 = probe.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
            row.get(0)
        })?;
        assert_eq!(count, 0, "{table}");
    }
    Ok(())
}

#[test]
fn invalid_archives_and_partial_projection_leave_existing_generation_intact() -> Result<()> {
    let root = tempfile::tempdir()?;
    let root = root.path();
    let artifact = archive(root)?;
    let managed = projected(&artifact, "a");
    let mut wiki = Wiki::open(root.join("wiki.sqlite"))?;
    assert_eq!(
        wiki.rebuild_with_managed_core(root, GENERATION, std::slice::from_ref(&managed))?,
        2
    );
    markdown(
        root,
        "core",
        "next.md",
        &core_markdown("COR-next", "latermarkdown", ""),
    )?;

    let mut bad_origin = projected(&artifact, "b");
    bad_origin.origin_claim = "wrong-origin".into();
    let mut missing_archive = projected(&artifact, "c");
    missing_archive.archive_id = format!("SED-{}", "f".repeat(64));
    let bad_id = projected(&artifact, "A");
    let mut bad_commit = projected(&artifact, "d");
    bad_commit.verified_at_commit = "c".repeat(64);
    let mut bad_title = projected(&artifact, "e");
    bad_title.title = "é".repeat(257);
    for (bad, expected_error) in [
        (bad_origin, "matching origin claim"),
        (missing_archive, "requires indexed Sediment archive"),
        (bad_id, "64 lowercase hex"),
        (bad_commit, "invalid verification commit"),
        (bad_title, "oversized managed Wiki Core metadata"),
        (managed.clone(), "UNIQUE constraint failed"),
    ] {
        let error = wiki
            .rebuild_with_managed_core(root, NEXT, &[managed.clone(), bad])
            .unwrap_err();
        assert!(format!("{error:#}").contains(expected_error));
        assert_eq!(
            wiki.search_at(WikiQuery::new("managedprojection"), GENERATION)?
                .hits[0]
                .summary
                .id,
            managed.id
        );
        assert!(
            wiki.search_at(WikiQuery::new("managedprojection"), NEXT)?
                .hits
                .is_empty()
        );
        assert!(
            wiki.search(WikiQuery::new("latermarkdown"))?
                .hits
                .is_empty()
        );
        assert_eq!(
            wiki.provenance_at(&managed.id, 1, 10, GENERATION)?
                .edges
                .len(),
            2
        );
    }
    for bad_generation in [
        WikiManagedGeneration {
            head: "abbreviated",
            ..NEXT
        },
        WikiManagedGeneration {
            event_seq: -1,
            ..NEXT
        },
    ] {
        let error = wiki
            .rebuild_with_managed_core(root, bad_generation, &[])
            .unwrap_err();
        assert!(format!("{error:#}").contains("invalid managed Wiki generation"));
        assert_eq!(
            wiki.provenance_at(&managed.id, 1, 10, GENERATION)?
                .edges
                .len(),
            2
        );
    }
    // Keep valid YAML, ID and origin but change the exact archive bytes.
    markdown(
        root,
        "sediment",
        "archive.md",
        &format!(
            "{}
",
            artifact.markdown()
        ),
    )?;
    let error = wiki
        .rebuild_with_managed_core(root, NEXT, &[managed.clone()])
        .unwrap_err();
    assert!(format!("{error:#}").contains("not canonical v1 Markdown"));
    assert_eq!(
        wiki.provenance_at(&managed.id, 1, 10, GENERATION)?
            .edges
            .len(),
        2
    );
    assert!(
        wiki.search(WikiQuery::new("latermarkdown"))?
            .hits
            .is_empty()
    );
    Ok(())
}

#[test]
fn markdown_cannot_claim_generated_ids_or_add_incident_edges() -> Result<()> {
    let root = tempfile::tempdir()?;
    let root = root.path();
    let mut wiki = Wiki::open(root.join("wiki.sqlite"))?;
    let spoof = format!("CORE-ADAPT-{}", "a".repeat(64));
    for source in [
        core_markdown(&spoof, "fake", ""),
        core_markdown("core-adapt-spoof", "fake", ""),
        core_markdown(
            "COR-fake",
            "fake",
            &format!(
                "edges:
  - {{dst_id: {spoof}, edge_type: superseded_by}}
"
            ),
        ),
        core_markdown(
            "COR-fake",
            "fake",
            &format!(
                "origin_claim: {spoof}
"
            ),
        ),
    ] {
        markdown(root, "core", "fake.md", &source)?;
        let error = wiki.rebuild(root).unwrap_err();
        assert!(format!("{error:#}").contains("reserved Core ID namespace"));
    }
    Ok(())
}

#[test]
fn rebuild_rejects_an_aliased_wiki_root() -> Result<()> {
    let root = tempfile::tempdir()?;
    let real = root.path().join("real");
    let linked = root.path().join("linked");
    fs::create_dir(&real)?;
    std::os::unix::fs::symlink(&real, &linked)?;
    let mut wiki = Wiki::open(root.path().join("wiki.sqlite"))?;
    assert!(wiki.rebuild(linked.join(".")).is_err());
    Ok(())
}

#[test]
fn legacy_index_guarded_reads_still_hide_reserved_ids_and_edges() -> Result<()> {
    let root = tempfile::tempdir()?;
    let root = root.path();
    let db = root.join("wiki.sqlite");
    assert!(Wiki::open_readonly(&db).is_err());
    assert!(!db.exists());
    markdown(
        root,
        "core",
        "ordinary.md",
        &core_markdown("COR-live", "Toric base", ""),
    )?;
    assert_eq!(Wiki::open(&db)?.rebuild(root)?, 1);
    let probe = Connection::open(&db)?;
    probe.execute_batch("DROP TABLE managed_core_generation; DROP TABLE managed_core_fts;")?;
    let spoof = "CORE-ADAPT-legacy";
    probe.execute(
        "INSERT INTO entries (id,stratum,trust_level,engine,status,title,informal_gloss,created_at,updated_at,file_path)
        VALUES (?1,'core','L5','lean4','active','fake','','unknown','unknown','core/spoof.md')",
        [spoof],
    )?;
    probe.execute(
        "INSERT INTO entry_edges (src_id,dst_id,edge_type) VALUES ('COR-live',?1,'depends_on'), (?1,?2,'born_from_claim')",
        params![spoof, TARGET],
    )?;
    let reader = Wiki::open_readonly(db)?;
    assert_eq!(
        reader.search_at(WikiQuery::default(), GENERATION)?.hits[0]
            .summary
            .id,
        "COR-live"
    );
    assert_eq!(
        reader
            .search_at(WikiQuery::default(), GENERATION)?
            .hits
            .len(),
        1
    );
    for id in ["COR-live", spoof, TARGET] {
        assert!(
            reader
                .provenance_at(id, 2, 10, GENERATION)?
                .edges
                .is_empty()
        );
    }
    let count: u32 = probe.query_row(
        "SELECT count(*) FROM sqlite_schema WHERE name='managed_core_generation'",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(count, 0);
    Ok(())
}
