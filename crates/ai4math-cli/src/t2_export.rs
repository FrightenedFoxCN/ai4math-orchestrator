//! Operator-only check of a pinned Lean export, not a managed project audit.
use ai4math_broker::{LEAN_EXPORT_MAX_BYTES, check_lean_export};
use anyhow::{Context, Result, ensure};
use std::{fs, io::Read, path::Path};

fn read_export(path: &Path) -> Result<Vec<u8>> {
    let before = fs::symlink_metadata(path)?;
    ensure!(
        before.is_file(),
        "Lean export must be a regular, non-symlink file"
    );
    ensure!(
        before.len() <= LEAN_EXPORT_MAX_BYTES as u64,
        "Lean export exceeds the 16 MiB input limit"
    );
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    let opened = file.metadata()?;
    ensure!(
        opened.is_file() && opened.len() <= LEAN_EXPORT_MAX_BYTES as u64,
        "invalid or oversized Lean export file"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ensure!(
            (opened.dev(), opened.ino()) == (before.dev(), before.ino()),
            "Lean export was replaced during open"
        );
    }
    let mut bytes = Vec::new();
    file.take(LEAN_EXPORT_MAX_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= LEAN_EXPORT_MAX_BYTES,
        "Lean export exceeds the 16 MiB input limit"
    );
    Ok(bytes)
}

pub(crate) fn run(
    export: &Path,
    export_sha256: &str,
    checker: &Path,
    checker_sha256: &str,
) -> Result<()> {
    let bytes = read_export(export).context("read pinned Lean export")?;
    crate::print_json(check_lean_export(
        &bytes,
        export_sha256,
        checker,
        checker_sha256,
    )?)
}
