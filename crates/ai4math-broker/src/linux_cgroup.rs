//! Read-only cgroup v2 leaf diagnostics. This is neither a delegation check nor
//! a backend launcher. The controls are sampled separately, not atomically.

use serde::Serialize;
use std::{fmt, path::Path};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct LinuxCgroupReadiness {
    pub memory_max_bytes: u64,
    pub pids_max: u64,
    pub cpu_quota_us: u64,
    pub cpu_period_us: u64,
    /// Presence only; absence is allowed, and presence does not imply permission.
    pub cgroup_kill_present: bool,
}

/// Bounded diagnostics: no supplied path, control contents, or OS error strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LinuxCgroupError {
    UnsupportedPlatform,
    InvalidPath,
    Openat2Unavailable,
    NotCgroup2,
    Io {
        operation: &'static str,
        control: Option<&'static str>,
        errno: Option<i32>,
    },
    NotRegularControl {
        control: &'static str,
    },
    OversizeControl {
        control: &'static str,
    },
    InvalidControl {
        control: &'static str,
    },
    Unlimited {
        control: &'static str,
    },
    OutOfRange {
        control: &'static str,
    },
    NotDomain,
    Populated,
    Frozen,
    HasDescendants,
}

impl fmt::Display for LinuxCgroupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedPlatform => f.write_str("cgroup v2 preflight requires Linux"),
            Self::InvalidPath => f.write_str("expected a canonical absolute cgroup path"),
            Self::Openat2Unavailable => f.write_str("cgroup v2 preflight requires openat2"),
            Self::NotCgroup2 => f.write_str("not on cgroup v2"),
            Self::Io {
                operation,
                control,
                errno,
            } => {
                write!(f, "cgroup {operation} failed")?;
                if let Some(control) = control {
                    write!(f, " for {control}")?;
                }
                if let Some(errno) = errno {
                    write!(f, " (errno {errno})")?;
                }
                Ok(())
            }
            Self::NotRegularControl { control } => write!(f, "{control} is not a regular control"),
            Self::OversizeControl { control } => write!(f, "{control} exceeds read cap"),
            Self::InvalidControl { control } => write!(f, "invalid {control} content"),
            Self::Unlimited { control } => write!(f, "{control} is unlimited"),
            Self::OutOfRange { control } => write!(f, "{control} is out of range"),
            Self::NotDomain => f.write_str("cgroup is not a plain domain"),
            Self::Populated => f.write_str("cgroup is populated"),
            Self::Frozen => f.write_str("cgroup is frozen"),
            Self::HasDescendants => f.write_str("cgroup is not a leaf"),
        }
    }
}

impl std::error::Error for LinuxCgroupError {}

type Result<T> = std::result::Result<T, LinuxCgroupError>;

/// Inspect the explicitly supplied path without writing or starting anything.
///
/// Linux must support `openat2` (introduced in 5.6); if unavailable, this fails
/// closed. The path must already be canonical: no symlinks, empty components,
/// dot components, or trailing slash except `/`. There is no path discovery.
/// Success does not attest delegation, writability, effective ancestor limits,
/// swap limits, or that the hierarchy will remain in the observed state.
pub fn preflight(path: &Path) -> Result<LinuxCgroupReadiness> {
    #[cfg(target_os = "linux")]
    return linux::preflight(path);

    #[cfg(not(target_os = "linux"))]
    {
        let _ = path;
        Err(LinuxCgroupError::UnsupportedPlatform)
    }
}

#[cfg(any(target_os = "linux", test))]
mod checks {
    use super::{LinuxCgroupError as E, Path, Result};
    use std::{collections::BTreeMap, io::Read};

    pub(super) const TYPE: &str = "cgroup.type";
    pub(super) const EVENTS: &str = "cgroup.events";
    pub(super) const STAT: &str = "cgroup.stat";
    pub(super) const MEMORY: &str = "memory.max";
    pub(super) const PIDS: &str = "pids.max";
    pub(super) const CPU: &str = "cpu.max";
    pub(super) const READ_CAP: usize = 4096;
    // Linux's pathname syscall buffer includes the terminating NUL.
    const PATH_MAX_BYTES: usize = 4095;
    // Sanity ceilings, not a resource policy. Linux limits the PID namespace
    // and CFS quota; memory's page counter is bounded by signed long.
    const MEMORY_MAX_BYTES: u64 = i64::MAX as u64;
    const PIDS_MAX: u64 = 4 * 1024 * 1024;
    const CPU_MIN_US: u64 = 1_000;
    const CPU_MAX_PERIOD_US: u64 = 1_000_000;
    const CPU_MAX_QUOTA_US: u64 = u64::MAX >> 20; // kernel MAX_BW, in µs

