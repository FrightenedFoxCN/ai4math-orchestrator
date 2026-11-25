//! Same extended ACL policy as the Graveyard archive publisher.
use anyhow::Result;
#[cfg(target_vendor = "apple")]
use anyhow::{Context, ensure};
use std::os::fd::RawFd;

/// POSIX ACL group-class permissions are bounded by the group mode mask on
/// Linux/FreeBSD, so the mode checks above also cover named-user/group ACLs.
/// macOS extended ACLs are evaluated before BSD mode fallback and can grant
/// write/delete while st_mode still looks private; inspect the live fd there.
#[cfg(not(target_vendor = "apple"))]
pub(crate) fn validate(_fd: RawFd) -> Result<()> {
    Ok(())
}

#[cfg(target_vendor = "apple")]
pub(crate) fn validate(fd: RawFd) -> Result<()> {
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
            "cannot inspect Wiki ACL: {error}"
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
                    Err(error).context("cannot enumerate Wiki ACL")
                };
            }
            let mut tag = 0;
            // SAFETY: entry came from acl_get_entry and tag is writable.
            ensure!(
                unsafe { acl_get_tag_type(entry, &mut tag) } == 0,
                "cannot inspect Wiki ACL entry"
            );
            if tag == ACL_EXTENDED_ALLOW {
                let mut rights = 0;
                // SAFETY: entry is live and rights is writable u64 storage.
                ensure!(
                    unsafe { acl_get_permset_mask_np(entry, &mut rights) } == 0,
                    "cannot inspect Wiki ACL permissions"
                );
                ensure!(
                    rights & MUTATION_RIGHTS == 0,
                    "extended ACL grants shared Wiki mutation rights"
                );
            }
            entry_id = ACL_NEXT_ENTRY;
        }
    })();
    // SAFETY: acl is the non-null allocation returned by acl_get_fd.
    let freed = unsafe { acl_free(acl) };
    ensure!(freed == 0, "cannot free inspected Wiki ACL");
    result
}
