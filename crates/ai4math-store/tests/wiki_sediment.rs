use ai4math_store::{
    SedimentInput, Trust, Wiki, WikiQuery, WikiStratum, publish_sediment, render_sediment,
};
use anyhow::Result;
use serde_yaml::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, Permissions},
    os::unix::fs::{MetadataExt, PermissionsExt, symlink},
    path::Path,
    sync::Barrier,
};

const SOURCE: &str = "import Dependencies.First\r\n\r\n/-\r\n~~~~~\r\n## Informal Statement\r\nprivateSedimentMarker\r\n---\r\n-/\r\ntheorem ArchiveSample : True := by trivial  ";
const COMMIT: &str = "4ea7ac483f4ffb41559e8711fa917f8853fa6378";

fn sha(s: &str) -> String {
    format!("{:x}", Sha256::digest(s.as_bytes()))
}

fn input<'a>(source_sha256: &'a str) -> SedimentInput<'a> {
    static EMPTY: BTreeMap<String, String> = BTreeMap::new();
    SedimentInput {
        claim_id: "CLM-200",
        engine: "lean4",
        verified_at_commit: COMMIT,
        source_module: "Historical.ArchiveSample",
        target_decl: "ArchiveSample",
        source: SOURCE,
        source_sha256,
        dependency_hashes: &EMPTY,
        dependency_order: &[],
    }
}

fn dirs(root: &Path) -> Result<()> {
    fs::create_dir_all(root.join(".mathwiki/sediment"))?;
    Ok(())
}

#[test]
fn deterministic_provenance_binds_every_input_and_preserves_exact_source() -> Result<()> {
    let source_sha = sha(SOURCE);
    let dep1 = "Dependencies.First".to_owned();
    let dep2 = "Dependencies.第二".to_owned();
    let hashes = BTreeMap::from([(dep1.clone(), sha("first")), (dep2.clone(), sha("second"))]);
    let order = [dep2.clone(), dep1.clone()];
    let sample = SedimentInput {
        dependency_hashes: &hashes,
        dependency_order: &order,
        ..input(&source_sha)
    };
    let artifact = render_sediment(sample)?;
    assert_eq!(artifact, render_sediment(sample)?);
    let id_digest = artifact.id().strip_prefix("SED-").unwrap();
    assert_eq!(id_digest.len(), 64);
    assert!(
        id_digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    );
    let (header, body) = artifact
        .markdown()
        .strip_prefix("---\n")
        .unwrap()
        .split_once("\n---\n")
        .unwrap();
    let fm: Value = serde_yaml::from_str(header)?;
    assert_eq!(fm["id"], artifact.id());
    assert_eq!(fm["stratum"], "sediment");
    assert_eq!(fm["trust_level"], "L5");
    assert_eq!(fm["engine"], "lean4");
    assert_eq!(fm["status"], "unreferenced");
    assert_eq!(fm["verified_at_commit"], COMMIT);
    assert_eq!(fm["origin_claim"], sample.claim_id);
    assert_eq!(fm["source_module"], sample.source_module);
    assert_eq!(fm["target_decl"], sample.target_decl);
    assert_eq!(fm["source_sha256"], source_sha.as_str());
    assert_eq!(fm["source_bytes"].as_u64(), Some(SOURCE.len() as u64));
    assert_eq!(fm["def_fingerprint_kind"], "module_bytes_sha256");
    assert_eq!(
        fm["def_fingerprints"][dep1.as_str()],
        format!("sha256:{}", hashes[&dep1])
    );
    assert_eq!(
        fm["def_fingerprints"][dep2.as_str()],
        format!("sha256:{}", hashes[&dep2])
    );
    assert_eq!(fm["dependency_order"], serde_yaml::to_value(&order)?);
    assert!(fm["formal_signature"].is_null());
    let (fence, payload) = body
        .split_once("\n## Formal Artifact\n\n")
        .unwrap()
        .1
        .split_once('\n')
        .unwrap();
    assert_eq!(fence, "~~~~~~");
    assert_eq!(&payload.as_bytes()[..SOURCE.len()], SOURCE.as_bytes());
    assert_eq!(&payload[SOURCE.len()..], format!("\n{fence}\n"));

    let reversed = [dep1.clone(), dep2.clone()];
    let altered_hashes =
        BTreeMap::from([(dep1.clone(), sha("other")), (dep2.clone(), sha("second"))]);
    let added_line_end = format!("{SOURCE}\n");
    let altered_source_sha = sha(&added_line_end);
    for altered in [
        SedimentInput {
            claim_id: "CLM-201",
            ..sample
        },
        SedimentInput {
            engine: "rocq",
            ..sample
        },
        SedimentInput {
            verified_at_commit: "other",
            ..sample
        },
        SedimentInput {
            source_module: "Another",
            ..sample
        },
        SedimentInput {
            target_decl: "OtherDecl",
            ..sample
        },
        SedimentInput {
            source: &added_line_end,
            source_sha256: &altered_source_sha,
            ..sample
        },
        SedimentInput {
            dependency_hashes: &altered_hashes,
            ..sample
        },
        SedimentInput {
            dependency_order: &reversed,
            ..sample
        },
    ] {
        assert_ne!(render_sediment(altered)?.id(), artifact.id());
    }
    for invalid in [
        SedimentInput {
            source_sha256: &altered_source_sha,
            ..sample
        },
        SedimentInput {
            engine: "lean4\nstratum: core",
            ..sample
        },
        SedimentInput {
            dependency_order: &[],
            ..sample
        },
        SedimentInput {
            dependency_order: &[dep1.clone(), dep2, dep1],
            ..sample
        },
    ] {
        assert!(render_sediment(invalid).is_err());
    }
    Ok(())
}

