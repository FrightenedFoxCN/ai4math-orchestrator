//! Compare a trusted, post-setup Linux mountinfo snapshot against a plan.
//! This does not open `/proc`, mount anything, pin source objects or attest
//! the namespace. A trusted launcher must acquire the snapshot before exec.
use crate::linux_mount_plan::{LinuxMountPlan, MountFlags, MountKind};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Read,
};

const MAX_BYTES: usize = 1024 * 1024;
const MAX_PATH: usize = 4095;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MountAuditError {
    Read,
    Oversize,
    InvalidRecord,
    DuplicateMount,
    UnexpectedMount,
    MissingMount,
    BadRoot,
    WrongParent,
    UnsafePropagation,
    WrongFlags,
}

impl std::fmt::Display for MountAuditError {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // No raw mountinfo strings, filesystem source names or paths in errors.
        out.write_str(match self {
            Self::Read => "could not read Linux mount snapshot",
            Self::Oversize => "Linux mount snapshot exceeded bound",
            Self::InvalidRecord => "invalid Linux mount snapshot record",
            Self::DuplicateMount => "duplicate Linux mount target or ID",
            Self::UnexpectedMount => "mount outside the Linux mount plan",
            Self::MissingMount => "planned Linux mount is missing",
            Self::BadRoot => "expected a private, sealed tmpfs root",
            Self::WrongParent => "Linux mount has the wrong parent",
            Self::UnsafePropagation => "Linux mount has unsupported propagation",
            Self::WrongFlags => "Linux mount flags do not match the plan",
        })
    }
}
impl std::error::Error for MountAuditError {}
type Result<T> = std::result::Result<T, MountAuditError>;
use MountAuditError as E;

/// The observed root must be a sealed tmpfs (not a host bind); all non-synthetic
/// targets must be individual mounts, including the writable scratch overlay.
/// Any extra mount, even under a read-only data tree, causes rejection.
/// This checks the topology and effective flags, not source inode identity,
/// directory contents, capabilities, cgroups or executable path enforcement.
pub fn verify_mount_snapshot(plan: &LinuxMountPlan, snapshot: impl Read) -> Result<()> {
    let mut bytes = Vec::new();
    snapshot
        .take(MAX_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| E::Read)?;
    if bytes.len() > MAX_BYTES {
        return Err(E::Oversize);
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| E::InvalidRecord)?;
    let text = text.strip_suffix('\n').ok_or(E::InvalidRecord)?;
    let mut expected = BTreeMap::new();
    for op in plan.operations() {
        if op.kind() != MountKind::SyntheticDirectory {
            expected.insert(op.target(), op.kind());
        }
    }
    let mut observed = BTreeMap::new();
    let mut ids = BTreeSet::new();
    for line in text.split('\n') {
        let row = parse(line)?;
        if row.target != "/" && !expected.contains_key(row.target.as_str()) {
            return Err(E::UnexpectedMount);
        }
        if !ids.insert(row.id) || observed.contains_key(&row.target) {
            return Err(E::DuplicateMount);
        }
        observed.insert(row.target.clone(), row);
    }

    let root = observed.get("/").ok_or(E::MissingMount)?;
    if root.fs != "tmpfs"
        || root.source_root != "/"
        || (root.parent != root.id && ids.contains(&root.parent))
        || !root.flags.matches(MountKind::SyntheticDirectory.flags())
    {
        return Err(E::BadRoot);
    }
    for (path, kind) in expected {
        let row = observed.get(path).ok_or(E::MissingMount)?;
        if !row.flags.matches(kind.flags())
            || (kind == MountKind::WritableScratch && !row.super_writable)
        {
            return Err(E::WrongFlags);
        }
        let parent = if kind == MountKind::WritableScratch {
            let (room, _) = path.rsplit_once('/').ok_or(E::WrongParent)?;
            observed.get(room).ok_or(E::MissingMount)?
        } else {
            root
        };
        if row.parent != parent.id {
            return Err(E::WrongParent);
        }
    }
    Ok(())
}

