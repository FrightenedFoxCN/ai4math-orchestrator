//! Deterministic, historical-only proof archives for the Wiki's Sediment stratum.
use super::ClaimEngine;
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::{CStr, CString},
    fs::{File, OpenOptions},
    io::{self, Read, Write},
    mem::MaybeUninit,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

/// Pinned historical provenance. Selecting and verifying the snapshot is the caller's job.
#[derive(Debug, Clone, Copy)]
pub struct SedimentInput<'a> {
    pub claim_id: &'a str,
    pub engine: &'a str,
    pub verified_at_commit: &'a str,
    pub source_module: &'a str,
    pub target_decl: &'a str,
    /// Exact historical module source, without line ending or whitespace normalization.
    pub source: &'a str,
    /// SHA-256 of the source bytes as 64 lowercase hex digits.
    pub source_sha256: &'a str,
    /// Module names mapped to SHA-256 of their exact pinned module bytes, in key order.
    pub dependency_hashes: &'a BTreeMap<String, String>,
    /// Historical dependency compilation order, exactly once per hashed dependency.
    pub dependency_order: &'a [String],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SedimentArtifact {
    id: String,
    markdown: String,
}

impl SedimentArtifact {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn markdown(&self) -> &str {
        &self.markdown
    }

    /// Destination relative to the project root, not the Wiki root.
    pub fn relative_path(&self) -> PathBuf {
        Path::new(".mathwiki")
            .join("sediment")
            .join(self.filename())
    }

    fn filename(&self) -> String {
        format!("{}.md", self.id)
    }
}

/// Render a non-authoritative archive. No live state, filesystem or clock is consulted.
pub fn render_sediment(input: SedimentInput<'_>) -> Result<SedimentArtifact> {
    let engine: ClaimEngine = input.engine.parse().context("invalid Sediment engine")?;
    ensure!(
        [
            input.claim_id,
            input.verified_at_commit,
            input.source_module,
            input.target_decl
        ]
        .iter()
        .all(|value| !value.trim().is_empty()),
        "blank Sediment provenance field"
    );
    ensure!(
        format!("{:x}", Sha256::digest(input.source.as_bytes())) == input.source_sha256,
        "Sediment source SHA-256 does not match the exact source bytes"
    );
    let mut fingerprints = BTreeMap::new();
    for (module, sha) in input.dependency_hashes {
        ensure!(
            !module.trim().is_empty() && lowercase_sha256(sha),
            "invalid Sediment dependency module fingerprint"
        );
        fingerprints.insert(module, format!("sha256:{sha}"));
    }

    let ordered: BTreeSet<_> = input.dependency_order.iter().collect();
    ensure!(
        ordered.len() == input.dependency_order.len()
            && ordered.into_iter().eq(input.dependency_hashes.keys()),
        "Sediment dependency order does not match dependency hashes"
    );

    // Emit JSON scalar/map syntax as YAML, avoiding plain scalars or interpolation
    // into human-readable Markdown headings. The index derives born_from_claim
    // from origin_claim; do not assert any other provenance or authority edge.
    let mut content = String::new();
    let title = format!("Historical proof archive: {}", input.target_decl);
    for (key, value) in [
        ("stratum", "sediment"),
        ("trust_level", "L5"),
        ("engine", engine.as_str()),
        ("status", "unreferenced"),
        ("title", &title),
        ("verified_at_commit", input.verified_at_commit),
        ("origin_claim", input.claim_id),
        ("source_module", input.source_module),
        ("target_decl", input.target_decl),
        ("source_sha256", input.source_sha256),
        ("def_fingerprint_kind", "module_bytes_sha256"),
    ] {
        field(&mut content, key, value)?;
    }
    field(&mut content, "def_fingerprints", &fingerprints)?;
    field(&mut content, "dependency_order", input.dependency_order)?;
    field(&mut content, "source_bytes", &input.source.len())?;
    content.push_str(concat!(
        "---\n\n# Historical proof archive\n\n",
        "This is an L5 historical archive, not current proof evidence or authorization.\n",
        "Dependency fingerprints conservatively hash whole module bytes, not individual definitions.\n\n",
        "## Formal Artifact\n\n",
    ));
    // A fence longer than any run in the source cannot be closed by source text,
    // even if the source contains Markdown with untrusted headings or code fences.
    let longest = input
        .source
        .split(|c| c != '~')
        .map(str::len)
        .max()
        .unwrap_or(0);
    let fence = "~".repeat((longest + 1).max(3));
    content.push_str(&fence);
    content.push('\n');
    content.push_str(input.source);
    if !input.source.is_empty() && !input.source.ends_with('\n') {
        // Separates the closing fence; source_bytes identifies the exact payload.
        content.push('\n');
    }
    content.push_str(&fence);
    content.push('\n');

    let mut digest = Sha256::new();
    digest.update(b"ai4math-wiki-sediment-v1\0");
    digest.update(content.as_bytes());
    let id = format!("SED-{:x}", digest.finalize());
    let markdown = format!("---\nid: \"{id}\"\n{content}");
    Ok(SedimentArtifact { id, markdown })
}