#[test]
fn publish_is_idempotent_and_rebuild_excludes_sediment_from_core_hits() -> Result<()> {
    let root = tempfile::tempdir()?;
    let wiki_path = root.path().join(".mathwiki");
    fs::create_dir(&wiki_path)?;
    let mut wiki = Wiki::open(wiki_path.join(".index.sqlite"))?;
    let artifact = render_sediment(input(&sha(SOURCE)))?;
    let file = root.path().join(artifact.relative_path());
    assert!(publish_sediment(root.path(), &artifact)?);
    assert!(!publish_sediment(root.path(), &artifact)?);
    assert_eq!(fs::read(&file)?, artifact.markdown().as_bytes());
    assert_eq!(fs::metadata(&file)?.mode() & 0o777, 0o600);
    assert_eq!(fs::metadata(&file)?.nlink(), 1);
    assert_eq!(fs::read_dir(file.parent().unwrap())?.count(), 1);
    let cold_query = WikiQuery {
        scope: WikiStratum::Sediment,
        ..WikiQuery::new("ArchiveSample")
    };
    assert!(wiki.search(cold_query)?.hits.is_empty());
    assert_eq!(wiki.rebuild(&wiki_path)?, 1);
    let core = wiki.search(WikiQuery::new("ArchiveSample"))?;
    assert!(core.hits.is_empty());
    assert_eq!(
        core.shadow_hint.unwrap().top_sediment.unwrap().id,
        artifact.id()
    );
    let cold = wiki.search(cold_query)?;
    let [hit] = cold.hits.as_slice() else {
        panic!("expected exactly one cold match")
    };
    assert_eq!(hit.summary.id, artifact.id());
    assert_eq!(hit.summary.verified_at_commit.as_deref(), Some(COMMIT));
    assert_eq!(hit.stratum, WikiStratum::Sediment);
    assert_eq!(hit.trust_level, Trust::L5);
    assert_eq!(hit.status, "unreferenced");
    assert!(hit.formal_signature.is_none());
    assert!(!hit.informal_gloss.contains("privateSedimentMarker"));
    let provenance = wiki.provenance(artifact.id(), 1, 10)?;
    let [edge] = provenance.edges.as_slice() else {
        panic!("expected one origin edge")
    };
    assert_eq!(edge.src_id, artifact.id());
    assert_eq!(edge.dst_id, "CLM-200");
    assert_eq!(edge.edge_type, "born_from_claim");
    assert_eq!(edge.dst_stratum, None);
    assert!(edge.metadata.is_empty());
    Ok(())
}

