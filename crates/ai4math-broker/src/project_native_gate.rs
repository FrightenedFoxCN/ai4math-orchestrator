use anyhow::Result;
use std::{
    fmt,
    fs::File,
    path::{Path, PathBuf},
};

/// Advisory OS lock for cooperating native-execution operators in one project.
///
/// All operators must leave `.runtime/native-execution.lock` in place. This gate neither
/// controls external processes and writes nor protects against same-user bypass.
/// When a coordinator dies abruptly, release of its lock does not certify that
/// any native children it launched were cleaned up.
#[derive(Clone, Debug)]
pub struct ProjectNativeGate {
    runtime: PathBuf,
}

/// Holds an advisory lock until dropped. Its descriptor is closed on exec and
/// is not exposed by the lease.
#[derive(Debug)]
#[must_use = "dropping the lease releases the project native lock"]
pub struct ProjectNativeLease {
    file: File,
}

#[cfg(unix)]
impl Drop for ProjectNativeLease {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;
        // A concurrent fork may have inherited this open file description
        // and not yet reached exec. Release the lock now, not when the last
        // transient copy is closed. The FD is also closed by File's drop.
        loop {
            // SAFETY: this guard owns the live descriptor of a successful flock.
            if unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) } == 0
                || std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted
            {
                break;
            }
        }
    }
}

/// A conflicting project native lease was held when acquisition was attempted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProjectNativeBusy;

impl fmt::Display for ProjectNativeBusy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("project native execution is busy")
    }
}

impl std::error::Error for ProjectNativeBusy {}

impl ProjectNativeGate {
    /// Record the project path without inspecting or modifying the filesystem.
    /// Prefer an absolute root to avoid dependence on the working directory.
    pub fn new(root: &Path) -> Self {
        Self {
            runtime: root.join(".runtime"),
        }
    }

    /// Try an exclusive lease without waiting for other holders.
    pub fn try_heavy(&self) -> Result<ProjectNativeLease> {
        self.try_lease(true)
    }

    /// Try a shared lease without waiting for other holders.
    pub fn try_fast(&self) -> Result<ProjectNativeLease> {
        self.try_lease(false)
    }

    #[cfg(unix)]
    fn try_lease(&self, exclusive: bool) -> Result<ProjectNativeLease> {
        unix::try_lease(&self.runtime, exclusive)
    }

    #[cfg(not(unix))]
    fn try_lease(&self, _exclusive: bool) -> Result<ProjectNativeLease> {
        anyhow::bail!(
            "project native execution locks are unsupported at {:?}",
            self.runtime
        )
    }
}