struct Row<'a> {
    id: u64,
    parent: u64,
    source_root: String,
    target: String,
    flags: ObservedFlags,
    fs: &'a str,
    super_writable: bool,
}

struct ObservedFlags {
    read_only: bool,
    no_exec: bool,
    no_suid: bool,
    no_dev: bool,
}
impl ObservedFlags {
    fn matches(&self, required: MountFlags) -> bool {
        self.read_only == required.read_only
            && self.no_exec == required.no_exec
            && self.no_suid == required.no_suid
            && self.no_dev == required.no_dev
    }
}

fn parse(line: &str) -> Result<Row<'_>> {
    if line.is_empty() || line.chars().any(char::is_control) || line.split(' ').any(str::is_empty) {
        return Err(E::InvalidRecord);
    }
    let mut fields = line.split(' ');
    let mut next = || fields.next().ok_or(E::InvalidRecord);
    let id = number(next()?)?;
    let parent = number(next()?)?;
    if id == 0 || parent == 0 {
        return Err(E::InvalidRecord);
    }
    let (major, minor) = next()?.split_once(':').ok_or(E::InvalidRecord)?;
    number(major)?;
    number(minor)?;
    let source_root = path(next()?)?;
    let target = path(next()?)?;
    let mount_opts = options(next()?)?;
    let flags = ObservedFlags {
        read_only: read_only(&mount_opts)?,
        no_exec: no_flag(&mount_opts, "noexec", "exec")?,
        no_suid: no_flag(&mount_opts, "nosuid", "suid")?,
        no_dev: no_flag(&mount_opts, "nodev", "dev")?,
    };
    match next()? {
        "-" => (),
        "unbindable" if next()? == "-" => (),
        _ => return Err(E::UnsafePropagation),
    }
    let fs = next()?;
    let _fs_source = next()?;
    let super_writable = !read_only(&options(next()?)?)?;
    if fields.next().is_some() {
        return Err(E::InvalidRecord);
    }
    Ok(Row {
        id,
        parent,
        source_root,
        target,
        flags,
        fs,
        super_writable,
    })
}

fn number(s: &str) -> Result<u64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err(E::InvalidRecord);
    }
    s.parse().map_err(|_| E::InvalidRecord)
}

fn options(s: &str) -> Result<BTreeSet<&str>> {
    let mut flags = BTreeSet::new();
    for token in s.split(',') {
        if token.is_empty() || !flags.insert(token) {
            return Err(E::InvalidRecord);
        }
    }
    Ok(flags)
}
fn read_only(flags: &BTreeSet<&str>) -> Result<bool> {
    match (flags.contains("ro"), flags.contains("rw")) {
        (true, false) => Ok(true),
        (false, true) => Ok(false),
        _ => Err(E::InvalidRecord),
    }
}
fn no_flag(flags: &BTreeSet<&str>, negative: &str, positive: &str) -> Result<bool> {
    if flags.contains(negative) && flags.contains(positive) {
        return Err(E::InvalidRecord);
    }
    Ok(flags.contains(negative))
}

fn path(raw: &str) -> Result<String> {
    let mut bytes = Vec::with_capacity(raw.len().min(MAX_PATH));
    let mut raw = raw.as_bytes();
    while let Some((&byte, rest)) = raw.split_first() {
        raw = rest;
        if byte == b'\\' {
            // mountinfo escapes space, tab, newline, and backslash. Reject
            // control characters as the plan cannot name them either.
            match raw.get(..3) {
                Some(b"040") => bytes.push(b' '),
                Some(b"134") => bytes.push(b'\\'),
                _ => return Err(E::InvalidRecord),
            }
            raw = &raw[3..];
        } else {
            bytes.push(byte);
        }
        if bytes.len() > MAX_PATH {
            return Err(E::InvalidRecord);
        }
    }
    let value = String::from_utf8(bytes).map_err(|_| E::InvalidRecord)?;
    if value != "/"
        && value
            .strip_prefix('/')
            .is_none_or(|tail| tail.split('/').any(|part| matches!(part, "" | "." | "..")))
    {
        return Err(E::InvalidRecord);
    }
    Ok(value)
}

#[cfg(test)]
mod tests;
