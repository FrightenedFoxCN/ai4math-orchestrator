use ai4math_store::{ClaimEngine, Wiki, WikiQuery, WikiStratum};
use anyhow::Result;
use rusqlite::Connection;
use std::{fs, path::Path};

fn entry(root: &Path, folder: &str, file: &str, source: &str) -> Result<()> {
    let folder = root.join(folder);
    fs::create_dir_all(&folder)?;
    fs::write(folder.join(file), source)?;
    Ok(())
}

const CORE: &str = "---
id: COR-1
stratum: core
trust_level: L5
engine: lean4
status: active
---
# Haar convolution
## Informal Statement
Convolution over Haar measure.
";

const SEDIMENT: &str = r#"---
id: SED-89
stratum: sediment
trust_level: L5
engine: lean4
status: unreferenced
verified_at_commit: a1b2c3d
origin_claim: CLM-072
origin_branch: exp/haar
def_fingerprints:
  Infra.Haar: sha256:aabb
symbols:
  conclusion_head: LE.le
  hypothesis_heads: [Integrable, IsCompact, IsCompact]
  typeclasses: [MeasureSpace]
  constants: [plancherelMap]
tags: [measure-theory, haar-measure]
edges:
  - dst_id: COR-1
    edge_type: generalizes
    metadata: {confidence: high}
---
# Haar convolution L2 bound

## Informal Statement
Haar convolution on a locally compact group. sedimentsecret

## Formal Artifact
~~~lean
lemma haar_convolution ... := by ...
## Informal Statement
fake heading in a fence
~~~

## Provenance & Notes
archivalkeyword
"#;

const GRAVEYARD: &str = "---
id: GRV-14
stratum: graveyard
trust_level: L2b
engine: agda
status: refuted
title: Incorrect convolution
informal_gloss: Haar convolution counterexample. graveyardsecret
formal_signature: originalClaim
---
";

#[test]
fn default_search_teases_cold_metadata_without_body_and_respects_filters() -> Result<()> {
    let root = tempfile::tempdir()?;
    let root = root.path();
    entry(root, "core", "first.md", CORE)?;
    entry(root, "sediment/topic", "retired.md", SEDIMENT)?;
    entry(root, "graveyard", "refuted.md", GRAVEYARD)?;
    let mut wiki = Wiki::open(root.join(".index.sqlite"))?;
    assert_eq!(wiki.rebuild(root)?, 3);

    let query = WikiQuery::new("Haar convolution");
    let result = wiki.search(query)?;
    assert_eq!(result.hits.len(), 1);
    assert_eq!(result.hits[0].summary.id, "COR-1");
    assert_eq!(
        result.hits[0].informal_gloss,
        "Convolution over Haar measure."
    );
    let hint = result.shadow_hint.unwrap();
    assert_eq!(hint.sediment_count, 1);
    let top = hint.top_sediment.as_ref().unwrap();
    assert_eq!(top.id, "SED-89");
    assert_eq!(top.verified_at_commit.as_deref(), Some("a1b2c3d"));
    assert!(top.score > 0.82);
    assert_eq!(hint.graveyard_count, 1);
    assert_eq!(hint.top_graveyard_id.as_deref(), Some("GRV-14"));
    let compact = serde_json::to_string(&hint)?;
    for excluded in [
        "informal_gloss",
        "formal_signature",
        "file_path",
        "sedimentsecret",
        "graveyardsecret",
        "archivalkeyword",
    ] {
        assert!(!compact.contains(excluded));
    }
    assert!(
        wiki.search(WikiQuery {
            include_shadow_hint: false,
            ..query
        })?
        .shadow_hint
        .is_none()
    );
    assert_eq!(
        wiki.search(WikiQuery {
            engine: Some(ClaimEngine::Lean4),
            ..query
        })?
        .shadow_hint
        .unwrap()
        .graveyard_count,
        0
    );
    assert!(
        wiki.search(WikiQuery::new("archivalkeyword"))?
            .shadow_hint
            .is_none()
    );
    assert!(
        wiki.search(WikiQuery::new("\" : *"))
            .unwrap()
            .hits
            .is_empty()
    );
    assert!(wiki.search(WikiQuery::default())?.shadow_hint.is_none());
    let symbolic = wiki.search(WikiQuery {
        conclusion_head: Some("LE.le"),
        ..WikiQuery::default()
    })?;
    assert!(symbolic.hits.is_empty());
    assert_eq!(symbolic.shadow_hint.unwrap().sediment_count, 1);

    let sediment = wiki.search(WikiQuery {
        scope: WikiStratum::Sediment,
        conclusion_head: Some("LE.le"),
        hypothesis_heads: &["IsCompact", "Integrable"],
        typeclasses: &["MeasureSpace"],
        ..WikiQuery::new("unrelated lexical text")
    })?;
    assert_eq!(sediment.hits.len(), 1);
    let hit = &sediment.hits[0];
    assert_eq!(hit.file_path, "sediment/topic/retired.md");
    assert!(hit.informal_gloss.contains("sedimentsecret"));
    assert!(!hit.informal_gloss.contains("fake heading"));
    assert!(sediment.shadow_hint.is_none());
    let backward = wiki.search(WikiQuery {
        scope: WikiStratum::Sediment,
        hypothesis_heads: &["LE.le"],
        ..query
    })?;
    assert!(backward.hits.is_empty());
    let graveyard = wiki.search(WikiQuery {
        scope: WikiStratum::Graveyard,
        ..query
    })?;
    assert_eq!(
        graveyard.hits[0].formal_signature.as_deref(),
        Some("originalClaim")
    );
    assert!(graveyard.shadow_hint.is_none());

    entry(root, "core", "second.md", &CORE.replace("COR-1", "COR-2"))?;
    assert_eq!(wiki.rebuild(root)?, 4);
    let result = wiki.search(WikiQuery { limit: 1, ..query })?;
    assert_eq!(result.hits.len(), 1);
    let hint = result.shadow_hint.unwrap();
    assert_eq!(
        hint.sediment_count, 0,
        "count CORE hits before display truncation"
    );
    assert!(hint.top_sediment.is_none());
    assert_eq!(
        hint.graveyard_count, 1,
        "refutation warnings bypass the CORE gate"
    );
    Ok(())
}