    pub(super) fn path_bytes(path: &Path) -> Result<&[u8]> {
        let bytes = path.as_os_str().as_encoded_bytes();
        if bytes.len() > PATH_MAX_BYTES || bytes.first() != Some(&b'/') || bytes.contains(&0) {
            return Err(E::InvalidPath);
        }
        if bytes != b"/"
            && bytes[1..]
                .split(|&b| b == b'/')
                .any(|part| part.is_empty() || part == b"." || part == b"..")
        {
            return Err(E::InvalidPath);
        }
        Ok(bytes)
    }

    pub(super) fn read_text(reader: impl Read, control: &'static str) -> Result<String> {
        // An extra byte distinguishes exact-size EOF from truncation. Metadata
        // sizes on kernfs aren't the length of the generated control text.
        let mut bytes = Vec::new();
        reader
            .take((READ_CAP + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|err| E::Io {
                operation: "read",
                control: Some(control),
                errno: err.raw_os_error(),
            })?;
        if bytes.len() > READ_CAP {
            return Err(E::OversizeControl { control });
        }
        String::from_utf8(bytes).map_err(|_| E::InvalidControl { control })
    }

    fn without_final_lf(text: &str) -> &str {
        text.strip_suffix('\n').unwrap_or(text)
    }

    fn decimal(token: &str, control: &'static str) -> Result<u64> {
        if token.is_empty()
            || (token.len() > 1 && token.starts_with('0'))
            || !token.bytes().all(|b| b.is_ascii_digit())
        {
            return Err(E::InvalidControl { control });
        }
        token.parse().map_err(|_| E::OutOfRange { control })
    }

    fn range(value: u64, min: u64, max: u64, control: &'static str) -> Result<u64> {
        if !(min..=max).contains(&value) {
            return Err(E::OutOfRange { control });
        }
        Ok(value)
    }

    fn finite(token: &str, min: u64, max: u64, control: &'static str) -> Result<u64> {
        if token == "max" {
            return Err(E::Unlimited { control });
        }
        range(decimal(token, control)?, min, max, control)
    }

    pub(super) fn domain(text: &str) -> Result<()> {
        match without_final_lf(text) {
            "domain" => Ok(()),
            "domain threaded" | "domain invalid" | "threaded" => Err(E::NotDomain),
            _ => Err(E::InvalidControl { control: TYPE }),
        }
    }

    pub(super) fn memory_max(text: &str) -> Result<u64> {
        finite(without_final_lf(text), 1, MEMORY_MAX_BYTES, MEMORY)
    }

    pub(super) fn pids_max(text: &str) -> Result<u64> {
        finite(without_final_lf(text), 1, PIDS_MAX, PIDS)
    }

    pub(super) fn cpu_max(text: &str) -> Result<(u64, u64)> {
        let (quota, period) = without_final_lf(text)
            .split_once(' ')
            .ok_or(E::InvalidControl { control: CPU })?;
        let period = range(decimal(period, CPU)?, CPU_MIN_US, CPU_MAX_PERIOD_US, CPU)?;
        let quota = finite(quota, CPU_MIN_US, CPU_MAX_QUOTA_US, CPU)?;
        Ok((quota, period))
    }

    // A flat map uses one `key decimal` per line. An unknown key is ignored
    // only after validating its `[a-z][a-z0-9_.-]*` spelling and unsigned value;
    // no key may repeat. Unknown entries never get into diagnostics or results.
    fn counters<'a>(text: &'a str, control: &'static str) -> Result<BTreeMap<&'a str, u64>> {
        let mut map = BTreeMap::new();
        for line in without_final_lf(text).split('\n') {
            let (key, value) = line.split_once(' ').ok_or(E::InvalidControl { control })?;
            if !key.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
                || !key.bytes().all(|b| {
                    b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'.' | b'-')
                })
            {
                return Err(E::InvalidControl { control });
            }
            if map.insert(key, decimal(value, control)?).is_some() {
                return Err(E::InvalidControl { control });
            }
        }
        Ok(map)
    }

