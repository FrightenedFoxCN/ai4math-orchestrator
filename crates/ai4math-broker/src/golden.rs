use super::{VerifiedCandidate, digest};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{fs, io::Read, path::Path};

pub(crate) const ARTIFACT_CAP: u64 = 32 * 1024 * 1024;
const SET_CAP: u64 = 128 * 1024 * 1024;

#[derive(Deserialize, Serialize)]
struct Entry {
    file: String,
    sha256: String,
}

/// Built by a successful fresh regression. Never deserialize caller-supplied artifacts.
pub struct RegressionArtifacts {
    binding: RegressionBinding,
    files: Vec<(String, Vec<u8>)>,
}

struct RegressionBinding {
    receipt_sha256: String,
    base_commit: String,
    file_path: String,
    source_sha256: String,
}

impl RegressionBinding {
    fn new(candidate: &VerifiedCandidate) -> Self {
        let receipt = candidate.receipt();
        Self {
            receipt_sha256: receipt.receipt_sha256().into(),
            base_commit: receipt.base_commit().into(),
            file_path: receipt.file_path().into(),
            source_sha256: digest(candidate.source().as_bytes()),
        }
    }

    fn matches(&self, candidate: &VerifiedCandidate) -> bool {
        let receipt = candidate.receipt();
        self.receipt_sha256 == receipt.receipt_sha256()
            && self.source_sha256 == receipt.patch_sha256()
            && receipt.matches(
                &self.base_commit,
                &self.file_path,
                candidate.source().as_bytes(),
            )
    }
}

fn commit_name(commit: &str) -> Result<()> {
    ensure!(
        matches!(commit.len(), 40 | 64) && commit.bytes().all(|b| b.is_ascii_hexdigit()),
        "expected full Git commit hash"
    );
    Ok(())
}

fn valid_artifact(name: &str) -> bool {
    name.split_once('.').is_some_and(|(n, ext)| {
        !n.is_empty()
            && n.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
            && matches!(ext, "olean" | "agdai" | "vo")
    })
}

fn validate_names<'a>(names: impl IntoIterator<Item = &'a str>) -> Result<()> {
    let mut folded = std::collections::BTreeSet::new();
    for name in names {
        ensure!(
            valid_artifact(name) && folded.insert(name.to_ascii_lowercase()),
            "invalid or colliding artifact name {name:?}"
        );
    }
    ensure!(
        (1..=super::MAX_MODULES).contains(&folded.len()),
        "invalid artifact count"
    );
    Ok(())
}

pub(crate) fn bounded_file(path: &Path, max: u64) -> Result<Vec<u8>> {
    ensure!(
        fs::symlink_metadata(path)?.is_file(),
        "expected a regular file: {}",
        path.display()
    );
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.len() <= max,
        "artifact is non-regular or too large"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ensure!(metadata.nlink() == 1, "reject linked artifacts");
    }
    let mut bytes = Vec::new();
    file.take(max + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= max, "artifact exceeds size cap");
    Ok(bytes)
}

impl RegressionArtifacts {
    pub(crate) fn collect(
        output: &Path,
        names: impl IntoIterator<Item = String>,
        candidate: &VerifiedCandidate,
    ) -> Result<Self> {
        let names: Vec<_> = names.into_iter().collect();
        validate_names(names.iter().map(String::as_str))?;
        let binding = RegressionBinding::new(candidate);
        ensure!(
            binding.matches(candidate),
            "invalid verified candidate binding"
        );
        let mut total = 0;
        let mut files = Vec::new();
        for name in names {
            let bytes = bounded_file(&output.join(&name), ARTIFACT_CAP.min(SET_CAP - total))
                .with_context(|| format!("read compiled artifact {name}"))?;
            total += bytes.len() as u64;
            files.push((name, bytes));
        }
        Ok(Self { binding, files })
    }

    /// Whether this successful regression belongs to this exact broker-verified candidate.
    pub fn matches(&self, candidate: &VerifiedCandidate) -> bool {
        self.binding.matches(candidate)
    }

    /// Call only after the broker-backed Git CAS has successfully promoted this regression.
    /// root must be operator-owned, never writable by the compiler sandbox or task workers.
    pub fn publish(self, root: &Path, commit: &str) -> Result<()> {
        commit_name(commit)?;
        fs::create_dir_all(root)?;
        let staging = tempfile::tempdir_in(root)?;
        let mut manifest = Vec::new();
        for (name, bytes) in self.files {
            manifest.push(Entry {
                sha256: digest(&bytes),
                file: name.clone(),
            });
            let path = staging.path().join(&name);
            fs::write(&path, bytes)?;
            readonly(&path)?;
        }
        let metadata = staging.path().join("artifacts.json");
        fs::write(&metadata, serde_json::to_vec(&manifest)?)?;
        readonly(&metadata)?;
        let dest = root.join(commit);
        ensure!(!dest.exists(), "golden cache already exists for {commit}");
        fs::rename(staging.path(), dest)?;
        Ok(())
    }
}

