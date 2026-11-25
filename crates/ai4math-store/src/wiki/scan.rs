//! Bounded, descriptor-relative Wiki tree reads. Two observations detect ordinary
//! filesystem drift but do not constitute a simultaneous snapshot of the tree.
use super::{STRATA, WikiStratum, sediment};
use anyhow::{Context, Result, ensure};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::{CStr, CString, OsString},
    fs::{File, OpenOptions},
    io::{self, Read},
    mem::MaybeUninit,
    os::{
        fd::{AsRawFd, IntoRawFd},
        unix::{ffi::OsStringExt, fs::OpenOptionsExt},
    },
    path::{Component, Path, PathBuf},
};

const MAX_SCAN_ENTRIES: usize = 16_384;
const MAX_MARKDOWN_FILES: usize = 4_096;
// Keep the same per-file allowance as the generated Sediment archives.
const MAX_FILE_BYTES: u64 = sediment::MAX_ARCHIVE_BYTES;
const MAX_TOTAL_BYTES: u64 = 64 * 1024 * 1024;
const MAX_DIR_DEPTH: usize = 64;
const O_SAFE_FILE: libc::c_int = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK;
const O_SAFE_DIR: libc::c_int = O_SAFE_FILE | libc::O_DIRECTORY;

pub(super) fn stage(
    root: &Path,
    mut stage_entry: impl FnMut(WikiStratum, &Path, &str) -> Result<()>,
) -> Result<()> {
    // Strip trailing `/` and `/.`, which could otherwise defeat O_NOFOLLOW on
    // the final name. The caller supplies the root's path prefix (e.g. /tmp).
    let root: PathBuf = root.components().collect();
    ensure!(
        !matches!(root.components().next_back(), Some(Component::ParentDir)),
        "Wiki root must not end in '..'"
    );
    let mut bytes_left = MAX_TOTAL_BYTES;
    let mut manifest = BTreeMap::new();
    scan(&root, |stratum, path, file, stamp| {
        ensure!(
            manifest.insert(path.to_owned(), stamp).is_none(),
            "duplicate Wiki path while scanning"
        );
        if let Node::Markdown(mut file) = file {
            let size = u64::try_from(stamp.0.st_size).context("invalid Wiki Markdown size")?;
            let budget = bytes_left.min(MAX_FILE_BYTES);
            ensure!(
                size <= MAX_FILE_BYTES,
                "Wiki Markdown exceeds the {MAX_FILE_BYTES}-byte file limit"
            );
            ensure!(
                size <= bytes_left,
                "Wiki Markdown exceeds the {MAX_TOTAL_BYTES}-byte total limit"
            );
            let mut content = Vec::with_capacity(size as usize);
            (&mut file).take(budget + 1).read_to_end(&mut content)?;
            ensure!(
                content.len() as u64 <= MAX_FILE_BYTES,
                "Wiki Markdown exceeds the {MAX_FILE_BYTES}-byte file limit"
            );
            ensure!(
                content.len() as u64 <= bytes_left,
                "Wiki Markdown exceeds the {MAX_TOTAL_BYTES}-byte total limit"
            );
            ensure!(
                content.len() as u64 == size && fd_stamp(&file)? == stamp,
                "Wiki Markdown changed while reading"
            );
            bytes_left -= content.len() as u64;
            stage_entry(stratum, path, std::str::from_utf8(&content)?)?;
        }
        Ok(())
    })?;

    // Recheck all discovered directories and Markdown after the last parse to
    // detect additions, removals, same-size edits, and directory/file replacement.
    // The second bounded scan never reads file contents.
    scan(&root, |_, path, _, stamp| {
        ensure!(
            manifest.remove(path) == Some(stamp),
            "Wiki path changed between scans: {}",
            root.join(path).display()
        );
        Ok(())
    })?;
    ensure!(manifest.is_empty(), "Wiki paths disappeared between scans");
    Ok(())
}

#[derive(Clone, Copy)]
struct Stamp(libc::stat);

impl Stamp {
    fn identity(self) -> (libc::dev_t, libc::ino_t) {
        (self.0.st_dev, self.0.st_ino)
    }

