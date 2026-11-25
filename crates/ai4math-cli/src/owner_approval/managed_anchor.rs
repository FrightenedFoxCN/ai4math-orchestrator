use super::MAX_KEY_BYTES;
use ai4math_store::retained::{open_retained_file, validate_artifact_acl};
use anyhow::{Context, Result, ensure};
use std::{
    ffi::{CStr, CString},
    fs::{File, Metadata},
    io::{self, Read},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{ffi::OsStrExt, fs::MetadataExt},
    },
    path::{Component, Path, PathBuf},
};

const RUNTIME: &CStr = c".runtime";
const OWNER_KEY: &CStr = c"owner-archive.pub";

pub(super) fn read(root_path: &Path) -> Result<Vec<u8>> {
    let root = open_root(root_path)?;
    let root_before = checked_dir_stamp(&root).context("validate managed root")?;
    let runtime = open_dir_at(&root, RUNTIME).context("open managed .runtime directory")?;
    let runtime_before =
        checked_dir_stamp(&runtime).context("validate managed .runtime directory")?;
    // Also checks regular type before open, no-follow, owner, one link, mode,
    // the bounded length and ACLs on the opened key.
    let mut key = open_retained_file(&runtime, OWNER_KEY, MAX_KEY_BYTES as u64)?;
    let key_before = Stamp::of(&key.metadata()?);
    let mut bytes = Vec::new();
    (&mut key)
        .take(MAX_KEY_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= MAX_KEY_BYTES && bytes.len() as u64 == key_before.len,
        "trusted Owner public key changed size during read"
    );

    // Rewalk the publicly named path (including the root) from scratch. Merely
    // using the pinned directory again would miss a renamed/replaced parent.
    let named_root = open_root(root_path)?;
    ensure!(
        checked_dir_stamp(&named_root)? == root_before,
        "managed root changed while reading trusted Owner public key"
    );
    let named_runtime = open_dir_at(&named_root, RUNTIME)?;
    ensure!(
        checked_dir_stamp(&named_runtime)? == runtime_before,
        "managed .runtime changed while reading trusted Owner public key"
    );
    let named_key = open_retained_file(&named_runtime, OWNER_KEY, MAX_KEY_BYTES as u64)?;
    ensure!(
        Stamp::of(&named_key.metadata()?) == key_before
            && Stamp::of(&key.metadata()?) == key_before,
        "trusted Owner public key changed during read"
    );
    // Also ensure no directory changed between its name recheck and key open.
    ensure!(
        checked_dir_stamp(&runtime)? == runtime_before && checked_dir_stamp(&root)? == root_before,
        "managed trust anchor directory changed during read"
    );
    Ok(bytes)
}

fn open_root(path: &Path) -> Result<File> {
    let mut components = path.components();
    let normalized: PathBuf = components.clone().collect();
    ensure!(
        path.as_os_str() == normalized.as_os_str()
            && components.next() == Some(Component::RootDir)
            && path.canonicalize()?.as_os_str() == path.as_os_str(),
        "managed root must be a canonical absolute path"
    );
    let mut dir = File::open("/").context("open filesystem root")?;
    // Do not follow symlinks even in ancestors of the canonical root: an
    // ancestor could have been replaced after canonicalization above.
    for component in components {
        let Component::Normal(name) = component else {
            anyhow::bail!("invalid managed root path component");
        };
        dir = open_dir_at(&dir, &CString::new(name.as_bytes())?)
            .context("open canonical managed root component")?;
    }
    Ok(dir)
}

fn open_dir_at(parent: &File, name: &CStr) -> io::Result<File> {
    // SAFETY: the parent is live and the basename is NUL-terminated.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY
                | libc::O_DIRECTORY
                | libc::O_NOFOLLOW
                | libc::O_NONBLOCK
                | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: success returned a fresh descriptor owned by this function.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn checked_dir_stamp(dir: &File) -> Result<Stamp> {
    let before = dir.metadata()?;
    // SAFETY: geteuid takes no arguments and has no memory preconditions.
    let uid = unsafe { libc::geteuid() };
    ensure!(
        before.is_dir() && before.uid() == uid && before.mode() & 0o022 == 0,
        "unsafe Owner trust anchor directory ownership or permissions"
    );
    validate_artifact_acl(dir)?;
    let stamp = Stamp::of(&before);
    ensure!(
        stamp == Stamp::of(&dir.metadata()?),
        "Owner trust anchor directory changed"
    );
    Ok(stamp)
}

#[derive(PartialEq, Eq)]
struct Stamp {
    device: u64,
    inode: u64,
    len: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
    uid: u32,
    gid: u32,
    mode: u32,
    links: u64,
}

impl Stamp {
    fn of(meta: &Metadata) -> Self {
        Self {
            device: meta.dev(),
            inode: meta.ino(),
            len: meta.len(),
            mtime: (meta.mtime(), meta.mtime_nsec()),
            ctime: (meta.ctime(), meta.ctime_nsec()),
            uid: meta.uid(),
            gid: meta.gid(),
            mode: meta.mode(),
            links: meta.nlink(),
        }
    }
}
