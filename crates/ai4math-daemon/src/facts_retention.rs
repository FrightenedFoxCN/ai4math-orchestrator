use super::FactPack;
use ai4math_store::retained::{
    MAX_LEGACY_FILE_BYTES, MAX_PACK_BYTES, decode_retained, open_retained_dir, open_retained_file,
    validate_artifact_acl, verify_retained_dir,
};
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::{
    ffi::{CStr, CString},
    fs::{File, Metadata},
    io::{self, Read, Write},
    mem::MaybeUninit,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::MetadataExt,
    },
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

pub(super) fn retain(path: &Path, pack: &FactPack, markdown: &[u8]) -> Result<()> {
    let json = serialize_pack(pack)?;
    let id = &pack.fact_sha256;
    let value = decode_retained(id, &json).context("validate new fact pack before retaining")?;
    let json_name = CString::new(format!("{id}.json"))?;
    let md_name = CString::new(format!("{id}.md"))?;
    let dir = open_retained_dir(path)?;

    retain_once(
        path,
        &dir,
        &json_name,
        Artifact::Json {
            id,
            bytes: &json,
            value: &value,
        },
    )
    .context("retain authoritative JSON fact pack")?;
    // JSON is authoritative and durable at this point. Never roll it back because
    // of a summary error. Retrying re-verifies JSON and can then fill in the summary.
    retain_once(path, &dir, &md_name, Artifact::Markdown(markdown))
        .context("retain Markdown summary (authoritative JSON already retained)")
}

fn serialize_pack(pack: &FactPack) -> Result<Vec<u8>> {
    struct Bounded(Vec<u8>);
    impl Write for Bounded {
        fn write(&mut self, data: &[u8]) -> io::Result<usize> {
            if data.len() > MAX_PACK_BYTES - self.0.len() {
                return Err(io::Error::other("fact pack exceeds compact byte budget"));
            }
            self.0.extend_from_slice(data);
            Ok(data.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    let mut out = Bounded(Vec::new());
    serde_json::to_writer(&mut out, pack)?;
    Ok(out.0)
}

enum Artifact<'a> {
    Json {
        id: &'a str,
        bytes: &'a [u8],
        value: &'a Value,
    },
    Markdown(&'a [u8]),
}

impl Artifact<'_> {
    fn bytes(&self) -> &[u8] {
        match self {
            Self::Json { bytes, .. } | Self::Markdown(bytes) => bytes,
        }
    }

    fn disk_limit(&self) -> u64 {
        match self {
            Self::Json { .. } => MAX_LEGACY_FILE_BYTES,
            Self::Markdown(bytes) => bytes.len() as u64,
        }
    }

    fn matches(&self, actual: &[u8]) -> Result<bool> {
        match self {
            Self::Json { id, bytes, value } => {
                // New compact files use the exact bytes already verified above.
                // Legacy pretty files may differ on disk but not in canonical content.
                Ok(actual == *bytes || decode_retained(id, actual)? == **value)
            }
            Self::Markdown(bytes) => Ok(actual == *bytes),
        }
    }
}

fn retain_once(path: &Path, dir: &File, name: &CStr, expected: Artifact<'_>) -> Result<()> {
    if verify_existing(path, dir, name, &expected)? {
        return Ok(());
    }

    let mut staged = Staged::new(dir)?;
    staged.file.write_all(expected.bytes())?;
    staged
        .file
        .sync_all()
        .context("sync staged fact artifact")?;
    let before = staged.file.metadata()?;
    check_file(&staged.file, &before, expected.bytes().len() as u64)?;
    ensure!(
        before.len() == expected.bytes().len() as u64,
        "staged fact artifact length changed"
    );
    verify_retained_dir(path, dir)?;
    verify_named(dir, &staged.name, &before)?;

    match rename_exclusive(dir, &staged.name, name) {
        Ok(()) => {
            staged.published = true;
            let final_meta = staged.file.metadata()?;
            check_file(&staged.file, &final_meta, expected.bytes().len() as u64)?;
            ensure!(
                final_meta.len() == before.len() && final_meta.modified()? == before.modified()?,
                "staged fact artifact changed during publication"
            );
            sync_verified(path, dir, name, &staged.file, &final_meta)
        }
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
            drop(staged);
            ensure!(
                verify_existing(path, dir, name, &expected)?,
                "retained fact artifact disappeared after publication conflict"
            );
            Ok(())
        }
        Err(err) => Err(err).context("publish retained fact artifact without replacing"),
    }
}

fn verify_existing(path: &Path, dir: &File, name: &CStr, expected: &Artifact<'_>) -> Result<bool> {
    let limit = expected.disk_limit();
    let mut file = match open_retained_file(dir, name, limit) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err).context("open existing retained fact artifact"),
    };
    let before = file.metadata()?;
    check_file(&file, &before, limit)?;
    let mut actual = Vec::new();
    (&mut file).take(limit + 1).read_to_end(&mut actual)?;
    ensure!(
        actual.len() as u64 <= limit,
        "retained fact artifact exceeds disk byte budget"
    );
    ensure!(
        actual.len() as u64 == before.len(),
        "retained fact artifact length changed while reading"
    );
    ensure!(
        expected.matches(&actual)?,
        "retained fact artifact mismatch"
    );
    // A prior attempt may have published before a failed directory sync. Re-sync
    // both the verified file and the directory on the already-present path, too.
    file.sync_all()
        .context("sync matching retained fact artifact")?;
    sync_verified(path, dir, name, &file, &before)?;
    Ok(true)
}