#[test]
fn path_and_frontmatter_strings_are_data_only() -> Result<()> {
    let root = tempfile::tempdir()?;
    let attack = "../../escape.md\n---\nstratum: core\ntrust_level: L1\r\n\"quoted\"\u{0085}\u{2028}\u{2029}\u{feff}";
    let dep = format!("{attack}\u{007f}");
    let dependency_hashes = BTreeMap::from([(dep.clone(), sha("dependency"))]);
    let source_sha = sha(SOURCE);
    let sample = SedimentInput {
        claim_id: attack,
        verified_at_commit: attack,
        source_module: attack,
        target_decl: attack,
        dependency_hashes: &dependency_hashes,
        dependency_order: std::slice::from_ref(&dep),
        ..input(&source_sha)
    };
    let artifact = render_sediment(sample)?;
    assert_eq!(
        artifact.relative_path(),
        Path::new(".mathwiki/sediment").join(format!("{}.md", artifact.id()))
    );
    assert!(publish_sediment(root.path(), &artifact)?);
    assert_eq!(fs::read_dir(root.path())?.count(), 1);
    let (header, _) = artifact
        .markdown()
        .strip_prefix("---\n")
        .unwrap()
        .split_once("\n---\n")
        .unwrap();
    let fm: Value = serde_yaml::from_str(header)?;
    for field in [
        "origin_claim",
        "verified_at_commit",
        "source_module",
        "target_decl",
    ] {
        assert_eq!(fm[field], attack);
    }
    assert_eq!(
        fm["def_fingerprints"][dep.as_str()],
        format!("sha256:{}", dependency_hashes[&dep])
    );
    assert_eq!(fm["stratum"], "sediment");
    assert_eq!(fm["trust_level"], "L5");
    let mut wiki = Wiki::open(":memory:")?;
    assert_eq!(wiki.rebuild(root.path().join(".mathwiki"))?, 1);
    assert!(wiki.search(WikiQuery::new("escape"))?.hits.is_empty());
    Ok(())
}

#[test]
fn directory_and_output_aliases_and_shared_paths_are_refused() -> Result<()> {
    let artifact = render_sediment(input(&sha(SOURCE)))?;
    let outside = tempfile::tempdir()?;
    let bytes = artifact.markdown().as_bytes();
    let outside_file = outside.path().join("untouched");
    fs::write(&outside_file, bytes)?;

    for path in [".mathwiki", ".mathwiki/sediment"] {
        let root = tempfile::tempdir()?;
        let dir = root.path().join(path);
        fs::create_dir_all(dir.parent().unwrap())?;
        symlink(outside.path(), &dir)?;
        assert!(publish_sediment(root.path(), &artifact).is_err());
        assert!(fs::symlink_metadata(&dir)?.is_symlink());
        fs::remove_file(&dir)?;
        fs::create_dir(&dir)?;
        fs::set_permissions(&dir, Permissions::from_mode(0o775))?;
        assert!(publish_sediment(root.path(), &artifact).is_err());
    }

    let root = tempfile::tempdir()?;
    dirs(root.path())?;
    let dest = root.path().join(artifact.relative_path());
    symlink(&outside_file, &dest)?;
    assert!(publish_sediment(root.path(), &artifact).is_err());
    assert!(fs::symlink_metadata(&dest)?.is_symlink());
    fs::remove_file(&dest)?;
    fs::hard_link(&outside_file, &dest)?;
    assert!(publish_sediment(root.path(), &artifact).is_err());
    assert_eq!(fs::metadata(&dest)?.nlink(), 2);
    fs::remove_file(&dest)?;
    fs::write(&dest, bytes)?;
    fs::set_permissions(&dest, Permissions::from_mode(0o662))?;
    assert!(publish_sediment(root.path(), &artifact).is_err());
    assert_eq!(fs::read(&outside_file)?, bytes);
    assert_eq!(fs::read_dir(outside.path())?.count(), 1);
    Ok(())
}

#[test]
fn stale_existing_bytes_are_never_replaced() -> Result<()> {
    let root = tempfile::tempdir()?;
    dirs(root.path())?;
    let artifact = render_sediment(input(&sha(SOURCE)))?;
    let dest = root.path().join(artifact.relative_path());
    let bytes = vec![b'x'; artifact.markdown().len()];
    fs::write(&dest, &bytes)?;
    assert!(publish_sediment(root.path(), &artifact).is_err());
    assert_eq!(fs::read(&dest)?, bytes);
    assert_eq!(fs::read_dir(dest.parent().unwrap())?.count(), 1);
    Ok(())
}

#[test]
fn simultaneous_publications_install_once() -> Result<()> {
    let project = tempfile::tempdir()?;
    let artifact = render_sediment(input(&sha(SOURCE)))?;
    let barrier = Barrier::new(8);
    std::thread::scope(|scope| -> Result<()> {
        let tasks: Vec<_> = (0..8)
            .map(|_| {
                scope.spawn(|| {
                    barrier.wait();
                    publish_sediment(project.path(), &artifact)
                })
            })
            .collect();
        let mut added = 0;
        for task in tasks {
            added += usize::from(task.join().unwrap()?);
        }
        assert_eq!(added, 1);
        Ok(())
    })?;
    let file = project.path().join(artifact.relative_path());
    assert_eq!(fs::read(&file)?, artifact.markdown().as_bytes());
    assert_eq!(fs::read_dir(file.parent().unwrap())?.count(), 1);
    Ok(())
}
