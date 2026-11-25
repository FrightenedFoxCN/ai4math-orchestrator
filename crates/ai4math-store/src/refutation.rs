//! Content-addressed Graveyard artifact generation for kernel-authorized refutations.
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{ErrorKind, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

pub struct WitnessInput<'a> {
    pub project_root: &'a Path,
    pub repository: &'a Path,
    pub claim_id: &'a str,
    pub committed_source: &'a str,
    pub proof_body: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WitnessSpecBinding {
    pub repo: String,
    pub commit_id: String,
    pub file_path: String,
    pub proposition_name: String,
}

#[derive(Debug, Serialize)]
pub struct RefutationRecord {
    pub wiki_id: String,
    pub witness_sha256: String,
    pub event_seq: i64,
    pub already_recorded: bool,
}

#[derive(Serialize)]
struct Frontmatter<'a> {
    id: &'a str,
    stratum: &'static str,
    engine: &'static str,
    trust_level: &'static str,
    status: &'static str,
    pinned: bool,
    title: &'a str,
    verified_at_commit: &'a str,
    origin_claim: &'a str,
    informal_gloss: &'a str,
}

pub(crate) struct RenderedRefutation {
    pub markdown: String,
    pub markdown_sha256: String,
}

pub(crate) fn wiki_id(claim: &str, witness_sha256: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(b"ai4math-graveyard-v1\0");
    hash.update(witness_sha256.as_bytes());
    hash.update(claim.as_bytes());
    format!("GRV-{:x}", hash.finalize())
}

pub(crate) fn render(
    input: &WitnessInput<'_>,
    id: &str,
    base: &str,
    module: &str,
    proposition: &str,
    source_sha: &str,
    witness_sha: &str,
    checker: &str,
) -> Result<RenderedRefutation> {
    let title = format!("Kernel refutation: {proposition}");
    let gloss = format!("Kernel-verified negation of {module}#{proposition} at commit {base}.");
    let fm = Frontmatter {
        id,
        stratum: "graveyard",
        engine: "lean4",
        trust_level: "L2b",
        status: "refuted",
        pinned: true,
        title: &title,
        verified_at_commit: base,
        origin_claim: input.claim_id,
        informal_gloss: &gloss,
    };
    let yaml = serde_yaml::to_string(&fm)?;
    let mut markdown = format!(
        "---\n{yaml}---\n\n## Kernel Refutation\n\n- Claim: {}\n- Spec: {module}#{proposition}\n- Source SHA-256: {source_sha}\n- Witness SHA-256: {witness_sha}\n\n## Verified Lean proof body\n\n",
        input.claim_id
    );
    // Indent rather than interpolate a code fence; proof text cannot terminate the code block.
    for line in input.proof_body.lines() {
        markdown.push_str("    ");
        markdown.push_str(line);
        markdown.push('\n');
    }
    markdown.push_str("\n## Broker checker module\n\n");
    for line in checker.lines() {
        markdown.push_str("    ");
        markdown.push_str(line);
        markdown.push('\n');
    }
    Ok(RenderedRefutation {
        markdown_sha256: format!("{:x}", Sha256::digest(markdown.as_bytes())),
        markdown,
    })
}

/// Write the durable rendered bytes into the private outbox without making them
/// visible to Wiki rebuilds. The SQLite intent must already be committed.
pub(crate) fn stage(project_root: &Path, id: &str, markdown: &str) -> Result<()> {
    #[cfg(unix)]
    {
        secure::stage(project_root, id, markdown.as_bytes())
    }
    #[cfg(not(unix))]
    {
        let _ = (project_root, id, markdown);
        anyhow::bail!("safe Graveyard staging requires a Unix dirfd filesystem")
    }
}

/// Publish previously staged bytes without replacing an existing archive.
/// Returns true only when this call installed the final Graveyard file.
///
/// The SQLite refutation ledger must already be committed. Publication is an
/// exclusive rename, so concurrent reconciliation can expose at most one final
/// inode and no unledgered L2b Markdown can become visible.
pub(crate) fn publish(project_root: &Path, id: &str, markdown: &str) -> Result<bool> {
    #[cfg(unix)]
    {
        secure::publish(project_root, id, markdown.as_bytes())
    }
    #[cfg(not(unix))]
    {
        let _ = (project_root, id, markdown);
        anyhow::bail!("safe Graveyard publication requires a Unix dirfd filesystem")
    }
}