fn check_file(file: &File, meta: &Metadata, limit: u64) -> Result<()> {
    // SAFETY: geteuid has no preconditions.
    let uid = unsafe { libc::geteuid() };
    ensure!(
        meta.is_file() && meta.uid() == uid && meta.nlink() == 1 && meta.mode() & 0o022 == 0,
        "unsafe retained fact artifact"
    );
    ensure!(
        meta.len() <= limit,
        "retained fact artifact exceeds disk byte budget"
    );
    validate_artifact_acl(file)
}

fn sync_verified(
    path: &Path,
    dir: &File,
    name: &CStr,
    file: &File,
    before: &Metadata,
) -> Result<()> {
    verify_named(dir, name, before)?;
    dir.sync_all()
        .context("sync retained fact pack directory")?;
    let after = file.metadata()?;
    check_file(file, &after, before.len())?;
    ensure!(
        before.len() == after.len()
            && before.mode() == after.mode()
            && (before.mtime(), before.mtime_nsec()) == (after.mtime(), after.mtime_nsec())
            && (before.ctime(), before.ctime_nsec()) == (after.ctime(), after.ctime_nsec()),
        "retained fact artifact changed during verification"
    );
    verify_named(dir, name, &after)?;
    verify_retained_dir(path, dir)
}

fn verify_named(dir: &File, name: &CStr, meta: &Metadata) -> Result<()> {
    let mut named = MaybeUninit::<libc::stat>::uninit();
    // SAFETY: the directory fd, NUL-terminated name and output storage are live.
    let result = unsafe {
        libc::fstatat(
            dir.as_raw_fd(),
            name.as_ptr(),
            named.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result < 0 {
        return Err(io::Error::last_os_error()).context("inspect named retained fact artifact");
    }
    // SAFETY: successful fstatat initialized named.
    let named = unsafe { named.assume_init() };
    ensure!(
        named.st_dev == meta.dev() as libc::dev_t && named.st_ino == meta.ino() as libc::ino_t,
        "retained fact artifact identity no longer matches its name"
    );
    Ok(())
}

struct Staged<'a> {
    dir: &'a File,
    name: CString,
    file: File,
    published: bool,
}

impl<'a> Staged<'a> {
    fn new(dir: &'a File) -> Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        for _ in 0..128 {
            let nonce = NEXT.fetch_add(1, Ordering::Relaxed);
            let name = CString::new(format!(
                ".fact-{}-{time:x}-{nonce:x}.tmp",
                std::process::id()
            ))?;
            // SAFETY: the descriptor and NUL-terminated name are live; C variadic mode uses int width.
            let fd = unsafe {
                libc::openat(
                    dir.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_WRONLY
                        | libc::O_CREAT
                        | libc::O_EXCL
                        | libc::O_NOFOLLOW
                        | libc::O_NONBLOCK
                        | libc::O_CLOEXEC,
                    0o600 as libc::c_uint,
                )
            };
            if fd < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::AlreadyExists {
                    continue;
                }
                return Err(err).context("create staged retained fact artifact");
            }
            // SAFETY: successful openat returns a new owned file descriptor.
            let file = unsafe { File::from_raw_fd(fd) };
            let staged = Self {
                dir,
                name,
                file,
                published: false,
            };
            check_file(&staged.file, &staged.file.metadata()?, 0)?;
            return Ok(staged);
        }
        anyhow::bail!("could not allocate a staging name for retained fact artifact")
    }
}

impl Drop for Staged<'_> {
    fn drop(&mut self) {
        if !self.published
            && self
                .file
                .metadata()
                .is_ok_and(|meta| verify_named(self.dir, &self.name, &meta).is_ok())
        {
            // SAFETY: the descriptor and NUL-terminated name are live. Remove only staging names.
            unsafe { libc::unlinkat(self.dir.as_raw_fd(), self.name.as_ptr(), 0) };
        }
    }
}

fn rename_exclusive(dir: &File, from: &CStr, to: &CStr) -> io::Result<()> {
    let fd = dir.as_raw_fd();
    // SAFETY: fd and names are live. Exclusive rename has no double-link interval
    // (unlike a link/unlink fallback) and does not replace a concurrent publication.
    #[cfg(target_vendor = "apple")]
    let result =
        unsafe { libc::renameatx_np(fd, from.as_ptr(), fd, to.as_ptr(), libc::RENAME_EXCL) };
    #[cfg(any(target_os = "linux", target_os = "android", target_os = "freebsd"))]
    let result =
        unsafe { libc::renameat2(fd, from.as_ptr(), fd, to.as_ptr(), libc::RENAME_NOREPLACE) };
    #[cfg(not(any(
        target_vendor = "apple",
        target_os = "linux",
        target_os = "android",
        target_os = "freebsd"
    )))]
    return Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "atomic exclusive rename unavailable",
    ));
    #[cfg(any(
        target_vendor = "apple",
        target_os = "linux",
        target_os = "android",
        target_os = "freebsd"
    ))]
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}