fn readonly(path: &Path) -> Result<()> {
    let mut perms = fs::metadata(path)?.permissions();
    perms.set_readonly(true);
    fs::set_permissions(path, perms)?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn clonefile(source: &Path, dest: &Path) -> Result<()> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    let source = CString::new(source.as_os_str().as_bytes())?;
    let dest = CString::new(dest.as_os_str().as_bytes())?;
    const CLONE_NOFOLLOW: u32 = 0x0001; // macOS sys/clonefile.h
    if unsafe { libc::clonefile(source.as_ptr(), dest.as_ptr(), CLONE_NOFOLLOW) } != 0 {
        return Err(std::io::Error::last_os_error())
            .context("APFS clonefile failed (no buffered copy fallback)");
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn clonefile(_: &Path, _: &Path) -> Result<()> {
    anyhow::bail!("APFS clones require macOS")
}

/// Atomically place copy-on-write artifacts in a fresh path on the same APFS volume.
pub fn clone_golden(root: &Path, commit: &str, dest: &Path) -> Result<usize> {
    commit_name(commit)?;
    let source = root.join(commit);
    ensure!(
        fs::symlink_metadata(&source)?.is_dir(),
        "golden snapshot must be a plain directory"
    );
    let manifest: Vec<Entry> =
        serde_json::from_slice(&bounded_file(&source.join("artifacts.json"), 64 * 1024)?)?;
    validate_names(manifest.iter().map(|entry| entry.file.as_str()))?;
    ensure!(
        fs::symlink_metadata(dest).is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound),
        "destination must not exist"
    );
    let parent = dest
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let staging = tempfile::tempdir_in(parent)?;
    let mut total = 0;
    for entry in &manifest {
        let from = source.join(&entry.file);
        let to = staging.path().join(&entry.file);
        clonefile(&from, &to)?;
        let bytes = bounded_file(&to, ARTIFACT_CAP.min(SET_CAP - total))?;
        total += bytes.len() as u64;
        ensure!(
            digest(&bytes) == entry.sha256,
            "golden artifact digest mismatch"
        );
    }
    fs::rename(staging.path(), dest)?;
    Ok(manifest.len())
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    #[test]
    fn rejects_colliding_artifact_names() {
        assert!(validate_names(["MathCore.olean", "Mathcore.olean"]).is_err());
        assert!(validate_names(["MathCore.agdai", "MathCore.agdai"]).is_err());
        assert!(validate_names(["MathCore.vo", "Mathcore.vo"]).is_err());
        assert!(validate_names(["MathCore.vo", "MathCore2.vo"]).is_ok());
        assert!(validate_names(["MathCore.olean", "MathCore2.olean"]).is_ok());
    }

    #[test]
    fn regression_matches_its_exact_candidate_only() -> Result<()> {
        use super::super::tests::candidate_fixture;
        let tmp = tempfile::tempdir()?;
        fs::write(tmp.path().join("Fixture.olean"), b"fixture")?;
        let mut candidate = candidate_fixture(&[]);
        let artifacts =
            RegressionArtifacts::collect(tmp.path(), ["Fixture.olean".into()], &candidate)?;
        assert!(artifacts.matches(&candidate));
        candidate.receipt.receipt_sha256.push('a');
        assert!(!artifacts.matches(&candidate));
        let mut candidate = candidate_fixture(&[]);
        candidate.receipt.base_commit.replace_range(..1, "b");
        assert!(!artifacts.matches(&candidate));
        let mut candidate = candidate_fixture(&[]);
        candidate.receipt.file_path = "Other.lean".into();
        assert!(!artifacts.matches(&candidate));
        let mut candidate = candidate_fixture(&[]);
        candidate.source.push('\n');
        assert!(!artifacts.matches(&candidate));
        Ok(())
    }

    #[test]
    fn publishes_and_clones_readonly_artifacts() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let root = tmp.path().join("golden");
        let new_dir = tmp.path().join("worktree-cache");
        let commit = "a".repeat(40);
        let candidate = super::super::tests::candidate_fixture(&[]);
        fs::write(tmp.path().join("Fixture.olean"), b"fixture")?;
        RegressionArtifacts::collect(tmp.path(), ["Fixture.olean".into()], &candidate)?
            .publish(&root, &commit)?;
        assert_eq!(clone_golden(&root, &commit, &new_dir)?, 1);
        assert_eq!(fs::read(new_dir.join("Fixture.olean"))?, b"fixture");
        assert!(
            fs::metadata(root.join(&commit).join("Fixture.olean"))?
                .permissions()
                .readonly()
        );
        assert!(clone_golden(&root, "../elsewhere", &new_dir).is_err());
        Ok(())
    }
}
