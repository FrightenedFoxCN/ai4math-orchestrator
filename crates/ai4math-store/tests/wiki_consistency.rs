use ai4math_store::{Wiki, WikiQuery, WikiSearchResult, WikiStratum};
use anyhow::Result;
use rusqlite::Connection;
use std::{fs, path::Path};

fn entry(
    root: &Path,
    stratum: WikiStratum,
    id: &str,
    trust: &str,
    status: &str,
    extra: &str,
) -> Result<()> {
    let dir = root.join(stratum.as_str());
    fs::create_dir_all(&dir)?;
    fs::write(
        dir.join(format!("{id}.md")),
        format!(
            "---
id: {id}
stratum: {}
trust_level: {trust}
engine: lean4
status: {status}
title: Convolution lemma
symbols:
  conclusion_head: LE.le
{extra}---
",
            stratum.as_str(),
        ),
    )?;
    Ok(())
}

fn ids(result: &WikiSearchResult) -> Vec<&str> {
    result
        .hits
        .iter()
        .map(|hit| hit.summary.id.as_str())
        .collect()
}

#[test]
fn rejects_invalid_strata_without_publishing_a_partial_rebuild() -> Result<()> {
    use WikiStratum::{Core, Graveyard, Sediment};
    let root = tempfile::tempdir()?;
    let root = root.path();
    entry(root, Core, "COR-live", "L5", "active", "")?;
    entry(root, Sediment, "SED-previous", "L5", "superseded", "")?;
    entry(root, Graveyard, "GRV-witness", "L2b", "refuted", "")?;
    let mut wiki = Wiki::open(root.join(".index.sqlite"))?;
    assert_eq!(wiki.rebuild(root)?, 3);

    // Even a pin cannot make a refuted or superseded Core entry reusable.
    for (scope, trust, status) in [
        (Core, "L2b", "active"),
        (Core, "L5", "refuted"),
        (Core, "L5", "superseded"),
        (Sediment, "L2b", "unreferenced"),
        (Sediment, "L5", "refuted"),
    ] {
        entry(root, scope, "invalid", trust, status, "pinned: true\n")?;
        let err = format!("{:#}", wiki.rebuild(root).unwrap_err());
        assert!(err.contains("invalid.md"), "{err}");
        assert!(err.contains("not valid in stratum"), "{err}");
        let published = wiki.search(WikiQuery::new("Convolution"))?;
        assert_eq!(ids(&published), ["COR-live"]);
        let hint = published.shadow_hint.unwrap();
        assert_eq!(hint.sediment_count, 1);
        assert_eq!(hint.graveyard_count, 1);
        fs::remove_file(root.join(scope.as_str()).join("invalid.md"))?;
    }
    Ok(())
}

#[test]
fn legacy_invalid_rows_cannot_win_or_suppress_shadow_hints() -> Result<()> {
    use WikiStratum::{Core, Graveyard, Sediment};
    let root = tempfile::tempdir()?;
    let root = root.path();
    let db_path = root.join(".index.sqlite");
    for id in ["COR-a", "COR-b", "COR-c", "COR-live"] {
        entry(root, Core, id, "L5", "unreferenced", "pinned: true\n")?;
    }
    for id in ["SED-a", "SED-b", "SED-previous"] {
        entry(root, Sediment, id, "L5", "superseded", "")?;
    }
    entry(root, Graveyard, "GRV-design", "L1", "superseded", "")?;
    entry(root, Graveyard, "GRV-witness", "L2b", "refuted", "")?;
    let mut wiki = Wiki::open(&db_path)?;
    assert_eq!(wiki.rebuild(root)?, 9);

    // Simulate rows accepted by a previous index version. No illegal SQL values are needed.
    Connection::open(&db_path)?.execute_batch(
        "UPDATE entries SET trust_level='L2b' WHERE id IN ('COR-a','SED-a');
         UPDATE entries SET status='refuted' WHERE id IN ('COR-b','SED-b');
         UPDATE entries SET status='superseded' WHERE id='COR-c';",
    )?;
    let query = WikiQuery {
        limit: 1,
        ..WikiQuery::new("Convolution")
    };
    let result = wiki.search(query)?;
    assert_eq!(ids(&result), ["COR-live"]);
    let hint = result.shadow_hint.unwrap();
    assert_eq!(hint.sediment_count, 1);
    assert_eq!(hint.top_sediment.unwrap().id, "SED-previous");
    assert_eq!(hint.graveyard_count, 1);
    assert_eq!(hint.top_graveyard_id.as_deref(), Some("GRV-witness"));

    assert_eq!(ids(&wiki.search(WikiQuery::default())?), ["COR-live"]);
    assert_eq!(
        ids(&wiki.search(WikiQuery {
            scope: Sediment,
            conclusion_head: Some("LE.le"),
            ..WikiQuery::default()
        })?),
        ["SED-previous"]
    );
    let graveyard = wiki.search(WikiQuery {
        scope: Graveyard,
        limit: 20,
        ..query
    })?;
    assert_eq!(ids(&graveyard), ["GRV-design", "GRV-witness"]);
    assert!(graveyard.shadow_hint.is_none());
    Ok(())
}

