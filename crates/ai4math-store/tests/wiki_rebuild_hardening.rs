use ai4math_store::{SedimentInput, Wiki, WikiQuery, publish_sediment, render_sediment};
use anyhow::Result;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    ffi::CString,
    fs::{self, File},
    io::Write,
    os::unix::{
        ffi::OsStrExt,
        fs::{PermissionsExt, symlink},
    },
    path::{Path, PathBuf},
};

const MIB: u64 = 1024 * 1024;

fn core(id: &str) -> String {
    format!(
        "---\nid: {id}\nstratum: core\ntrust_level: L5\nengine: lean4\nstatus: active\n---\n# Example {id}\n"
    )
}

fn fixture(root: &Path) -> Result<(PathBuf, Wiki)> {
    let directory = root.join("core");
    fs::create_dir(&directory)?;
    fs::write(directory.join("old.md"), core("old"))?;
    let mut wiki = Wiki::open(root.join("index.sqlite"))?;
    assert_eq!(wiki.rebuild(root)?, 1);
    Ok((directory, wiki))
}

fn assert_refused(wiki: &mut Wiki, root: &Path, expected: &str) -> Result<()> {
    let error = wiki.rebuild(root).unwrap_err();
    assert!(format!("{error:#}").contains(expected), "{error:#}");
    let result = wiki.search(WikiQuery::default())?;
    assert_eq!(result.hits.len(), 1);
    assert_eq!(result.hits[0].summary.id, "old");
    assert_eq!(wiki.search(WikiQuery::new("Example old"))?.hits.len(), 1);
    Ok(())
}

#[test]
fn over_limit_markdown_files_and_ignored_entries_preserve_old_index() -> Result<()> {
    let root = tempfile::tempdir()?;
    let (directory, mut wiki) = fixture(root.path())?;
    let many = directory.join("many");
    fs::create_dir(&many)?;
    for i in 0..4096 {
        fs::write(many.join(format!("{i:04}.MD")), core("new"))?;
    }
    assert_refused(&mut wiki, root.path(), "4096-Markdown-file limit")?;
    fs::rename(&many, root.path().join("not-scanned"))?;
    fs::create_dir(&many)?;
    for i in 0..16_384 {
        File::create(many.join(format!("{i:05}.ignored")))?;
    }
    assert_refused(&mut wiki, root.path(), "16384-directory-entry limit")?;
    Ok(())
}

#[test]
fn oversized_file_and_combined_markdown_preserve_old_index() -> Result<()> {
    let root = tempfile::tempdir()?;
    let (directory, mut wiki) = fixture(root.path())?;
    let oversized = directory.join("oversize.md");
    File::create(&oversized)?.set_len(8 * MIB + 1)?;
    assert_refused(&mut wiki, root.path(), "8388608-byte file limit")?;
    fs::remove_file(oversized)?;

    // Sparse files with valid frontmatter hit the total limit without any one
    // path exceeding its own allowance.
    for i in 0..8 {
        let mut file = File::create(directory.join(format!("z-{i}.md")))?;
        file.write_all(core(&format!("new-{i}")).as_bytes())?;
        file.set_len(8 * MIB)?;
    }
    assert_refused(&mut wiki, root.path(), "67108864-byte total limit")?;
    Ok(())
}

#[test]
fn symlink_hard_link_fifo_and_shared_write_preserve_old_index() -> Result<()> {
    let scratch = tempfile::tempdir()?;
    let root = scratch.path().join("wiki");
    fs::create_dir(&root)?;
    let (directory, mut wiki) = fixture(&root)?;
    let original = directory.join("old.md");
    let alias = directory.join("alias.md");
    symlink(&original, &alias)?;
    assert_refused(&mut wiki, &root, "unsafe file type")?;
    fs::remove_file(&alias)?;
    fs::hard_link(&original, &alias)?;
    assert_refused(&mut wiki, &root, "exactly one hard link")?;
    fs::remove_file(&alias)?;

    let name = CString::new(alias.as_os_str().as_bytes())?;
    // SAFETY: name is a valid NUL-terminated path; mode is a normal file mode.
    if unsafe { libc::mkfifo(name.as_ptr(), 0o600) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    assert_refused(&mut wiki, &root, "unsafe file type")?;
    fs::remove_file(&alias)?;

    fs::write(&alias, core("replacement"))?;
    fs::set_permissions(&alias, fs::Permissions::from_mode(0o664))?;
    assert_refused(&mut wiki, &root, "not writable by group or others")?;
    fs::remove_file(&alias)?;
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o775))?;
    assert_refused(&mut wiki, &root, "not writable by group or others")?;
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o755))?;

    let moved = scratch.path().join("detached-core");
    fs::rename(&directory, &moved)?;
    symlink(&moved, &directory)?;
    assert_refused(&mut wiki, &root, "unsafe file type")?;
    fs::remove_file(&directory)?;
    fs::rename(&moved, &directory)?;
    symlink(&directory, directory.join("nested"))?;
    assert_refused(&mut wiki, &root, "entry is a symlink")?;
    Ok(())
}

#[test]
fn preparation_needs_no_sqlite_writer_and_publish_uses_only_staged_data() -> Result<()> {
    let root = tempfile::tempdir()?;
    let (directory, mut wiki) = fixture(root.path())?;
    let db = rusqlite::Connection::open(root.path().join("index.sqlite"))?;
    db.execute_batch("BEGIN IMMEDIATE")?;
    let source = directory.join("old.md");
    fs::write(&source, core("prepared"))?;
    let prepared = Wiki::prepare_rebuild(root.path())?;
    assert_eq!(wiki.search(WikiQuery::default())?.hits[0].summary.id, "old");
    db.execute_batch("ROLLBACK")?;

    // The prepare/publish API intentionally does no filesystem access at
    // publication. In particular, it cannot claim a filesystem snapshot.
    fs::remove_file(source)?;
    assert_eq!(wiki.publish_rebuild(prepared)?, 1);
    assert_eq!(
        wiki.search(WikiQuery::default())?.hits[0].summary.id,
        "prepared"
    );
    Ok(())
}

#[test]
fn rebuild_accepts_large_canonical_sediment_archive() -> Result<()> {
    let project = tempfile::tempdir()?;
    // A legal 2 MiB source may require two longer fences, producing a >6 MiB
    // archive. A generic small Markdown cap must not reject this output.
    let source = "~".repeat(2 * MIB as usize);
    let artifact = render_sediment(SedimentInput {
        claim_id: "archive-source",
        engine: "lean4",
        verified_at_commit: &"a".repeat(40),
        source_module: "Math.Archive",
        target_decl: "example",
        source: &source,
        source_sha256: &format!("{:x}", Sha256::digest(source.as_bytes())),
        dependency_hashes: &BTreeMap::new(),
        dependency_order: &[],
    })?;
    assert!(artifact.markdown().len() > 6 * MIB as usize);
    publish_sediment(project.path(), &artifact)?;
    let mut wiki = Wiki::open(project.path().join("index.sqlite"))?;
    assert_eq!(wiki.rebuild(project.path().join(".mathwiki"))?, 1);
    Ok(())
}