fn field<T: Serialize + ?Sized>(out: &mut String, key: &str, value: &T) -> Result<()> {
    use std::fmt::Write;

    write!(out, "{key}: ")?;
    for ch in serde_json::to_string(value)?.chars() {
        // JSON may emit these literally, but YAML can interpret some as line
        // breaks and rejects others unless escaped.
        if matches!(
            ch,
            '\u{007f}'
                ..='\u{009f}' | '\u{2028}' | '\u{2029}' | '\u{feff}' | '\u{fffe}' | '\u{ffff}'
        ) {
            write!(out, "\\u{:04x}", ch as u32)?;
        } else {
            out.push(ch);
        }
    }
    out.push('\n');
    Ok(())
}

fn lowercase_sha256(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Publish the rendered bytes without replacing an existing entry or updating the index.
/// Returns true for a new file, or false for a safe byte-identical replay.
pub fn publish_sediment(project_root: &Path, artifact: &SedimentArtifact) -> Result<bool> {
    let root = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(project_root)
        .with_context(|| format!("open Sediment project root {}", project_root.display()))?;
    // SAFETY: geteuid has no pointer arguments or preconditions.
    let uid = unsafe { libc::geteuid() };
    validate_dir(&root, uid).context("unsafe Sediment project root")?;
    let wiki = archive_dir(&root, c".mathwiki", uid).context("unsafe .mathwiki directory")?;
    let sediment = archive_dir(&wiki, c"sediment", uid).context("unsafe sediment directory")?;
    let name = CString::new(artifact.filename())?;
    let bytes = artifact.markdown.as_bytes();
    let installed = if read_existing(&sediment, &name, bytes, uid)? {
        false
    } else {
        let (mut staged, temp) = Staged::new(&sediment)?;
        staged
            .write_all(bytes)
            .context("write staged Sediment archive")?;
        staged.sync_all().context("sync staged Sediment archive")?;
        match rename_exclusive(&sediment, &temp.name, &name) {
            Ok(()) => true,
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                ensure!(
                    read_existing(&sediment, &name, bytes, uid)?,
                    "Sediment archive disappeared during publication"
                );
                false
            }
            Err(err) => return Err(err).context("install Sediment archive without replacement"),
        }
    };
    sediment.sync_all().context("sync sediment directory")?;
    wiki.sync_all().context("sync .mathwiki directory")?;
    root.sync_all().context("sync Sediment project root")?;
    verify_named(&wiki, &sediment, c"sediment")?;
    verify_named(&root, &wiki, c".mathwiki")?;
    Ok(installed)
}

fn validate_dir(dir: &File, uid: libc::uid_t) -> Result<()> {
    let meta = dir.metadata()?;
    ensure!(
        meta.is_dir() && meta.uid() == uid && meta.mode() & 0o022 == 0,
        "directory must be owned by the effective user and not writable by group or others"
    );
    Ok(())
}

fn archive_dir(parent: &File, name: &CStr, uid: libc::uid_t) -> Result<File> {
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW;
    let dir = match open_at(parent, name, flags) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            // SAFETY: the parent descriptor is live; name is a single NUL-terminated component.
            if unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) } < 0 {
                let err = io::Error::last_os_error();
                if err.kind() != io::ErrorKind::AlreadyExists {
                    return Err(err.into());
                }
            }
            open_at(parent, name, flags)?
        }
        result => result?,
    };
    validate_dir(&dir, uid)?;
    Ok(dir)
}

