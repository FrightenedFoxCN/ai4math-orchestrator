//! Validated project paths and native-execution admission.
//!
//! `ManagedProject` is the single CLI/application facade for an initialized
//! project. It keeps filesystem validation, SQLite opening and native lease
//! acquisition in one place so individual commands cannot silently use a
//! different state path or acquire resources in a different order.

#[cfg(unix)]
mod paths;

use ai4math_broker::{ProjectNativeGate, ProjectNativeLease};
use ai4math_store::StateStore;
use anyhow::{Context, Result, ensure};
use std::path::{Path, PathBuf};

/// A canonical, initialized project with an owner-controlled runtime directory.
#[derive(Clone, Debug)]
pub struct ManagedProject {
    root: PathBuf,
    runtime: PathBuf,
    state: PathBuf,
}

impl ManagedProject {
    /// Validate an existing initialized project without creating any files.
    pub fn new(root: impl AsRef<Path>) -> Result<Self> {
        let root = root
            .as_ref()
            .canonicalize()
            .context("resolve project root")?;
        ensure!(root.is_dir(), "project root is not a directory");
        let runtime = root.join(".runtime");
        let state =
            managed_state_path(&root).context("project is not an initialized private project")?;
        ensure!(runtime.is_dir(), "project runtime is not a directory");
        Ok(Self {
            root,
            runtime,
            state,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn runtime(&self) -> &Path {
        &self.runtime
    }

    pub fn state_path(&self) -> &Path {
        &self.state
    }

    /// Revalidate immediately before opening the writable state store.
    pub fn open_state(&self) -> Result<StateStore> {
        open_managed_state(&self.root, false)
    }

    /// Revalidate immediately before opening the read-only state store.
    pub fn open_state_readonly(&self) -> Result<StateStore> {
        open_managed_state(&self.root, true)
    }

    /// Acquire the exclusive project-native lease without waiting.
    pub fn try_native_heavy(&self) -> Result<ProjectNativeLease> {
        ProjectNativeGate::new(&self.root).try_heavy()
    }

    /// Run a native operation after admission. Callers must acquire any Git
    /// coordination lock inside `operation`, preserving native -> Git order.
    pub fn with_native_heavy<T>(&self, operation: impl FnOnce() -> Result<T>) -> Result<T> {
        let _lease = self.try_native_heavy()?;
        operation()
    }
}

/// Validate and return the state path for an initialized project.
pub fn managed_state_path(root: &Path) -> Result<PathBuf> {
    #[cfg(unix)]
    {
        paths::managed_state_path(root)
    }
    #[cfg(not(unix))]
    {
        let _ = root;
        anyhow::bail!("managed project state requires an owner-controlled Unix runtime")
    }
}

/// Open an initialized project's state store through the shared path gate.
pub fn open_managed_state(root: &Path, readonly: bool) -> Result<StateStore> {
    let project = ManagedProject::new(root)?;
    // Validate again after construction so a replacement between calls is not
    // mistaken for the project that was originally inspected.
    let path = managed_state_path(project.root())?;
    ensure!(
        path == project.state_path(),
        "project state path changed during validation"
    );
    if readonly {
        StateStore::open_readonly(path)
    } else {
        StateStore::open(path)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf};

    fn fixture() -> Result<(tempfile::TempDir, PathBuf)> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().canonicalize()?;
        let runtime = root.join(".runtime");
        fs::create_dir(&runtime)?;
        fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700))?;
        let state = runtime.join("state.sqlite");
        StateStore::open(&state)?;
        Ok((temp, root))
    }

    #[test]
    fn opens_existing_private_state_readonly_and_writable() -> Result<()> {
        let (_temp, root) = fixture()?;
        let project = ManagedProject::new(&root)?;
        ensure!(project.state_path().is_file(), "state file missing");
        ensure!(
            project.open_state_readonly()?.status()?.claims >= 0,
            "read failed"
        );
        ensure!(
            project.open_state()?.status()?.claims >= 0,
            "write open failed"
        );
        Ok(())
    }

    #[test]
    fn rejects_symlinked_runtime_and_shared_writable_runtime() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().canonicalize()?;
        let real = temp.path().join("real-runtime");
        fs::create_dir(&real)?;
        fs::set_permissions(&real, fs::Permissions::from_mode(0o700))?;
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, root.join(".runtime"))?;
        ensure!(
            ManagedProject::new(&root).is_err(),
            "symlink runtime accepted"
        );

        fs::remove_file(root.join(".runtime"))?;
        fs::create_dir(root.join(".runtime"))?;
        fs::set_permissions(root.join(".runtime"), fs::Permissions::from_mode(0o755))?;
        ensure!(
            ManagedProject::new(&root).is_err(),
            "shared runtime accepted"
        );
        Ok(())
    }

    #[test]
    fn rejects_state_symlink_hardlink_and_sidecar_alias() -> Result<()> {
        let (_temp, root) = fixture()?;
        let runtime = root.join(".runtime");
        let state = runtime.join("state.sqlite");
        let backup = runtime.join("state.backup");
        fs::rename(&state, &backup)?;
        #[cfg(unix)]
        std::os::unix::fs::symlink(&backup, &state)?;
        ensure!(
            ManagedProject::new(&root).is_err(),
            "state symlink accepted"
        );
        fs::remove_file(&state)?;
        fs::hard_link(&backup, &state)?;
        ensure!(
            ManagedProject::new(&root).is_err(),
            "state hardlink accepted"
        );
        fs::remove_file(&state)?;
        fs::rename(&backup, &state)?;

        let outside = root.join("outside");
        fs::write(&outside, b"keep")?;
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, runtime.join("state.sqlite-wal"))?;
        ensure!(
            ManagedProject::new(&root).is_err(),
            "sidecar symlink accepted"
        );
        Ok(())
    }
}
