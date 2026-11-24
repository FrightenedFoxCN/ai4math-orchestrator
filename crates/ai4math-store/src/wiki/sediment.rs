//! Deterministic, historical-only proof archives for the Wiki's Sediment stratum.
use super::ClaimEngine;
use anyhow::{Context, Result, ensure};
use serde::{Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::{CStr, CString},
    fs::{File, Metadata, OpenOptions},
    io::{self, Read, Write},
    mem::MaybeUninit,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

// Match the Phase 1 rendered proof limit in ai4math-core's proof.rs. The archive
// needs a separate cap: two Markdown fences can each be longer than the source.
const MAX_SOURCE_BYTES: usize = 2 * 1024 * 1024;
const MAX_ARCHIVE_BYTES: u64 = 8 * 1024 * 1024;
const BODY_PREAMBLE: &str = concat!(
    "\n# Historical proof archive\n\n",
    "This is an L5 historical archive, not current proof evidence or authorization.\n",
    "Dependency fingerprints conservatively hash whole module bytes, not individual definitions.\n\n",
    "## Formal Artifact\n\n",
);

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
    content.push_str("---\n");
    content.push_str(BODY_PREAMBLE);
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

/// Historical recipe fields recovered from a canonical v1 Sediment archive.
/// Archive consistency does not authenticate the historical proof or its provenance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StoredSediment {
    pub claim_id: String,
    pub engine: String,
    pub verified_at_commit: String,
    pub source_module: String,
    pub target_decl: String,
    pub source_sha256: String,
    /// Module names mapped to raw lowercase SHA-256, without the wire-format prefix.
    pub dependency_hashes: BTreeMap<String, String>,
    pub dependency_order: Vec<String>,
}

/// Read a canonical generated archive, with a 2 MiB source and 8 MiB Markdown limit.
/// Checks the named archive's content address; does not authenticate proof evidence.
/// Neither creates directories nor updates the Wiki index.
pub fn read_sediment(project_root: &Path, id: &str) -> Result<StoredSediment> {
    ensure!(
        id.strip_prefix("SED-").is_some_and(lowercase_sha256),
        "invalid Sediment id; expected SED- followed by 64 lowercase hex digits"
    );
    let dirs = ArchiveDirs::open(project_root, DirMode::Existing)?;
    let name = CString::new(format!("{id}.md"))?;
    let (mut file, meta) =
        open_existing(&dirs.sediment, &name, dirs.uid)?.context("Sediment archive not found")?;
    ensure!(
        meta.len() <= MAX_ARCHIVE_BYTES,
        "Sediment archive exceeds the {MAX_ARCHIVE_BYTES}-byte file limit"
    );
    let mut bytes = Vec::with_capacity(meta.len() as usize);
    (&mut file)
        .take(MAX_ARCHIVE_BYTES + 1)
        .read_to_end(&mut bytes)
        .context("read Sediment archive")?;
    ensure!(
        bytes.len() as u64 <= MAX_ARCHIVE_BYTES,
        "Sediment archive exceeds the {MAX_ARCHIVE_BYTES}-byte file limit"
    );
    let stored = parse_sediment(
        id,
        std::str::from_utf8(&bytes).context("Sediment archive is not UTF-8")?,
    )?;
    let after = file.metadata()?;
    validate_file(&after, dirs.uid)?;
    ensure!(
        bytes.len() as u64 == meta.len() && meta.len() == after.len(),
        "Sediment archive size changed while reading"
    );
    verify_named(&dirs.sediment, &file, &name)?;
    dirs.verify_named()?;
    Ok(stored)
}