    fn kind(self) -> libc::mode_t {
        self.0.st_mode & libc::S_IFMT
    }
}

impl PartialEq for Stamp {
    fn eq(&self, other: &Self) -> bool {
        let (a, b) = (&self.0, &other.0);
        self.identity() == other.identity()
            && a.st_mode == b.st_mode
            && a.st_uid == b.st_uid
            && a.st_gid == b.st_gid
            && a.st_nlink == b.st_nlink
            && a.st_size == b.st_size
            && a.st_mtime == b.st_mtime
            && a.st_mtime_nsec == b.st_mtime_nsec
            && a.st_ctime == b.st_ctime
            && a.st_ctime_nsec == b.st_ctime_nsec
    }
}

fn fd_stamp(file: &File) -> io::Result<Stamp> {
    let mut stat = MaybeUninit::uninit();
    // SAFETY: file's descriptor is live; stat points to writable storage.
    if unsafe { libc::fstat(file.as_raw_fd(), stat.as_mut_ptr()) } < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful fstat initialized stat.
    Ok(Stamp(unsafe { stat.assume_init() }))
}

fn root_dir(root: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(O_SAFE_DIR)
        .open(root)
}

enum Node {
    Directory,
    Markdown(File),
}

fn scan(
    root: &Path,
    mut visit: impl FnMut(WikiStratum, &Path, Node, Stamp) -> Result<()>,
) -> Result<()> {
    // SAFETY: geteuid has no preconditions.
    let uid = unsafe { libc::geteuid() };
    let mut walker = Walker {
        uid,
        entries: 0,
        files: 0,
        identities: BTreeSet::new(),
        visit: &mut visit,
    };
    (|| -> Result<()> {
        let dir = root_dir(root).context("open Wiki root without following links")?;
        let stamp = fd_stamp(&dir)?;
        walker.validate(stamp, libc::S_IFDIR, &dir)?;
        // The root is not enumerated: only known strata are inputs. Check its
        // full stamp conservatively, including changes to its directory entries.
        (walker.visit)(WikiStratum::Core, Path::new(""), Node::Directory, stamp)?;
        for stratum in STRATA {
            let path = Path::new(stratum.as_str());
            let name = CString::new(stratum.as_str())?;
            match sediment::stat_at(&dir, &name) {
                Ok(stat) => walker.directory(stratum, path, &dir, &name, Stamp(stat), 1)?,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error).context("inspect Wiki stratum"),
            }
        }
        let named = root_dir(root).context("reopen Wiki root after scan")?;
        ensure!(
            fd_stamp(&named)? == stamp,
            "Wiki root path or directory contents changed during scan"
        );
        Ok(())
    })()
    .with_context(|| format!("scan Wiki root {}", root.display()))
}

struct Walker<'a, F> {
    uid: libc::uid_t,
    entries: usize,
    files: usize,
    identities: BTreeSet<(libc::dev_t, libc::ino_t)>,
    visit: &'a mut F,
}

