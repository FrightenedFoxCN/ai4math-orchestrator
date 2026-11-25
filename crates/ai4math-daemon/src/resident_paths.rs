//! Trusted filesystem prerequisites shared by resident operator workers.
use anyhow::{Result, ensure};
use serde::de::DeserializeOwned;
use std::{
    fs::{self, Metadata, OpenOptions},
    io::{self, Read},
    os::unix::{
        ffi::OsStrExt,
        fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
};

const MAX_CONFIG_BYTES: u64 = 16 * 1024;

pub(super) fn trusted_directory(path: &Path) -> Result<()> {
    ensure!(
        trusted_metadata(path)?.is_dir(),
        "expected a trusted directory"
    );
    Ok(())
}

pub(super) fn trusted_metadata(path: &Path) -> Result<Metadata> {
    ensure!(
        path.is_absolute()
            && path
                .to_str()
                .is_some_and(|s| !s.chars().any(char::is_control)),
        "expected an absolute UTF-8 path"
    );
    // SAFETY: geteuid takes no arguments.
    let uid = unsafe { libc::geteuid() };
    let ancestors: Vec<_> = path.ancestors().skip(1).collect();
    for dir in ancestors.into_iter().rev() {
        let meta = fs::symlink_metadata(dir)?;
        // System parents can be root-owned. Sticky, root-owned shared parents (e.g. /tmp)
        // cannot be used to replace other owners' entries; every descendant is checked too.
        ensure!(
            meta.is_dir()
                && (meta.uid() == uid || meta.uid() == 0)
                && (meta.mode() & 0o022 == 0 || (meta.uid() == 0 && meta.mode() & 0o1000 != 0)),
            "untrusted parent directory"
        );
        reject_acl_grants(dir)?;
    }
    let meta = fs::symlink_metadata(path)?;
    ensure!(
        !meta.file_type().is_symlink() && meta.uid() == uid && meta.mode() & 0o022 == 0,
        "untrusted path"
    );
    // Byte equality also rejects alternate spellings that Path's equality normalizes.
    ensure!(
        path.canonicalize()?.as_os_str().as_bytes() == path.as_os_str().as_bytes(),
        "expected a canonical path"
    );
    reject_acl_grants(path)?;
    Ok(meta)
}

pub(super) fn read_trusted_config<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let expected = trusted_metadata(path)?;
    check_config_file(&expected)?;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let opened = file.metadata()?;
    check_config_file(&opened)?;
    ensure!(
        (expected.dev(), expected.ino()) == (opened.dev(), opened.ino()),
        "configuration replaced while opening"
    );
    let mut bytes = Vec::new();
    file.take(MAX_CONFIG_BYTES + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_CONFIG_BYTES,
        "configuration too large"
    );
    Ok(serde_json::from_slice(&bytes)?)
}

pub(super) fn trusted_executable(path: &Path) -> Result<()> {
    let meta = trusted_metadata(path)?;
    ensure!(
        meta.is_file() && meta.nlink() == 1 && meta.mode() & 0o7100 == 0o100,
        "expected an ordinary owner-executable file"
    );
    Ok(())
}

pub(super) fn trusted_state_path(root: &Path) -> Result<PathBuf> {
    trusted_directory(root)?;
    let runtime = root.join(".runtime");
    trusted_directory(&runtime)?;
    let path = runtime.join("state.sqlite");
    check_state_file(&trusted_metadata(&path)?)?;
    for name in [
        "state.sqlite-wal",
        "state.sqlite-shm",
        "state.sqlite-journal",
    ] {
        let path = runtime.join(name);
        match fs::symlink_metadata(&path) {
            Ok(meta) => {
                check_state_file(&meta)?;
                reject_acl_grants(&path)?;
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
    }
    Ok(path)
}

fn check_config_file(meta: &Metadata) -> Result<()> {
    ensure!(
        meta.is_file()
            && meta.nlink() == 1
            && meta.mode() & 0o7177 == 0
            && meta.mode() & 0o400 != 0
            && meta.len() <= MAX_CONFIG_BYTES,
        "expected bounded owner-only configuration"
    );
    Ok(())
}

pub(super) fn check_state_file(meta: &Metadata) -> Result<()> {
    // SAFETY: geteuid takes no arguments.
    let uid = unsafe { libc::geteuid() };
    ensure!(
        meta.is_file() && meta.nlink() == 1 && meta.uid() == uid && meta.mode() & 0o022 == 0,
        "unsafe state file"
    );
    Ok(())
}

#[cfg(target_os = "macos")]
fn reject_acl_grants(path: &Path) -> Result<()> {
    // Darwin's extended ACL grants need not be reflected in group mode bits. Deny-only
    // ACLs (including the standard home-directory deny-delete entry) grant no new access.
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
    // SAFETY: encoded is NUL-terminated and lives for the call; the result is owned below.
    let acl = unsafe { acl_get_link_np(encoded.as_ptr(), ACL_TYPE_EXTENDED) };
    if acl.is_null() {
        let error = io::Error::last_os_error();
        // ENOENT is also Darwin's no-ACL result for an object known to exist.
        return match error.raw_os_error() {
            Some(libc::ENOENT | libc::ENOTSUP) => Ok(()),
            _ => Err(error.into()),
        };
    }
    let result = (|| -> Result<()> {
        let mut index = ACL_FIRST_ENTRY;
        loop {
            let mut entry = ptr::null_mut();
            // SAFETY: acl remains live; entry and tag are writable, correctly typed outputs.
            if unsafe { acl_get_entry(acl, index, &mut entry) } != 0 {
                let error = io::Error::last_os_error();
                // EINVAL for an out-of-range index is the end of an ACL obtained above.
                return if error.raw_os_error() == Some(libc::EINVAL) {
                    Ok(())
                } else {
                    Err(error.into())
                };
            }
            let mut tag = 0;
            ensure!(
                unsafe { acl_get_tag_type(entry, &mut tag) } == 0 && tag == ACL_EXTENDED_DENY,
                "ACL grants are not allowed on trusted paths"
            );
            index = ACL_NEXT_ENTRY;
        }
    })();
    // SAFETY: acl_get_link_np returned a non-null ACL that is freed just once.
    unsafe { acl_free(acl) };
    result
}

#[cfg(not(target_os = "macos"))]
fn reject_acl_grants(_path: &Path) -> Result<()> {
    // POSIX ACL grants are constrained by the group mode bits checked above.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    #[test]
    fn config_rejects_links_and_nonprivate_permissions() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().canonicalize()?;
        let config = root.join("resident.json");
        fs::write(&config, b"{\"version\":1}")?;
        fs::set_permissions(&config, fs::Permissions::from_mode(0o600))?;
        assert_eq!(
            read_trusted_config::<serde_json::Value>(&config)?["version"],
            1
        );
        let link = root.join("linked.json");
        symlink(&config, &link)?;
        assert!(read_trusted_config::<serde_json::Value>(&link).is_err());
        fs::set_permissions(&config, fs::Permissions::from_mode(0o644))?;
        assert!(read_trusted_config::<serde_json::Value>(&config).is_err());
        Ok(())
    }
}