fn open_at(parent: &File, name: &CStr, flags: libc::c_int) -> io::Result<File> {
    // SAFETY: valid live parent descriptor and NUL-terminated name. The mode
    // variadic argument has C int width, including on platforms with a narrower mode_t.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_CLOEXEC,
            0o600 as libc::c_uint,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful openat returns a fresh descriptor, now owned by File.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn stat_at(parent: &File, name: &CStr) -> io::Result<libc::stat> {
    let mut stat = MaybeUninit::uninit();
    // SAFETY: valid descriptor and NUL-terminated name; stat points to writable storage.
    let rc = unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful fstatat initialized stat.
    Ok(unsafe { stat.assume_init() })
}

fn verify_named(parent: &File, file: &File, name: &CStr) -> Result<()> {
    let named = stat_at(parent, name).context("recheck named Sediment path")?;
    let actual = file.metadata()?;
    ensure!(
        named.st_dev == actual.dev() as libc::dev_t && named.st_ino == actual.ino() as libc::ino_t,
        "Sediment path changed while publishing"
    );
    Ok(())
}

/// Returns false only for absence, never for aliases or stale bytes.
fn read_existing(dir: &File, name: &CStr, expected: &[u8], uid: libc::uid_t) -> Result<bool> {
    let named = match stat_at(dir, name) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(false),
        result => result.context("inspect existing Sediment archive")?,
    };
    // Reject visible special files before opening and validate the opened inode too.
    ensure!(
        named.st_mode & libc::S_IFMT == libc::S_IFREG
            && named.st_nlink == 1
            && named.st_uid == uid
            && named.st_mode & 0o022 == 0,
        "unsafe existing Sediment archive"
    );
    let mut file = open_at(
        dir,
        name,
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK,
    )
    .context("open existing Sediment archive")?;
    let meta = file.metadata()?;
    ensure!(
        meta.is_file()
            && meta.nlink() == 1
            && meta.uid() == uid
            && meta.mode() & 0o022 == 0
            && named.st_dev == meta.dev() as libc::dev_t
            && named.st_ino == meta.ino() as libc::ino_t,
        "unsafe existing Sediment archive"
    );
    let mut actual = Vec::new();
    ensure!(
        meta.len() == expected.len() as u64,
        "existing Sediment archive has stale bytes"
    );
    (&mut file)
        .take(expected.len() as u64 + 1)
        .read_to_end(&mut actual)?;
    ensure!(
        actual == expected,
        "existing Sediment archive has stale bytes"
    );
    verify_named(dir, &file, name)?;
    Ok(true)
}

struct Staged<'a> {
    dir: &'a File,
    name: CString,
}

impl<'a> Staged<'a> {
    fn new(dir: &'a File) -> Result<(File, Self)> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        for _ in 0..128 {
            let nonce = NEXT.fetch_add(1, Ordering::Relaxed);
            let name = CString::new(format!(".sediment-{}-{nonce}.tmp", std::process::id()))?;
            match open_at(
                dir,
                &name,
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW,
            ) {
                Ok(file) => return Ok((file, Self { dir, name })),
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(err) => return Err(err).context("create staged Sediment archive"),
            }
        }
        anyhow::bail!("could not allocate a staged Sediment archive")
    }
}

impl Drop for Staged<'_> {
    fn drop(&mut self) {
        // SAFETY: the descriptor and the NUL-terminated staging name are live.
        unsafe { libc::unlinkat(self.dir.as_raw_fd(), self.name.as_ptr(), 0) };
    }
}

fn rename_exclusive(dir: &File, from: &CStr, to: &CStr) -> io::Result<()> {
    let fd = dir.as_raw_fd();
    // SAFETY: all descriptors are live and all names are NUL-terminated.
    // Exclusive rename avoids exposing an intermediate double-linked file on replay.
    #[cfg(target_vendor = "apple")]
    let rc = unsafe { libc::renameatx_np(fd, from.as_ptr(), fd, to.as_ptr(), libc::RENAME_EXCL) };
    #[cfg(any(target_os = "linux", target_os = "android", target_os = "freebsd"))]
    let rc = unsafe { libc::renameat2(fd, from.as_ptr(), fd, to.as_ptr(), libc::RENAME_NOREPLACE) };
    #[cfg(not(any(
        target_vendor = "apple",
        target_os = "linux",
        target_os = "android",
        target_os = "freebsd"
    )))]
    let rc = unsafe { libc::linkat(fd, from.as_ptr(), fd, to.as_ptr(), 0) };
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}