impl<F: FnMut(WikiStratum, &Path, Node, Stamp) -> Result<()>> Walker<'_, F> {
    fn validate(&mut self, stamp: Stamp, kind: libc::mode_t, file: &File) -> Result<()> {
        validate_named(stamp, kind, self.uid)?;
        ensure!(
            self.identities.insert(stamp.identity()),
            "aliased Wiki file or directory"
        );
        super::scan_acl::validate(file.as_raw_fd())
    }

    fn directory(
        &mut self,
        stratum: WikiStratum,
        path: &Path,
        parent: &File,
        name: &CStr,
        stamp: Stamp,
        depth: usize,
    ) -> Result<()> {
        (|| -> Result<()> {
            ensure!(
                depth <= MAX_DIR_DEPTH,
                "Wiki directory depth exceeds {MAX_DIR_DEPTH}"
            );
            validate_named(stamp, libc::S_IFDIR, self.uid)?;
            let dir = sediment::open_at(parent, name, O_SAFE_DIR)?;
            ensure!(
                fd_stamp(&dir)? == stamp,
                "Wiki directory changed while opening"
            );
            self.validate(stamp, libc::S_IFDIR, &dir)?;
            (self.visit)(stratum, path, Node::Directory, stamp)?;
            for child in sorted_names(&dir, &mut self.entries)? {
                let path = path.join(OsString::from_vec(child.to_bytes().to_vec()));
                let stamp = Stamp(sediment::stat_at(&dir, &child)?);
                if stamp.kind() == libc::S_IFDIR {
                    self.directory(stratum, &path, &dir, &child, stamp, depth + 1)?;
                } else if path
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
                {
                    self.markdown(stratum, &path, &dir, &child, stamp)?;
                } else {
                    ensure!(
                        stamp.kind() != libc::S_IFLNK,
                        "Wiki directory entry is a symlink: {}",
                        path.display()
                    );
                }
            }
            ensure!(
                fd_stamp(&dir)? == stamp && Stamp(sediment::stat_at(parent, name)?) == stamp,
                "Wiki directory changed during scan"
            );
            Ok(())
        })()
        .with_context(|| format!("scan Wiki directory {}", path.display()))
    }

    fn markdown(
        &mut self,
        stratum: WikiStratum,
        path: &Path,
        dir: &File,
        name: &CStr,
        stamp: Stamp,
    ) -> Result<()> {
        (|| -> Result<()> {
            self.files += 1;
            ensure!(
                self.files <= MAX_MARKDOWN_FILES,
                "Wiki scan exceeds the {MAX_MARKDOWN_FILES}-Markdown-file limit"
            );
            // Reject named special inodes and hard links before opening; verify
            // the descriptor again before reading, so a name swap is not trusted.
            validate_named(stamp, libc::S_IFREG, self.uid)?;
            let file = sediment::open_at(dir, name, O_SAFE_FILE)?;
            ensure!(
                fd_stamp(&file)? == stamp,
                "Wiki Markdown changed while opening"
            );
            self.validate(stamp, libc::S_IFREG, &file)?;
            (self.visit)(stratum, path, Node::Markdown(file), stamp)?;
            ensure!(
                Stamp(sediment::stat_at(dir, name)?) == stamp,
                "Wiki Markdown path changed during scan"
            );
            Ok(())
        })()
        .with_context(|| format!("index Wiki Markdown {}", path.display()))
    }
}

fn validate_named(stamp: Stamp, kind: libc::mode_t, uid: libc::uid_t) -> Result<()> {
    ensure!(
        stamp.kind() == kind,
        "Wiki path has an unsafe file type (expected a regular file or directory, never a symlink)"
    );
    ensure!(
        stamp.0.st_uid == uid && stamp.0.st_mode & 0o022 == 0,
        "Wiki path must be owned by the effective user and not writable by group or others"
    );
    ensure!(
        kind != libc::S_IFREG || stamp.0.st_nlink == 1,
        "Wiki Markdown must have exactly one hard link"
    );
    Ok(())
}

struct DirStream(*mut libc::DIR);

impl Drop for DirStream {
    fn drop(&mut self) {
        // SAFETY: this stream is uniquely owned and fdopendir succeeded.
        unsafe { libc::closedir(self.0) };
    }
}