pub(super) fn parse_sediment(id: &str, markdown: &str) -> Result<StoredSediment> {
    let (header, body) = markdown
        .strip_prefix("---\n")
        .and_then(|document| document.split_once("\n---\n"))
        .context("invalid Sediment v1 header envelope")?;
    // This is a generated, ordered subset of YAML (one JSON value per key),
    // not a permissive YAML parser. The renderer below checks ignored fields too.
    let mut fields = header.split('\n');
    let header_id: String = json_field(&mut fields, "id")?;
    ensure!(
        header_id == id,
        "Sediment header id does not match requested id"
    );
    expect_field(&mut fields, "stratum")?;
    expect_field(&mut fields, "trust_level")?;
    let engine = json_field(&mut fields, "engine")?;
    expect_field(&mut fields, "status")?;
    expect_field(&mut fields, "title")?;
    let verified_at_commit = json_field(&mut fields, "verified_at_commit")?;
    let claim_id = json_field(&mut fields, "origin_claim")?;
    let source_module = json_field(&mut fields, "source_module")?;
    let target_decl = json_field(&mut fields, "target_decl")?;
    let source_sha256 = json_field(&mut fields, "source_sha256")?;
    expect_field(&mut fields, "def_fingerprint_kind")?;
    let fingerprints: BTreeMap<String, String> = json_field(&mut fields, "def_fingerprints")?;
    let dependency_hashes = fingerprints
        .into_iter()
        .map(|(module, fingerprint)| {
            let sha = fingerprint
                .strip_prefix("sha256:")
                .context("invalid Sediment dependency fingerprint")?;
            Ok((module, sha.to_owned()))
        })
        .collect::<Result<_>>()?;
    let dependency_order = json_field(&mut fields, "dependency_order")?;
    let source_bytes: usize = json_field(&mut fields, "source_bytes")?;
    ensure!(fields.next().is_none(), "unexpected Sediment header field");
    ensure!(
        source_bytes <= MAX_SOURCE_BYTES,
        "Sediment source exceeds the {MAX_SOURCE_BYTES}-byte limit"
    );
    let (fence, payload) = body
        .strip_prefix(BODY_PREAMBLE)
        .and_then(|body| body.split_once('\n'))
        .context("invalid Sediment v1 body envelope")?;
    ensure!(
        fence.len() >= 3 && fence.bytes().all(|b| b == b'~'),
        "invalid Sediment source fence"
    );
    let source = payload
        .get(..source_bytes)
        .context("invalid Sediment source byte length or UTF-8 boundary")?;
    let stored = StoredSediment {
        claim_id,
        engine,
        verified_at_commit,
        source_module,
        target_decl,
        source_sha256,
        dependency_hashes,
        dependency_order,
    };
    let rendered = render_sediment(SedimentInput {
        claim_id: &stored.claim_id,
        engine: &stored.engine,
        verified_at_commit: &stored.verified_at_commit,
        source_module: &stored.source_module,
        target_decl: &stored.target_decl,
        source,
        source_sha256: &stored.source_sha256,
        dependency_hashes: &stored.dependency_hashes,
        dependency_order: &stored.dependency_order,
    })?;
    ensure!(
        rendered.id() == id,
        "Sediment archive digest does not match requested id"
    );
    ensure!(
        rendered.markdown().as_bytes() == markdown.as_bytes(),
        "Sediment archive is not canonical v1 Markdown"
    );
    Ok(stored)
}

fn expect_field<'a>(fields: &mut impl Iterator<Item = &'a str>, key: &str) -> Result<&'a str> {
    fields
        .next()
        .and_then(|line| line.strip_prefix(key)?.strip_prefix(": "))
        .with_context(|| format!("invalid or missing Sediment {key} field"))
}

fn json_field<'a, T: DeserializeOwned>(
    fields: &mut impl Iterator<Item = &'a str>,
    key: &str,
) -> Result<T> {
    serde_json::from_str(expect_field(fields, key)?)
        .with_context(|| format!("invalid JSON in Sediment {key} field"))
}