#[cfg(unix)]
mod secure {
    use super::*;
    use std::{
        ffi::{CStr, CString},
        mem::MaybeUninit,
        os::fd::{AsRawFd, FromRawFd, RawFd},
        os::unix::fs::{MetadataExt, OpenOptionsExt},
        sync::atomic::{AtomicU64, Ordering},
    };

    const O_ARCHIVE_DIR: libc::c_int = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW;
    const O_SAFE_FILE: libc::c_int = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK;

    struct ArchiveDirs {
        root: File,
        wiki: File,
        graveyard: File,
        pending: File,
        uid: libc::uid_t,
    }

    impl ArchiveDirs {
        fn open(project_root: &Path) -> Result<Self> {
            // A trailing slash or "/." would make the project root an intermediate
            // component, bypassing O_NOFOLLOW on that directory name.
            let root_path: PathBuf = project_root.components().collect();
            let root = OpenOptions::new()
                .read(true)
                .custom_flags(O_ARCHIVE_DIR | libc::O_NONBLOCK)
                .open(root_path)
                .with_context(|| {
                    format!("open Graveyard project root {}", project_root.display())
                })?;
            // SAFETY: geteuid has no pointer arguments or preconditions.
            let uid = unsafe { libc::geteuid() };
            validate_dir(&root, uid).context("unsafe Graveyard project root")?;
            let wiki = archive_dir(&root, c".mathwiki", uid, false)
                .context("open safe .mathwiki directory")?;
            let graveyard = archive_dir(&wiki, c"graveyard", uid, false)
                .context("open safe graveyard directory")?;
            // This directory is outside every Wiki stratum. It may be created after
            // the intent commits, and a crash can leave only private replay state here.
            let pending = archive_dir(&wiki, c".refutation-outbox", uid, true)
                .context("open private refutation outbox directory")?;
            Ok(Self {
                root,
                wiki,
                graveyard,
                pending,
                uid,
            })
        }

        fn verify_named(&self) -> Result<()> {
            verify_named(&self.wiki, &self.graveyard, c"graveyard")?;
            verify_named(&self.wiki, &self.pending, c".refutation-outbox")?;
            verify_named(&self.root, &self.wiki, c".mathwiki")
        }

        fn sync(&self) -> Result<()> {
            self.graveyard
                .sync_all()
                .context("sync graveyard directory")?;
            self.pending
                .sync_all()
                .context("sync refutation outbox directory")?;
            self.wiki.sync_all().context("sync .mathwiki directory")?;
            self.root.sync_all().context("sync Graveyard project root")
        }
    }

    pub(super) fn stage(project_root: &Path, id: &str, bytes: &[u8]) -> Result<()> {
        let dirs = ArchiveDirs::open(project_root)?;
        let stage_name = archive_name(id, ".pending")?;
        let lock_name = archive_name(id, ".lock")?;
        let _lock = acquire_lock(&dirs.pending, &lock_name, dirs.uid)?;
        dirs.verify_named()?;
        let (staged, _) = write_stage(&dirs.pending, &stage_name, bytes, dirs.uid)?;
        dirs.sync()?;
        dirs.verify_named()?;
        drop(staged);
        Ok(())
    }

    pub(super) fn publish(project_root: &Path, id: &str, bytes: &[u8]) -> Result<bool> {
        let dirs = ArchiveDirs::open(project_root)?;
        let final_name = archive_name(id, ".md")?;
        let stage_name = archive_name(id, ".pending")?;
        let lock_name = archive_name(id, ".lock")?;
        // Serialize filesystem reconciliation without holding a SQLite writer.
        // The lock file is private, singly linked, and never interpreted as evidence.
        let _lock = acquire_lock(&dirs.pending, &lock_name, dirs.uid)?;
        dirs.verify_named()?;

        if read_existing(&dirs.graveyard, &final_name, bytes, dirs.uid)? {
            // A previous publisher may have installed the final inode and crashed
            // before retiring its verified staged bytes. Remove only an exact,
            // singly-linked copy of the durable recipe, never an arbitrary path.
            remove_verified_stage(&dirs.pending, &stage_name, bytes, dirs.uid)?;
            dirs.sync()?;
            dirs.verify_named()?;
            return Ok(false);
        }

        let (staged, _) = write_stage(&dirs.pending, &stage_name, bytes, dirs.uid)?;
        match rename_exclusive(&dirs.pending, &stage_name, &dirs.graveyard, &final_name) {
            Ok(()) => {}
            Err(err) if err.kind() == ErrorKind::AlreadyExists => {
                ensure!(
                    read_existing(&dirs.graveyard, &final_name, bytes, dirs.uid)?,
                    "Graveyard archive disappeared during publication"
                );
                remove_verified_stage(&dirs.pending, &stage_name, bytes, dirs.uid)?;
                dirs.sync()?;
                dirs.verify_named()?;
                return Ok(false);
            }
            Err(err) => {
                return Err(err).context("install Graveyard archive without replacement");
            }
        }
        // Keep the staged descriptor open until after the exclusive rename. It is
        // the exact inode now named in Graveyard and is checked again after fsync.
        dirs.sync()?;
        dirs.verify_named()?;
        ensure!(
            read_existing(&dirs.graveyard, &final_name, bytes, dirs.uid)?,
            "published Graveyard archive failed inode recheck"
        );
        drop(staged);
        Ok(true)
    }