fn sorted_names(dir: &File, entries: &mut usize) -> Result<Vec<CString>> {
    // Open `.` relative to the pinned inode for an independent directory
    // offset. POSIX transfers ownership of this fd on successful fdopendir.
    let stream_fd = sediment::open_at(dir, c".", O_SAFE_DIR)?;
    // SAFETY: live directory fd. The stream takes it only on success.
    let raw = unsafe { libc::fdopendir(stream_fd.as_raw_fd()) };
    if raw.is_null() {
        return Err(io::Error::last_os_error()).context("open Wiki directory stream");
    }
    let _ = stream_fd.into_raw_fd();
    let stream = DirStream(raw);
    let mut names = Vec::new();
    loop {
        // SAFETY: readdir uses this thread's errno cell; the stream is live and
        // not shared. A null pointer with zero errno indicates EOF, not failure.
        let entry = unsafe {
            *errno_ptr() = 0;
            libc::readdir(stream.0)
        };
        if entry.is_null() {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(0) {
                return Err(error).context("read Wiki directory stream");
            }
            break;
        }
        // SAFETY: readdir provides a NUL-terminated name valid until the next
        // readdir or closedir. We copy it before advancing the stream.
        let name = unsafe { CStr::from_ptr(std::ptr::addr_of!((*entry).d_name).cast()) };
        if name == c"." || name == c".." {
            continue;
        }
        *entries += 1;
        ensure!(
            *entries <= MAX_SCAN_ENTRIES,
            "Wiki scan exceeds the {MAX_SCAN_ENTRIES}-directory-entry limit"
        );
        names.push(name.to_owned());
    }
    names.sort_unstable();
    ensure!(
        names.windows(2).all(|pair| pair[0] != pair[1]),
        "duplicate name while reading Wiki directory stream"
    );
    Ok(names)
}

#[cfg(target_os = "android")]
use libc::__errno as errno_ptr;
#[cfg(target_os = "linux")]
use libc::__errno_location as errno_ptr;

#[cfg(any(target_vendor = "apple", target_os = "freebsd"))]
use libc::__error as errno_ptr;

#[cfg(test)]
mod tests {
    use super::stage;
    use anyhow::Result;
    use std::{fs, path::Path, time::SystemTime};

    #[test]
    fn wiki_detects_root_replacement_while_reading_from_pinned_directory() -> Result<()> {
        let scratch = tempfile::tempdir()?;
        let root = scratch.path().join("wiki");
        fs::create_dir_all(root.join("core"))?;
        fs::write(root.join("core/a.md"), "before")?;
        let error = stage(&root, |_, _, _| {
            fs::rename(&root, scratch.path().join("detached"))?;
            fs::create_dir_all(root.join("core"))?;
            fs::write(root.join("core/a.md"), "newdir")?;
            Ok(())
        })
        .unwrap_err();
        assert!(format!("{error:#}").contains("Wiki root path or directory contents changed"));
        Ok(())
    }

    #[test]
    fn wiki_detects_same_size_edit_after_reading_an_earlier_file() -> Result<()> {
        let root = tempfile::tempdir()?;
        let root = root.path();
        fs::create_dir_all(root.join("core/nested"))?;
        let early = root.join("core/nested/a.md");
        fs::write(&early, "first!")?;
        fs::write(root.join("core/z.md"), "second")?;
        let error = stage(root, |_, path, _| {
            if path == Path::new("core/z.md") {
                fs::write(&early, "edited")?;
                // Independent of filesystem clock resolution and runtime speed.
                fs::File::open(&early)?.set_modified(SystemTime::UNIX_EPOCH)?;
            }
            Ok(())
        })
        .unwrap_err();
        assert!(format!("{error:#}").contains("changed between scans"));
        assert!(format!("{error:#}").contains("core/nested/a.md"));
        Ok(())
    }

    #[test]
    fn wiki_detects_directory_swap_after_earlier_subtree_was_read() -> Result<()> {
        let scratch = tempfile::tempdir()?;
        let root = scratch.path().join("wiki");
        let old = root.join("core/nested");
        let standby = scratch.path().join("standby");
        fs::create_dir_all(&old)?;
        fs::create_dir_all(root.join("sediment"))?;
        fs::create_dir(&standby)?;
        fs::write(old.join("a.md"), "original")?;
        fs::write(standby.join("a.md"), "swapped!")?;
        fs::write(root.join("sediment/z.md"), "second")?;
        let error = stage(&root, |_, path, _| {
            if path == Path::new("sediment/z.md") {
                fs::rename(&old, scratch.path().join("detached"))?;
                fs::rename(&standby, &old)?;
                // Force a distinguishable parent stamp even on coarse clocks.
                fs::File::open(root.join("core"))?.set_modified(SystemTime::UNIX_EPOCH)?;
            }
            Ok(())
        })
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("changed between scans"),
            "{error:#}"
        );
        Ok(())
    }
}