#[test]
fn provenance_preserves_edges_across_strata_and_bounds_cycle_safe_traversal() -> Result<()> {
    use WikiStratum::{Core, Graveyard, Sediment};
    let root = tempfile::tempdir()?;
    let root = root.path();
    entry(
        root,
        Core,
        "COR-A",
        "L5",
        "active",
        "edges:
  - {dst_id: COR-B, edge_type: depends_on}
",
    )?;
    entry(
        root,
        Core,
        "COR-B",
        "L5",
        "active",
        "edges:
  - {dst_id: COR-A, edge_type: used_by}
",
    )?;
    entry(
        root,
        Graveyard,
        "GRV-A",
        "L2b",
        "refuted",
        "edges:
  - {dst_id: COR-A, edge_type: refutes}
",
    )?;
    entry(
        root,
        Sediment,
        "SED-A",
        "L5",
        "superseded",
        "origin_claim: CLM-42
edges:
  - dst_id: COR-B
    edge_type: superseded_by
    metadata: {reason: changed-upstream}
  - {dst_id: missing, edge_type: depends_on}
",
    )?;
    let mut wiki = Wiki::open(root.join(".index.sqlite"))?;
    assert_eq!(wiki.rebuild(root)?, 4);

    let result = wiki.provenance("COR-A", 4, 10)?;
    let edges: Vec<_> = result
        .edges
        .iter()
        .map(|e| {
            (
                e.src_id.as_str(),
                e.edge_type.as_str(),
                e.dst_id.as_str(),
                e.depth,
            )
        })
        .collect();
    assert_eq!(
        edges,
        [
            ("COR-A", "depends_on", "COR-B", 1),
            ("COR-B", "used_by", "COR-A", 1),
            ("GRV-A", "refutes", "COR-A", 1),
            ("SED-A", "superseded_by", "COR-B", 2),
            ("SED-A", "born_from_claim", "CLM-42", 3),
            ("SED-A", "depends_on", "missing", 3),
        ]
    );
    assert!(!result.truncated);
    assert_eq!(result.edges[2].src_stratum, Graveyard);
    assert_eq!(result.edges[3].src_stratum, Sediment);
    assert_eq!(result.edges[3].dst_stratum, Some(Core));
    assert_eq!(result.edges[3].metadata["reason"], "changed-upstream");
    assert_eq!(result.edges[4].dst_stratum, None);
    assert_eq!(result.edges[5].dst_stratum, None);

    let one_hop = wiki.provenance("COR-A", 1, 3)?;
    assert_eq!(one_hop.edges.len(), 3);
    assert!(!one_hop.truncated);
    let capped = wiki.provenance("COR-A", 2, 3)?;
    assert_eq!(capped.edges.len(), 3);
    assert!(capped.truncated);
    assert!(wiki.provenance("COR-A", 0, 3)?.edges.is_empty());
    assert!(wiki.provenance("unknown", 1, 3)?.edges.is_empty());
    let from_claim = wiki.provenance("CLM-42", 1, 3)?;
    assert_eq!(from_claim.edges.len(), 1);
    assert_eq!(from_claim.edges[0].src_id, "SED-A");
    assert_eq!(from_claim.edges[0].edge_type, "born_from_claim");
    assert_eq!(from_claim.edges[0].depth, 1);
    assert!(wiki.provenance("COR-A", 17, 3).is_err());
    assert!(wiki.provenance("COR-A", 1, 257).is_err());
    Ok(())
}