    fn archive_name(id: &str, suffix: &str) -> Result<CString> {
        ensure!(
            !id.is_empty()
                && id.len() <= 128
                && id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':')),
            "invalid Graveyard archive identifier"
        );
        Ok(CString::new(format!("{id}{suffix}"))?)
    }

    fn acquire_lock(dir: &File, name: &CStr, uid: libc::uid_t) -> Result<File> {
        let file = match open_at(
            dir,
            name,
            libc::O_RDWR | libc::O_CREAT | libc::O_NOFOLLOW | libc::O_NONBLOCK,
        ) {
            Ok(file) => file,
            Err(err) if err.kind() == ErrorKind::AlreadyExists => open_at(
                dir,
                name,
                libc::O_RDWR | libc::O_NOFOLLOW | libc::O_NONBLOCK,
            )?,
            Err(err) => return Err(err).context("open refutation publication lock"),
        };
        validate_file(&file.metadata()?, uid).context("unsafe refutation publication lock")?;
        verify_named(dir, &file, name)?;
        // SAFETY: the descriptor is live and flock has no pointer preconditions.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == ErrorKind::WouldBlock {
                anyhow::bail!("another process is publishing this Graveyard archive");
            }
            return Err(err).context("lock refutation publication");
        }
        Ok(file)
    }

    fn validate_dir(dir: &File, uid: libc::uid_t) -> Result<()> {
        let meta = dir.metadata()?;
        ensure!(
            meta.is_dir() && meta.uid() == uid && meta.mode() & 0o022 == 0,
            "directory must be owned by the effective user and not writable by group or others"
        );
        validate_acl(dir.as_raw_fd()).context("unsafe directory ACL")
    }

    fn archive_dir(parent: &File, name: &CStr, uid: libc::uid_t, create: bool) -> Result<File> {
        let flags = O_ARCHIVE_DIR | libc::O_NONBLOCK;
        let dir = match open_at(parent, name, flags) {
            Err(err) if create && err.kind() == ErrorKind::NotFound => {
                // SAFETY: the parent descriptor is live; name is one NUL-terminated component.
                if unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) } < 0 {
                    let err = std::io::Error::last_os_error();
                    if err.kind() != ErrorKind::AlreadyExists {
                        return Err(err.into());
                    }
                }
                open_at(parent, name, flags)?
            }
            result => result?,
        };
        validate_dir(&dir, uid)?;
        verify_named(parent, &dir, name)?;
        Ok(dir)
    }

    fn open_at(parent: &File, name: &CStr, flags: libc::c_int) -> std::io::Result<File> {
        // SAFETY: valid live parent descriptor and NUL-terminated name. The mode
        // argument has C int width, including on platforms with a narrower mode_t.
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                name.as_ptr(),
                flags | libc::O_CLOEXEC,
                0o600 as libc::c_uint,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: successful openat returns a fresh descriptor, now owned by File.
        Ok(unsafe { File::from_raw_fd(fd) })
    }

    fn stat_at(parent: &File, name: &CStr) -> std::io::Result<libc::stat> {
        let mut stat = MaybeUninit::uninit();
        // SAFETY: valid descriptor and NUL-terminated name; stat is writable storage.
        let rc = unsafe {
            libc::fstatat(
                parent.as_raw_fd(),
                name.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if rc < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: successful fstatat initialized stat.
        Ok(unsafe { stat.assume_init() })
    }

    fn verify_named(parent: &File, file: &File, name: &CStr) -> Result<()> {
        let named = stat_at(parent, name).context("recheck named Graveyard path")?;
        let actual = file.metadata()?;
        ensure!(
            named.st_dev == actual.dev() as libc::dev_t
                && named.st_ino == actual.ino() as libc::ino_t,
            "Graveyard path changed while accessing archive"
        );
        validate_acl(file.as_raw_fd()).context("Graveyard path ACL changed while accessing archive")
    }

    fn validate_file(meta: &fs::Metadata, uid: libc::uid_t) -> Result<()> {
        ensure!(
            meta.is_file() && meta.nlink() == 1 && meta.uid() == uid && meta.mode() & 0o022 == 0,
            "unsafe existing Graveyard archive"
        );
        Ok(())
    }

    /// Returns None only for absence, never for aliases or unsafe paths.
    fn open_existing(
        dir: &File,
        name: &CStr,
        uid: libc::uid_t,
    ) -> Result<Option<(File, fs::Metadata)>> {
        let named = match stat_at(dir, name) {
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
            result => result.context("inspect existing Graveyard archive")?,
        };
        // Reject hard links, FIFOs, devices and symlinks before opening; validate
        // the opened inode again so a name swap cannot redirect the read.
        ensure!(
            named.st_mode & libc::S_IFMT == libc::S_IFREG
                && named.st_nlink == 1
                && named.st_uid == uid
                && named.st_mode & 0o022 == 0,
            "unsafe existing Graveyard archive"
        );
        let file = open_at(dir, name, O_SAFE_FILE).context("open existing Graveyard archive")?;
        let meta = file.metadata()?;
        validate_file(&meta, uid)?;
        ensure!(
            named.st_dev == meta.dev() as libc::dev_t && named.st_ino == meta.ino() as libc::ino_t,
            "Graveyard archive changed while opening"
        );
        Ok(Some((file, meta)))
    }

    /// Returns false only for absence, never for aliases or stale bytes.
    fn read_existing(dir: &File, name: &CStr, expected: &[u8], uid: libc::uid_t) -> Result<bool> {
        let Some((mut file, meta)) = open_existing(dir, name, uid)? else {
            return Ok(false);
        };
        ensure!(
            meta.len() == expected.len() as u64,
            "existing Graveyard archive has stale bytes"
        );
        let mut actual = Vec::new();
        (&mut file)
            .take(expected.len() as u64 + 1)
            .read_to_end(&mut actual)?;
        ensure!(
            actual == expected,
            "existing Graveyard archive has stale bytes"
        );
        let after = file.metadata()?;
        ensure!(
            after.dev() == meta.dev()
                && after.ino() == meta.ino()
                && after.len() == meta.len()
                && after.mtime() == meta.mtime()
                && after.ctime() == meta.ctime(),
            "Graveyard archive changed while reading"
        );
        verify_named(dir, &file, name)?;
        Ok(true)
    }

    fn write_stage(
        dir: &File,
        name: &CStr,
        expected: &[u8],
        uid: libc::uid_t,
    ) -> Result<(File, fs::Metadata)> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        if let Some((mut file, meta)) = open_existing(dir, name, uid)? {
            ensure!(
                meta.len() <= expected.len() as u64,
                "staged Graveyard archive has stale bytes"
            );
            let mut prefix = Vec::new();
            (&mut file).take(meta.len()).read_to_end(&mut prefix)?;
            ensure!(
                prefix.len() == meta.len() as usize && expected.starts_with(&prefix),
                "staged Graveyard archive has stale bytes"
            );
            if prefix.len() == expected.len() {
                return Ok((file, meta));
            }
            // Resume a crash-truncated private stage. The publication lock and
            // inode checks ensure this appends only to our exact durable prefix.
            let mut writer = open_at(
                dir,
                name,
                libc::O_WRONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK,
            )?;
            let writer_meta = writer.metadata()?;
            validate_file(&writer_meta, uid)?;
            ensure!(
                writer_meta.dev() == meta.dev() && writer_meta.ino() == meta.ino(),
                "staged Graveyard archive changed before append"
            );
            writer.seek(SeekFrom::Start(prefix.len() as u64))?;
            writer.write_all(&expected[prefix.len()..])?;
            writer.sync_all().context("sync staged Graveyard archive")?;
            drop(writer);
        } else {
            let mut created = None;
            for _ in 0..128 {
                // The nonce is not security-sensitive; O_EXCL and the per-ID
                // flock make collision handling deterministic and race-free.
                let nonce = NEXT.fetch_add(1, Ordering::Relaxed);
                let temp = CString::new(format!(".{}.{nonce}.tmp", name.to_string_lossy()))?;
                match open_at(
                    dir,
                    &temp,
                    libc::O_WRONLY
                        | libc::O_CREAT
                        | libc::O_EXCL
                        | libc::O_NOFOLLOW
                        | libc::O_NONBLOCK,
                ) {
                    Ok(mut file) => {
                        validate_file(&file.metadata()?, uid)?;
                        file.write_all(expected)?;
                        file.sync_all().context("sync staged Graveyard archive")?;
                        verify_named(dir, &file, &temp)?;
                        match rename_exclusive(dir, &temp, dir, name) {
                            Ok(()) => {}
                            Err(err) if err.kind() == ErrorKind::AlreadyExists => {
                                // Another writer won before our private rename.
                                // This inode was created by us and is still singly
                                // linked; retire only that exact temporary inode.
                                ensure!(
                                    read_existing(dir, &temp, expected, uid)?,
                                    "staged Graveyard temporary failed byte recheck"
                                );
                                // SAFETY: live directory and our NUL-terminated temp name.
                                if unsafe { libc::unlinkat(dir.as_raw_fd(), temp.as_ptr(), 0) } < 0
                                {
                                    return Err(std::io::Error::last_os_error().into());
                                }
                            }
                            Err(err) => {
                                return Err(err).context("install staged Graveyard recipe");
                            }
                        }
                        created = Some(file);
                        break;
                    }
                    Err(err) if err.kind() == ErrorKind::AlreadyExists => continue,
                    Err(err) => return Err(err).context("create staged Graveyard archive"),
                }
            }
            let file = created.context("could not allocate a staged Graveyard archive")?;
            drop(file);
        }
        let (file, meta) =
            open_existing(dir, name, uid)?.context("staged Graveyard archive disappeared")?;
        ensure!(
            read_existing(dir, name, expected, uid)?,
            "staged Graveyard archive failed byte recheck"
        );
        Ok((file, meta))
    }

    fn remove_verified_stage(
        dir: &File,
        name: &CStr,
        expected: &[u8],
        uid: libc::uid_t,
    ) -> Result<()> {
        if !read_existing(dir, name, expected, uid)? {
            return Ok(());
        }
        // SAFETY: live directory and NUL-terminated name, verified as our exact
        // singly-linked staged inode immediately above.
        if unsafe { libc::unlinkat(dir.as_raw_fd(), name.as_ptr(), 0) } < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() != ErrorKind::NotFound {
                return Err(err.into());
            }
        }
        dir.sync_all().context("sync retired Graveyard stage")?;
        Ok(())
    }

    /// POSIX ACL group-class permissions are bounded by the group mode mask on
    /// Linux/FreeBSD, so the mode checks above also cover named-user/group ACLs.
    /// macOS extended ACLs are evaluated before BSD mode fallback and can grant
    /// write/delete while st_mode still looks private; inspect the live fd there.
    #[cfg(not(target_vendor = "apple"))]
    fn validate_acl(_fd: RawFd) -> Result<()> {
        Ok(())
    }

    #[cfg(target_vendor = "apple")]
    fn validate_acl(fd: RawFd) -> Result<()> {
        use std::{ffi::c_void, ptr};

        // Darwin's libc exposes these C ABI functions, but the libc crate does not.
        // acl_get_fd operates on the already-open inode, avoiding a path race.
        unsafe extern "C" {
            fn acl_get_fd(fd: libc::c_int) -> *mut c_void;
            fn acl_free(acl: *mut c_void) -> libc::c_int;
            fn acl_get_entry(
                acl: *mut c_void,
                entry_id: libc::c_int,
                entry: *mut *mut c_void,
            ) -> libc::c_int;
            fn acl_get_tag_type(entry: *mut c_void, tag: *mut libc::c_int) -> libc::c_int;
            fn acl_get_permset_mask_np(entry: *mut c_void, mask: *mut u64) -> libc::c_int;
        }
        const ACL_FIRST_ENTRY: libc::c_int = 0;
        const ACL_NEXT_ENTRY: libc::c_int = -1;
        const ACL_EXTENDED_ALLOW: libc::c_int = 1;
        const ACL_WRITE_DATA: u64 = 1 << 2;
        const ACL_DELETE: u64 = 1 << 4;
        const ACL_APPEND_DATA: u64 = 1 << 5;
        const ACL_DELETE_CHILD: u64 = 1 << 6;
        const ACL_WRITE_ATTRIBUTES: u64 = 1 << 8;
        const ACL_WRITE_EXTATTRIBUTES: u64 = 1 << 10;
        const ACL_WRITE_SECURITY: u64 = 1 << 12;
        const ACL_CHANGE_OWNER: u64 = 1 << 13;
        const MUTATION_RIGHTS: u64 = ACL_WRITE_DATA
            | ACL_DELETE
            | ACL_APPEND_DATA
            | ACL_DELETE_CHILD
            | ACL_WRITE_ATTRIBUTES
            | ACL_WRITE_EXTATTRIBUTES
            | ACL_WRITE_SECURITY
            | ACL_CHANGE_OWNER;

        // SAFETY: __error returns this thread's errno cell; fd is owned by File.
        unsafe { *libc::__error() = 0 };
        // SAFETY: live descriptor; the returned ACL is freed below.
        let acl = unsafe { acl_get_fd(fd) };
        if acl.is_null() {
            let error = std::io::Error::last_os_error();
            let errno = error.raw_os_error().unwrap_or(0);
            // filesec_get_property reports ENOENT when the live inode simply has
            // no extended ACL. Other failures (including unsupported filesystems)
            // fail closed rather than pretending authorization is absent.
            ensure!(
                errno == 0 || errno == libc::ENOENT,
                "cannot inspect Graveyard ACL: {error}"
            );
            return Ok(());
        }
        let result = (|| -> Result<()> {
            let mut entry = ptr::null_mut();
            let mut entry_id = ACL_FIRST_ENTRY;
            loop {
                // SAFETY: acl is live and entry points to writable pointer storage.
                let rc = unsafe { acl_get_entry(acl, entry_id, &mut entry) };
                if rc != 0 {
                    let error = std::io::Error::last_os_error();
                    return if error.raw_os_error() == Some(libc::EINVAL) {
                        Ok(())
                    } else {
                        Err(error).context("cannot enumerate Graveyard ACL")
                    };
                }
                let mut tag = 0;
                // SAFETY: entry came from acl_get_entry and tag is writable.
                ensure!(
                    unsafe { acl_get_tag_type(entry, &mut tag) } == 0,
                    "cannot inspect Graveyard ACL entry"
                );
                if tag == ACL_EXTENDED_ALLOW {
                    let mut rights = 0;
                    // SAFETY: entry is live and rights is writable u64 storage.
                    ensure!(
                        unsafe { acl_get_permset_mask_np(entry, &mut rights) } == 0,
                        "cannot inspect Graveyard ACL permissions"
                    );
                    ensure!(
                        rights & MUTATION_RIGHTS == 0,
                        "extended ACL grants shared Graveyard mutation rights"
                    );
                }
                entry_id = ACL_NEXT_ENTRY;
            }
        })();
        // SAFETY: acl is the non-null allocation returned by acl_get_fd.
        let freed = unsafe { acl_free(acl) };
        ensure!(freed == 0, "cannot free inspected Graveyard ACL");
        result
    }

    fn rename_exclusive(
        old_dir: &File,
        old_name: &CStr,
        new_dir: &File,
        new_name: &CStr,
    ) -> std::io::Result<()> {
        let old_fd = old_dir.as_raw_fd();
        let new_fd: RawFd = new_dir.as_raw_fd();
        // SAFETY: both descriptors are live and all names are NUL-terminated.
        // Exclusive rename avoids exposing an intermediate double-linked inode.
        #[cfg(target_vendor = "apple")]
        let rc = unsafe {
            libc::renameatx_np(
                old_fd,
                old_name.as_ptr(),
                new_fd,
                new_name.as_ptr(),
                libc::RENAME_EXCL,
            )
        };
        #[cfg(any(target_os = "linux", target_os = "android", target_os = "freebsd"))]
        let rc = unsafe {
            libc::renameat2(
                old_fd,
                old_name.as_ptr(),
                new_fd,
                new_name.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        #[cfg(not(any(
            target_vendor = "apple",
            target_os = "linux",
            target_os = "android",
            target_os = "freebsd"
        )))]
        let rc = -1;
        if rc < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}
