use anyhow::{Context, Result, ensure};
use std::{
    fs, io,
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::{Path, PathBuf},
};

pub(super) fn managed_state_path(root: &Path) -> Result<PathBuf> {
    let root = root.canonicalize().context("resolve project root")?;
    ensure!(root.is_dir(), "project root is not a directory");
    let runtime = root.join(".runtime");
    validate_runtime(&runtime)?;
    let state = runtime.join("state.sqlite");
    validate_state_file(&state)?;
    for name in [
        "state.sqlite-wal",
        "state.sqlite-shm",
        "state.sqlite-journal",
    ] {
        let path = runtime.join(name);
        match fs::symlink_metadata(&path) {
            Ok(_) => validate_state_file(&path)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).with_context(|| format!("inspect {}", path.display())),
        }
    }
    Ok(state)
}

fn validate_runtime(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect project runtime {}", path.display()))?;
    let uid = unsafe { libc::geteuid() };
    ensure!(
        metadata.is_dir()
            && !metadata.file_type().is_symlink()
            && metadata.uid() == uid
            && metadata.mode() & 0o7777 == 0o700,
        "project .runtime must be an owner-only .runtime directory with mode 0700"
    );
    reject_acl_grants(path)?;
    Ok(())
}

fn validate_state_file(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect managed state file {}", path.display()))?;
    let uid = unsafe { libc::geteuid() };
    ensure!(
        metadata.is_file()
            && !metadata.file_type().is_symlink()
            && metadata.uid() == uid
            && metadata.nlink() == 1
            && metadata.mode() & 0o022 == 0,
        "managed state files must be regular, singly linked, owner-controlled, and not writable by group or others"
    );
    reject_acl_grants(path)?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn reject_acl_grants(path: &Path) -> Result<()> {
    // Darwin extended ACL grants are not represented by group mode bits.
    // Deny-only ACLs grant no additional access and remain acceptable.
    use std::{ffi::CString, ptr};
    const ACL_TYPE_EXTENDED: libc::c_uint = 0x100;
    const ACL_EXTENDED_DENY: libc::c_uint = 2;
    const ACL_FIRST_ENTRY: libc::c_int = 0;
    const ACL_NEXT_ENTRY: libc::c_int = -1;

    unsafe extern "C" {
        fn acl_get_link_np(path: *const libc::c_char, ty: libc::c_uint) -> *mut libc::c_void;
        fn acl_get_entry(
            acl: *mut libc::c_void,
            index: libc::c_int,
            entry: *mut *mut libc::c_void,
        ) -> libc::c_int;
        fn acl_get_tag_type(entry: *mut libc::c_void, tag: *mut libc::c_uint) -> libc::c_int;
        fn acl_free(acl: *mut libc::c_void) -> libc::c_int;
    }

    let encoded = CString::new(path.as_os_str().as_bytes())?;
    // SAFETY: `encoded` is NUL-terminated and remains live for the call.
    let acl = unsafe { acl_get_link_np(encoded.as_ptr(), ACL_TYPE_EXTENDED) };
    if acl.is_null() {
        let error = io::Error::last_os_error();
        // ENOENT is Darwin's no-ACL result for an object known to exist.
        return match error.raw_os_error() {
            Some(libc::ENOENT | libc::ENOTSUP) => Ok(()),
            _ => Err(error.into()),
        };
    }
    let result = (|| -> Result<()> {
        let mut index = ACL_FIRST_ENTRY;
        loop {
            let mut entry = ptr::null_mut();
            // SAFETY: `acl` is live; `entry` is a writable output pointer.
            if unsafe { acl_get_entry(acl, index, &mut entry) } != 0 {
                let error = io::Error::last_os_error();
                // EINVAL marks the end of the entries in this ACL.
                return if error.raw_os_error() == Some(libc::EINVAL) {
                    Ok(())
                } else {
                    Err(error.into())
                };
            }
            let mut tag = 0;
            ensure!(
                unsafe { acl_get_tag_type(entry, &mut tag) } == 0 && tag == ACL_EXTENDED_DENY,
                "ACL grants are not allowed on managed project paths"
            );
            index = ACL_NEXT_ENTRY;
        }
    })();
    // SAFETY: the non-null ACL returned above is freed exactly once.
    unsafe { acl_free(acl) };
    result
}

#[cfg(not(target_os = "macos"))]
fn reject_acl_grants(_path: &Path) -> Result<()> {
    // POSIX ACL access is constrained by the group mode bits checked above.
    Ok(())
}
