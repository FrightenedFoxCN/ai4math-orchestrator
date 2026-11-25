//! Broker-owned Lean object pool with a fresh, writable scratch for each compiler invocation.
use super::{Engine, path_for, stage_source};
use anyhow::{Context, Result, ensure};
use std::{
    fs,
    io::{self, ErrorKind, Read},
    path::{Path, PathBuf},
};

const PART_SUFFIXES: &[&str] = &["olean", "olean.server", "olean.private", "ir.sig", "ir"];
pub(super) const MAX_PARTS_PER_MODULE: u64 = PART_SUFFIXES.len() as u64;

pub(super) struct LeanObjects {
    path: PathBuf,
    artifact_cap: u64,
    remaining_bytes: u64,
}

impl LeanObjects {
    pub(super) fn create(path: &Path, artifact_cap: u64, remaining_bytes: u64) -> Result<Self> {
        fs::create_dir(path)?;
        Ok(Self {
            path: path.canonicalize()?,
            artifact_cap,
            remaining_bytes,
        })
    }

    /// The compiler sees only its own source and the committed predecessor data.
    /// Keep the pool outside the per-module room and its sole writable subtree.
    pub(super) fn compile(
        &mut self,
        relative: &str,
        source: &str,
        save_object: bool,
        check: impl FnOnce(&Path, &Path, &PathBuf) -> Result<String>,
    ) -> Result<String> {
        let module = path_for(Engine::Lean4, relative)?;
        let parent = self
            .path
            .parent()
            .context("Lean object pool has no parent")?;
        let scratch = tempfile::Builder::new()
            .prefix("build-")
            .tempdir_in(parent)?;
        let writable = scratch.path().join("scratch");
        fs::create_dir(&writable)?;
        stage_source(Engine::Lean4, scratch.path(), &writable, relative, source)?;
        let log = check(scratch.path(), &writable, &self.path)?;
        if save_object {
            commit_object_parts(
                &writable,
                &self.path,
                module,
                self.artifact_cap,
                &mut self.remaining_bytes,
            )?;
        }
        scratch.close()?;
        Ok(log)
    }
}

/// Only broker-owned files for the current module can enter the pool. Ignore any
/// scratch output not recognized as an object part; never replace a committed part.
fn commit_object_parts(
    scratch: &Path,
    objects: &Path,
    module: &str,
    artifact_cap: u64,
    remaining_bytes: &mut u64,
) -> Result<()> {
    ensure!(
        fs::symlink_metadata(scratch)?.is_dir(),
        "invalid Lean output directory"
    );
    for &ext in PART_SUFFIXES {
        let name = format!("{module}.{ext}");
        let path = scratch.join(&name);
        match fs::symlink_metadata(&path) {
            Ok(info) => ensure!(info.is_file(), "invalid Lean build artifact {name}"),
            Err(e) if e.kind() == ErrorKind::NotFound && ext != "olean" => continue,
            Err(e) => return Err(e).with_context(|| format!("read Lean build artifact {name}")),
        }
        let mut options = fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        let source = options.open(&path)?;
        let info = source.metadata()?;
        ensure!(info.is_file(), "invalid Lean build artifact {name}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            ensure!(info.nlink() == 1, "linked Lean build artifact {name}");
        }
        let bytes = info.len();
        ensure!(
            bytes <= artifact_cap,
            "oversized Lean build artifact {name}"
        );
        *remaining_bytes = remaining_bytes
            .checked_sub(bytes)
            .context("isolated Lean build artifacts exceed byte limit")?;
        let mut dest = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(objects.join(&name))?;
        ensure!(
            io::copy(&mut source.take(bytes + 1), &mut dest)? == bytes,
            "Lean build artifact changed: {name}"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commits_only_named_parts_and_discards_module_scratch() -> Result<()> {
        let root = tempfile::tempdir()?;
        let mut pool = LeanObjects::create(&root.path().join("objects"), 32, 64)?;
        let mut first_room = PathBuf::new();
        pool.compile(
            "First.lean",
            "def first := 0",
            true,
            |room, writable, objects| {
                first_room = room.to_owned();
                assert_eq!(
                    fs::read_to_string(room.join("First.lean"))?,
                    "def first := 0"
                );
                assert_eq!(fs::read_dir(objects)?.count(), 0);
                for &suffix in PART_SUFFIXES {
                    fs::write(writable.join(format!("First.{suffix}")), b"part")?;
                }
                fs::write(writable.join("notes.txt"), b"scratch only")?;
                Ok(String::new())
            },
        )?;
        assert!(!first_room.exists());
        pool.compile(
            "Second.lean",
            "def second := 1",
            true,
            |room, writable, objects| {
                assert_ne!(room, first_room);
                assert!(!room.join("First.lean").exists());
                assert_eq!(fs::read_dir(objects)?.count(), PART_SUFFIXES.len());
                for &suffix in PART_SUFFIXES {
                    assert_eq!(fs::read(objects.join(format!("First.{suffix}")))?, b"part");
                }
                fs::write(writable.join("Second.olean"), b"next")?;
                Ok(String::new())
            },
        )?;
        Ok(())
    }

    #[test]
    fn artifact_byte_budgets_fail_closed() -> Result<()> {
        let room = tempfile::tempdir()?;
        let objects = room.path().join("objects");
        let artifact = room.path().join("Target.olean");
        fs::create_dir(&objects)?;
        fs::write(artifact, b"test")?;
        assert!(commit_object_parts(room.path(), &objects, "Target", 3, &mut 8).is_err());
        assert!(commit_object_parts(room.path(), &objects, "Target", 8, &mut 3).is_err());
        assert!(!objects.join("Target.olean").exists());
        Ok(())
    }
}
