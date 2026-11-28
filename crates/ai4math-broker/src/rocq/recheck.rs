//! Standalone object checking is separate from both source compilation and the
//! target's zero-assumptions audit. Only a fresh broker build may supply objects.

use super::{Install, PROJECT, TIMEOUT, Toolchain, golden, path_for, require_time, sandbox};
use crate::{MAX_MODULES, ModuleBuild, file_digest};
use ai4math_core::Engine;
use anyhow::{Context, Result, ensure};
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    time::Instant,
};

const OBJECT_SET_CAP: u64 = 128 * 1024 * 1024;

type LibraryRoot = (PathBuf, &'static str);

/// Success of the standalone checker, not a closed-target audit or receipt.
#[derive(Debug)]
pub(crate) struct ObjectRecheck {
    checker_sha256: String,
}

impl ObjectRecheck {
    pub(crate) fn checker_sha256(&self) -> &str {
        &self.checker_sha256
    }
}

fn resolve_checker(install: &Install, checker_path: &Path) -> Result<PathBuf> {
    ensure!(
        checker_path.is_absolute(),
        "expected an absolute Rocq checker path"
    );
    let checker =
        Toolchain::native_executable(checker_path).context("resolve native Rocq checker")?;
    ensure!(
        checker == install.prefix.join("bin/coqchk"),
        "Rocq checker must be the native coqchk in the compiler installation"
    );
    Ok(checker)
}

fn pin_checker(checker: &Path, expected_checker_sha: &str) -> Result<String> {
    ensure!(
        expected_checker_sha.len() == 64
            && expected_checker_sha
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "expected a lowercase Rocq checker SHA-256"
    );
    let observed = file_digest(checker).context("hash standalone Rocq checker")?;
    ensure!(
        observed == expected_checker_sha,
        "Rocq checker SHA-256 mismatch"
    );
    Ok(observed)
}

fn libraries(install: &Install) -> Result<Vec<LibraryRoot>> {
    let mut roots = Vec::new();
    for (relative, logical, required) in [
        ("theories", "Corelib", true),
        ("user-contrib/Stdlib", "Stdlib", false),
        ("user-contrib/Ltac2", "Ltac2", false),
    ] {
        let path = install.data.join(relative);
        match path.symlink_metadata() {
            Err(err) if !required && err.kind() == std::io::ErrorKind::NotFound => continue,
            result => {
                ensure!(
                    result?.is_dir() && path.canonicalize()? == path,
                    "Rocq checker library root {logical} must be a physical directory in the paired installation"
                );
            }
        }
        roots.push((path, logical));
    }
    Ok(roots)
}

fn object_files(objects: &Path, expected_sources: &[&str]) -> Result<Vec<PathBuf>> {
    ensure!(
        (1..=MAX_MODULES).contains(&expected_sources.len()),
        "invalid Rocq checker object count"
    );
    let mut folded = BTreeSet::new();
    let mut expected = BTreeSet::new();
    let mut ordered = Vec::new();
    for source in expected_sources {
        let module = path_for(Engine::Rocq, source)?;
        ensure!(
            folded.insert(module.to_ascii_lowercase()),
            "colliding Rocq checker object name {module:?}"
        );
        let name = format!("{module}.vo");
        ordered.push(objects.join(&name));
        expected.insert(name);
    }
    let mut total = 0;
    for entry in fs::read_dir(objects)? {
        let entry = entry?;
        let name = entry.file_name();
        ensure!(
            name.to_str().is_some_and(|name| expected.remove(name)),
            "unexpected file in Rocq checker object set: {name:?}"
        );
        let metadata = entry.path().symlink_metadata()?;
        ensure!(
            metadata.is_file() && (1..=golden::ARTIFACT_CAP).contains(&metadata.len()),
            "Rocq checker object must be a nonempty, bounded, regular file: {name:?}"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            ensure!(
                metadata.nlink() == 1,
                "reject linked Rocq checker object: {name:?}"
            );
        }
        total += metadata.len();
        ensure!(
            total <= OBJECT_SET_CAP,
            "Rocq checker object set exceeds size cap"
        );
    }
    ensure!(
        expected.is_empty(),
        "missing generated Rocq objects: {expected:?}"
    );
    Ok(ordered)
}

fn args(libraries: &[LibraryRoot], objects: &Path, files: &[PathBuf]) -> Result<Vec<String>> {
    // Native coqchk is the dedicated checker executable. -boot disables its
    // automatic standard/user/current-directory load paths; explicitly map the
    // approved library roots. Positional paths check recursively (unlike
    // -admit/-norec), including imported Corelib/Stdlib modules as needed.
    let mut args = ["-boot", "-silent", "-bytecode-compiler", "no"]
        .map(str::to_owned)
        .to_vec();
    for (physical, logical) in libraries
        .iter()
        .map(|(p, l)| (p.as_path(), *l))
        .chain([(objects, PROJECT)])
    {
        args.extend([
            "-Q".into(),
            physical.to_str().context("non-UTF8 Rocq load path")?.into(),
            logical.into(),
        ]);
    }
    for file in files {
        args.push(file.to_str().context("non-UTF8 Rocq object path")?.into());
    }
    Ok(args)
}

/// Advisory checker run for the exact object set produced by `build`. Never
/// widen this interface to accept an object directory from outside the broker.
/// The caller still needs a separate, downstream `audit_target` for closure.
pub(crate) fn recheck_generated_objects(
    build: &ModuleBuild<'_>,
    expected_sources: &[&str],
    checker_path: &Path,
    expected_checker_sha: &str,
    deadline: Option<Instant>,
) -> Result<ObjectRecheck> {
    require_time(deadline)?;
    let tool = build.tool;
    ensure!(
        tool.engine == Engine::Rocq,
        "standalone checker requires a Rocq build"
    );
    let install = tool
        .rocq_install
        .as_ref()
        .context("missing pinned Rocq installation")?;
    let room = build.room.canonicalize()?;
    let objects = build.output.canonicalize()?;
    ensure!(
        objects == room.join("output"),
        "invalid Rocq build output location"
    );
    let files = object_files(&objects, expected_sources)?;
    let libraries = libraries(install)?;
    let args = args(&libraries, &objects, &files)?;
    let mut data = libraries
        .into_iter()
        .map(|(path, _)| path)
        .collect::<Vec<_>>();
    data.push(objects);

    // Give the checker neither source files nor access to compiler plugins,
    // compiler runtime grants, or ambient home/load-path configuration.
    let checker_room = tempfile::tempdir()?;
    let scratch = tempfile::tempdir_in(checker_room.path())?;
    let checker = resolve_checker(install, checker_path)?;
    let checker_sha256 = pin_checker(&checker, expected_checker_sha)?;
    sandbox::run_grouped_until_in_dir(
        sandbox::Invocation {
            executable: &checker,
            args: &args,
            room: checker_room.path(),
            writable: scratch.path(),
            reads: &[],
            env: &[],
            timeout: TIMEOUT,
        },
        &data,
        // OCaml/Rocq initialization lists its own install root and bin
        // directory. Literal grants permit those directory entries without
        // recursively exposing unrelated installation files.
        &[install.prefix.clone(), install.prefix.join("bin")],
        tool.sandbox_group,
        deadline,
        scratch.path(),
    )
    .context("run standalone Rocq object checker")?;
    Ok(ObjectRecheck { checker_sha256 })
}

#[cfg(test)]
mod tests;