    pub(super) fn unpopulated(text: &str) -> Result<()> {
        let map = counters(text, EVENTS)?;
        // `frozen` is absent on older kernels but must be 0 or 1 when present.
        let invalid = E::InvalidControl { control: EVENTS };
        let frozen = match map.get("frozen") {
            None | Some(0) => false,
            Some(1) => true,
            _ => return Err(invalid),
        };
        match map.get("populated") {
            Some(0) if frozen => Err(E::Frozen),
            Some(0) => Ok(()),
            Some(1) => Err(E::Populated),
            _ => Err(invalid),
        }
    }

    pub(super) fn leaf(text: &str) -> Result<()> {
        let map = counters(text, STAT)?;
        // Also reject a removed descendant that still has resources pinned.
        // `nr_dying_descendants` is documented in cgroup.stat on modern Linux;
        // if absent, only the mandatory live descendant count can be checked.
        match (map.get("nr_descendants"), map.get("nr_dying_descendants")) {
            (Some(0), None | Some(0)) => Ok(()),
            (Some(_), _) => Err(E::HasDescendants),
            (None, _) => Err(E::InvalidControl { control: STAT }),
        }
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::{LinuxCgroupError as E, LinuxCgroupReadiness, Path, Result, checks::*};
    use std::{
        ffi::{CStr, CString},
        fs::File,
        io,
        mem::{MaybeUninit, size_of},
        os::fd::{AsRawFd, FromRawFd},
    };

    const KILL: &str = "cgroup.kill";

    // Linux UAPI open_how v0 (fixed u64 fields on 32- and 64-bit targets).
    #[repr(C)]
    struct OpenHow {
        flags: u64,
        mode: u64,
        resolve: u64,
    }

    fn open_no_symlinks(
        dirfd: libc::c_int,
        path: &CStr,
        flags: libc::c_int,
        extra_resolve: u64,
    ) -> io::Result<File> {
        let how = OpenHow {
            flags: (flags | libc::O_CLOEXEC) as u64,
            mode: 0,
            resolve: libc::RESOLVE_NO_SYMLINKS | extra_resolve,
        };
        // SAFETY: Both pointers are valid for the syscall, with a NUL-terminated
        // path and the exact size of the UAPI struct. A successful fd is owned.
        let fd = unsafe {
            libc::syscall(
                libc::SYS_openat2,
                dirfd,
                path.as_ptr(),
                &how as *const OpenHow,
                size_of::<OpenHow>(),
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: The successful openat2 returned a new fd representable as int.
        Ok(unsafe { File::from_raw_fd(fd as libc::c_int) })
    }

    fn io_error(operation: &'static str, control: Option<&'static str>, err: io::Error) -> E {
        E::Io {
            operation,
            control,
            errno: err.raw_os_error(),
        }
    }

    fn verify_cgroup2(file: &File, control: Option<&'static str>) -> Result<()> {
        let mut info = MaybeUninit::<libc::statfs>::uninit();
        // SAFETY: info has space for statfs, and file holds a live fd. Only read
        // the result if libc reports successful initialization.
        if unsafe { libc::fstatfs(file.as_raw_fd(), info.as_mut_ptr()) } != 0 {
            return Err(io_error("fstatfs", control, io::Error::last_os_error()));
        }
        // SAFETY: fstatfs succeeded and initialized the structure.
        let fs_type = unsafe { info.assume_init() }.f_type;
        if i128::from(fs_type) != i128::from(libc::CGROUP2_SUPER_MAGIC) {
            return Err(E::NotCgroup2);
        }
        Ok(())
    }

    fn open_control(dir: &File, control: &'static str, flags: libc::c_int) -> Result<Option<File>> {
        let name = CString::new(control).expect("static cgroup control name");
        // Same descriptor-relative mount, not merely another cgroup2 mount:
        // NO_XDEV also rejects a bind mount over an individual control.
        let file = match open_no_symlinks(
            dir.as_raw_fd(),
            &name,
            flags,
            libc::RESOLVE_BENEATH | libc::RESOLVE_NO_XDEV,
        ) {
            Ok(file) => file,
            Err(err) if control == KILL && err.raw_os_error() == Some(libc::ENOENT) => {
                return Ok(None);
            }
            Err(err) => return Err(io_error("open", Some(control), err)),
        };
        verify_cgroup2(&file, Some(control))?;
        if !file
            .metadata()
            .map_err(|err| io_error("stat", Some(control), err))?
            .is_file()
        {
            return Err(E::NotRegularControl { control });
        }
        Ok(Some(file))
    }

    fn read_control(dir: &File, control: &'static str) -> Result<String> {
        let file = open_control(dir, control, libc::O_RDONLY | libc::O_NONBLOCK)?
            .ok_or(E::InvalidControl { control })?;
        read_text(file, control)
    }

    pub(super) fn preflight(path: &Path) -> Result<LinuxCgroupReadiness> {
        let path = CString::new(path_bytes(path)?).map_err(|_| E::InvalidPath)?;
        let dir = open_no_symlinks(libc::AT_FDCWD, &path, libc::O_PATH | libc::O_DIRECTORY, 0)
            .map_err(|err| {
                if err.raw_os_error() == Some(libc::ENOSYS) {
                    E::Openat2Unavailable
                } else {
                    io_error("open directory", None, err)
                }
            })?;
        verify_cgroup2(&dir, None)?;
        domain(&read_control(&dir, TYPE)?)?;
        let memory_max_bytes = memory_max(&read_control(&dir, MEMORY)?)?;
        let pids_max = pids_max(&read_control(&dir, PIDS)?)?;
        let (cpu_quota_us, cpu_period_us) = cpu_max(&read_control(&dir, CPU)?)?;
        // O_PATH checks presence/type only; it never opens cgroup.kill for I/O.
        let cgroup_kill_present = open_control(&dir, KILL, libc::O_PATH)?.is_some();
        leaf(&read_control(&dir, STAT)?)?;
        unpopulated(&read_control(&dir, EVENTS)?)?;
        Ok(LinuxCgroupReadiness {
            memory_max_bytes,
            pids_max,
            cpu_quota_us,
            cpu_period_us,
            cgroup_kill_present,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{LinuxCgroupError as E, checks::*, *};

    #[test]
    fn paths_are_lexically_exact() {
        for invalid in [
            "",
            "rel",
            ".",
            "//group",
            "/group//child",
            "/group/",
            "/./group",
            "/group/../child",
            "/group\0",
        ] {
            assert_eq!(
                path_bytes(Path::new(invalid)),
                Err(E::InvalidPath),
                "{invalid:?}"
            );
        }
        assert_eq!(
            path_bytes(Path::new("/group/child")),
            Ok(&b"/group/child"[..])
        );
        assert_eq!(path_bytes(Path::new("/")), Ok(&b"/"[..]));
        assert_eq!(
            path_bytes(Path::new(&format!("/{}", "x".repeat(4095)))),
            Err(E::InvalidPath)
        );
    }

    #[test]
    fn scalar_limits_are_finite_positive_and_use_implied_units() {
        assert_eq!(memory_max("524288\n"), Ok(524288));
        assert_eq!(pids_max("32\n"), Ok(32));
        assert_eq!(pids_max("4194304"), Ok(4_194_304));
        assert_eq!(pids_max("4194305"), Err(E::OutOfRange { control: PIDS }));
        for invalid in [
            "0",
            "max",
            "1 B",
            "1k",
            " 1",
            "+1",
            "01",
            "1\r\n",
            "1\n\n",
            "9223372036854775808",
            "18446744073709551616",
        ] {
            assert!(memory_max(invalid).is_err(), "{invalid:?}");
        }
        assert_eq!(memory_max("max\n"), Err(E::Unlimited { control: MEMORY }));
        assert_eq!(pids_max("0\n"), Err(E::OutOfRange { control: PIDS }));
    }

    #[test]
    fn cpu_has_exactly_two_microsecond_fields_and_valid_clamps() {
        assert_eq!(cpu_max("1000 1000000\n"), Ok((1000, 1000000)));
        assert_eq!(cpu_max("200000 100000"), Ok((200000, 100000))); // multiple cores
        assert_eq!(cpu_max("17592186044415 1000"), Ok((17592186044415, 1000)));
        for invalid in [
            "1000",
            "1000 1000 0",
            "1000  1000",
            "1000\t1000",
            "1ms 1000",
            "1000 1000us",
            "999 1000",
            "1000 999",
            "1000 1000001",
            "17592186044416 1000",
            "1000 max",
        ] {
            assert!(cpu_max(invalid).is_err(), "{invalid:?}");
        }
        assert_eq!(cpu_max("max 100000\n"), Err(E::Unlimited { control: CPU }));
    }

    #[test]
    fn domain_and_leaf_are_separate_from_population() {
        assert_eq!(domain("domain\n"), Ok(()));
        assert_eq!(domain("domain threaded\n"), Err(E::NotDomain));
        assert_eq!(domain("domain invalid\n"), Err(E::NotDomain));
        assert_eq!(domain("threaded\n"), Err(E::NotDomain));
        assert!(domain(" domain ").is_err());
        assert_eq!(leaf("nr_descendants 0\nnr_dying_descendants 0\n"), Ok(()));
        assert_eq!(
            leaf("nr_descendants 0\nnr_dying_descendants 2\n"),
            Err(E::HasDescendants)
        );
        assert_eq!(leaf("nr_descendants 1\n"), Err(E::HasDescendants));
        assert!(leaf("nr_dying_descendants 0\n").is_err());
    }

    #[test]
    fn events_are_keyed_validate_known_flags_and_ignore_well_formed_extensions() {
        assert_eq!(unpopulated("populated 0\n"), Ok(()));
        assert_eq!(
            unpopulated("future-key.1 42\nfrozen 0\npopulated 0\n"),
            Ok(())
        );
        assert_eq!(unpopulated("populated 1\nfrozen 0\n"), Err(E::Populated));
        assert_eq!(unpopulated("populated 0\nfrozen 1\n"), Err(E::Frozen));
        for invalid in [
            "frozen 0",
            "populated 2",
            "populated 0\nfrozen 2",
            "populated 0\npopulated 1",
            "populated 0\nfrozen 0\nfrozen 1",
            "populated 0\nnew 1\nnew 2",
            "populated 0\nnew true",
            "populated 0\nnew 1 value",
            "populated 0\n@new 1",
            "populated 0\n\n",
        ] {
            assert!(unpopulated(invalid).is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn read_cap_rejects_truncation_and_invalid_utf8() {
        assert_eq!(
            read_text(&vec![b'a'; READ_CAP][..], TYPE).unwrap().len(),
            READ_CAP
        );
        assert_eq!(
            read_text(&vec![b'a'; READ_CAP + 1][..], TYPE),
            Err(E::OversizeControl { control: TYPE })
        );
        assert_eq!(
            read_text(&[0xff][..], TYPE),
            Err(E::InvalidControl { control: TYPE })
        );
    }

    #[test]
    fn readiness_serializes_limits_and_optional_kill_presence_only() {
        let ready = LinuxCgroupReadiness {
            memory_max_bytes: 524288,
            pids_max: 32,
            cpu_quota_us: 20000,
            cpu_period_us: 100000,
            cgroup_kill_present: false,
        };
        assert_eq!(
            serde_json::to_value(ready).unwrap(),
            serde_json::json!({
                "memory_max_bytes": 524288, "pids_max": 32, "cpu_quota_us": 20000,
                "cpu_period_us": 100000, "cgroup_kill_present": false,
            })
        );
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn non_linux_is_a_serializable_typed_error() {
        let error = preflight(Path::new("/some/path")).unwrap_err();
        assert_eq!(error, E::UnsupportedPlatform);
        assert_eq!(
            serde_json::to_value(error).unwrap(),
            serde_json::json!({"kind": "unsupported_platform"})
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn rejects_non_cgroup_and_symlink_paths_before_reading_controls() {
        assert_eq!(preflight(Path::new("/proc")), Err(E::NotCgroup2));
        assert!(matches!(
            preflight(Path::new("/proc/self")),
            Err(E::Io {
                operation: "open directory",
                errno: Some(libc::ELOOP),
                ..
            })
        ));
        assert!(matches!(
            preflight(Path::new("/proc/self/fd")),
            Err(E::Io {
                operation: "open directory",
                errno: Some(libc::ELOOP),
                ..
            })
        ));
    }
}