#[test]
fn rebuild_is_atomic_and_removes_old_fts_symbols_and_edges() -> Result<()> {
    let root = tempfile::tempdir()?;
    let root = root.path();
    let db_path = root.join(".index.sqlite");
    let mut wiki = Wiki::open(&db_path)?;
    entry(
        root,
        "sediment",
        "retired.md",
        &format!("\u{feff}{}", SEDIMENT.replace('\n', "\r\n")),
    )?;
    assert_eq!(wiki.rebuild(root)?, 1);
    let read = Connection::open(&db_path)?;
    let count = |table: &str| -> rusqlite::Result<i64> {
        read.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
    };
    assert_eq!(count("entries")?, 1);
    assert_eq!(count("entries_fts")?, 1);
    assert_eq!(count("entry_symbols")?, 5);
    assert_eq!(count("entry_edges")?, 2);
    let metadata: String = read.query_row(
        "SELECT metadata_json FROM entry_edges WHERE edge_type='generalizes'",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&metadata)?["confidence"],
        "high"
    );
    let stored: (bool, String, String) = read.query_row(
        "SELECT pinned,origin_branch,def_fingerprints_json FROM entries",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    assert!(!stored.0);
    assert_eq!(stored.1, "exp/haar");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&stored.2)?["Infra.Haar"],
        "sha256:aabb"
    );

    entry(root, "core", "first.md", CORE)?;
    entry(root, "sediment", "z-duplicate.md", SEDIMENT)?;
    let err = wiki.rebuild(root).unwrap_err();
    assert!(err.to_string().contains("z-duplicate.md"));
    assert!(
        wiki.search(WikiQuery::new("Haar convolution"))?
            .hits
            .is_empty()
    );
    assert_eq!(count("entries_fts")?, 1);
    assert_eq!(count("entry_symbols")?, 5);
    assert_eq!(count("entry_edges")?, 2);

    entry(root, "sediment", "z-duplicate.md", CORE)?;
    let err = wiki.rebuild(root).unwrap_err();
    assert!(format!("{err:#}").contains("stratum does not match"));
    fs::remove_file(root.join("sediment/z-duplicate.md"))?;
    fs::remove_file(root.join("sediment/retired.md"))?;
    assert_eq!(wiki.rebuild(root)?, 1);
    assert_eq!(count("entries_fts")?, 1);
    assert_eq!(count("entry_symbols")?, 0);
    assert_eq!(count("entry_edges")?, 0);
    assert!(
        wiki.search(WikiQuery::new("Haar convolution"))?
            .shadow_hint
            .is_none()
    );
    assert_eq!(fs::read_to_string(root.join("core/first.md"))?, CORE);
    Ok(())
}