#[cfg(unix)]
mod unix {
    use super::{ProjectNativeBusy, ProjectNativeLease};
    use anyhow::{Context, Result, ensure};
    use std::{
        ffi::CStr,
        fs::{self, File, Metadata, OpenOptions},
        io,
        mem::MaybeUninit,
        os::{
            fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd},
            unix::fs::{MetadataExt, OpenOptionsExt},
        },
        path::Path,
    };

    const LOCK_NAME: &CStr = c"native-execution.lock";

    pub(super) fn try_lease(runtime: &Path, exclusive: bool) -> Result<ProjectNativeLease> {
        let dir = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY | libc::O_CLOEXEC)
            .open(runtime)
            .with_context(|| format!("open project runtime directory {}", runtime.display()))?;
        // SAFETY: geteuid has no arguments or memory requirements.
        let uid = unsafe { libc::geteuid() };
        let opened_dir = dir
            .metadata()
            .context("stat opened project runtime directory")?;
        validate_dir(&opened_dir, uid)?;

        // Reject already-visible special files before opening them. The open is
        // still nonblocking and no-follow, and its actual inode is rechecked.
        let previous = match named_lock(dir.as_fd()) {
            Ok(entry) => {
                validate_lock(&entry, uid)?;
                Some(entry)
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => None,
            Err(err) => return Err(err).context("stat project native lock entry"),
        };
        let file = open_lock(dir.as_fd()).context("open project native lock")?;
        let opened = opened_lock(file.as_fd()).context("stat opened project native lock")?;
        validate_lock(&opened, uid)?;
        if let Some(previous) = previous {
            ensure!(
                same_lock(&opened, &previous),
                "project native lock entry changed while opening"
            );
        }

        let operation = if exclusive {
            libc::LOCK_EX
        } else {
            libc::LOCK_SH
        };
        // SAFETY: file owns a valid descriptor, and the operation is a valid flock mask.
        let lock_result =
            if unsafe { libc::flock(file.as_raw_fd(), operation | libc::LOCK_NB) } == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            };

        // Compare the current entries even on contention so a structural
        // failure cannot be mistaken for a busy, otherwise healthy project.
        let named = named_lock(dir.as_fd()).context("recheck project native lock entry")?;
        validate_lock(&named, uid)?;
        ensure!(
            same_lock(&opened, &named),
            "project native lock entry changed while acquiring"
        );
        let named_dir =
            fs::symlink_metadata(runtime).context("recheck project runtime directory")?;
        validate_dir(&named_dir, uid)?;
        ensure!(
            opened_dir.dev() == named_dir.dev() && opened_dir.ino() == named_dir.ino(),
            "project runtime directory changed while acquiring native lock"
        );

        match lock_result {
            Ok(()) => Ok(ProjectNativeLease { file }),
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => Err(ProjectNativeBusy.into()),
            Err(err) => Err(err).context("flock project native lock"),
        }
    }

    fn validate_dir(dir: &Metadata, uid: libc::uid_t) -> Result<()> {
        ensure!(
            dir.is_dir() && dir.uid() == uid && dir.mode() & 0o022 == 0,
            "project .runtime must be a directory owned by the effective user and not writable by group or others"
        );
        Ok(())
    }

    fn validate_lock(file: &libc::stat, uid: libc::uid_t) -> Result<()> {
        ensure!(
            file.st_mode & libc::S_IFMT == libc::S_IFREG
                && file.st_uid == uid
                && file.st_nlink == 1
                && file.st_mode & 0o077 == 0,
            "project native lock must be a private, singly linked regular file owned by the effective user"
        );
        Ok(())
    }

    fn same_lock(opened: &libc::stat, named: &libc::stat) -> bool {
        opened.st_dev == named.st_dev && opened.st_ino == named.st_ino
    }

    fn opened_lock(file: BorrowedFd<'_>) -> io::Result<libc::stat> {
        let mut stat = MaybeUninit::<libc::stat>::uninit();
        // SAFETY: file is a live descriptor and stat is writable.
        if unsafe { libc::fstat(file.as_raw_fd(), stat.as_mut_ptr()) } == 0 {
            // SAFETY: a successful fstat initialized stat.
            Ok(unsafe { stat.assume_init() })
        } else {
            Err(io::Error::last_os_error())
        }
    }

    fn named_lock(dir: BorrowedFd<'_>) -> io::Result<libc::stat> {
        let mut stat = MaybeUninit::<libc::stat>::uninit();
        // SAFETY: dir is a live descriptor; LOCK_NAME is NUL-terminated; stat is writable.
        let rc = unsafe {
            libc::fstatat(
                dir.as_raw_fd(),
                LOCK_NAME.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if rc == 0 {
            // SAFETY: a successful fstatat initialized stat.
            Ok(unsafe { stat.assume_init() })
        } else {
            Err(io::Error::last_os_error())
        }
    }

    fn open_lock(dir: BorrowedFd<'_>) -> io::Result<File> {
        // SAFETY: dir is a live descriptor, LOCK_NAME is NUL-terminated, and
        // mode is passed at C variadic argument width (including on macOS).
        let fd = unsafe {
            libc::openat(
                dir.as_raw_fd(),
                LOCK_NAME.as_ptr(),
                libc::O_RDWR
                    | libc::O_NOFOLLOW
                    | libc::O_NONBLOCK
                    | libc::O_CLOEXEC
                    | libc::O_CREAT,
                0o600 as libc::c_uint,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful openat returned a new descriptor that File now owns.
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn ordinary_release_unlocks_even_with_a_transient_inherited_description() -> Result<()> {
        let root = tempfile::tempdir()?;
        std::fs::create_dir(root.path().join(".runtime"))?;
        let gate = ProjectNativeGate::new(root.path());
        let coordinator = gate.try_heavy()?;
        // A duplicate models the same open file description shared after
        // fork, before the new process reaches close-on-exec.
        let transient = coordinator.file.try_clone()?;
        drop(coordinator);
        let next = gate.try_heavy()?;
        drop(transient);
        assert!(gate.try_fast().unwrap_err().is::<ProjectNativeBusy>());
        drop(next);
        assert!(gate.try_fast().is_ok());
        Ok(())
    }
}