/// Publish the rendered bytes without replacing an existing entry or updating the index.
/// Returns true for a new file, or false for a safe byte-identical replay.
pub fn publish_sediment(project_root: &Path, artifact: &SedimentArtifact) -> Result<bool> {
    let dirs = ArchiveDirs::open(project_root, DirMode::CreateMissing)?;
    let sediment = &dirs.sediment;
    let uid = dirs.uid;
    let name = CString::new(artifact.filename())?;
    let bytes = artifact.markdown.as_bytes();
    let installed = if read_existing(sediment, &name, bytes, uid)? {
        false
    } else {
        let (mut staged, temp) = Staged::new(sediment)?;
        staged
            .write_all(bytes)
            .context("write staged Sediment archive")?;
        staged.sync_all().context("sync staged Sediment archive")?;
        match rename_exclusive(sediment, &temp.name, &name) {
            Ok(()) => true,
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                ensure!(
                    read_existing(sediment, &name, bytes, uid)?,
                    "Sediment archive disappeared during publication"
                );
                false
            }
            Err(err) => return Err(err).context("install Sediment archive without replacement"),
        }
    };
    sediment.sync_all().context("sync sediment directory")?;
    dirs.wiki.sync_all().context("sync .mathwiki directory")?;
    dirs.root.sync_all().context("sync Sediment project root")?;
    dirs.verify_named()?;
    Ok(installed)
}

#[derive(Clone, Copy)]
enum DirMode {
    Existing,
    CreateMissing,
}

struct ArchiveDirs {
    root: File,
    wiki: File,
    sediment: File,
    uid: libc::uid_t,
}

impl ArchiveDirs {
    fn open(project_root: &Path, mode: DirMode) -> Result<Self> {
        // A trailing slash or "/." would make the project root an intermediate
        // component, bypassing O_NOFOLLOW on that directory name.
        let root_path: PathBuf = project_root.components().collect();
        let root = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(root_path)
            .with_context(|| format!("open Sediment project root {}", project_root.display()))?;
        // SAFETY: geteuid has no pointer arguments or preconditions.
        let uid = unsafe { libc::geteuid() };
        validate_dir(&root, uid).context("unsafe Sediment project root")?;
        let wiki =
            archive_dir(&root, c".mathwiki", uid, mode).context("open safe .mathwiki directory")?;
        let sediment =
            archive_dir(&wiki, c"sediment", uid, mode).context("open safe sediment directory")?;
        Ok(Self {
            root,
            wiki,
            sediment,
            uid,
        })
    }

    fn verify_named(&self) -> Result<()> {
        verify_named(&self.wiki, &self.sediment, c"sediment")?;
        verify_named(&self.root, &self.wiki, c".mathwiki")
    }
}

fn validate_dir(dir: &File, uid: libc::uid_t) -> Result<()> {
    let meta = dir.metadata()?;
    ensure!(
        meta.is_dir() && meta.uid() == uid && meta.mode() & 0o022 == 0,
        "directory must be owned by the effective user and not writable by group or others"
    );
    Ok(())
}

fn archive_dir(parent: &File, name: &CStr, uid: libc::uid_t, mode: DirMode) -> Result<File> {
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW;
    let dir = match open_at(parent, name, flags) {
        Err(err)
            if matches!(mode, DirMode::CreateMissing) && err.kind() == io::ErrorKind::NotFound =>
        {
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
        "Sediment path changed while accessing archive"
    );
    Ok(())
}

fn validate_file(meta: &Metadata, uid: libc::uid_t) -> Result<()> {
    ensure!(
        meta.is_file() && meta.nlink() == 1 && meta.uid() == uid && meta.mode() & 0o022 == 0,
        "unsafe existing Sediment archive"
    );
    Ok(())
}

/// Returns None only for absence, never for aliases or unsafe paths.
fn open_existing(dir: &File, name: &CStr, uid: libc::uid_t) -> Result<Option<(File, Metadata)>> {
    let named = match stat_at(dir, name) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
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
    let file = open_at(
        dir,
        name,
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK,
    )
    .context("open existing Sediment archive")?;
    let meta = file.metadata()?;
    validate_file(&meta, uid)?;
    ensure!(
        named.st_dev == meta.dev() as libc::dev_t && named.st_ino == meta.ino() as libc::ino_t,
        "Sediment archive changed while opening"
    );
    Ok(Some((file, meta)))
}

/// Returns false only for absence, never for aliases or stale bytes.
fn read_existing(dir: &File, name: &CStr, expected: &[u8], uid: libc::uid_t) -> Result<bool> {
    let Some((mut file, meta)) = open_existing(dir, name, uid)? else {
        return Ok(false);
    };
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
