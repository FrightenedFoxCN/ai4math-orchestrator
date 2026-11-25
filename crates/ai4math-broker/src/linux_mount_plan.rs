//! Pure policy compiler for the first Linux verifier profile. It does not
//! inspect or mount anything, and its output cannot authorize proof receipts.
use std::collections::BTreeMap;

const MAX_EXPLICIT_GRANTS: usize = 256;
const MAX_OPERATIONS: usize = 1024;
const MAX_PATH_BYTES: usize = 4095;

/// Trusted platform configuration only, never a proof request or compiler output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Grant<'a> {
    /// An individually approved dynamic loader or shared object (not a program).
    RuntimeFile(&'a str),
    DataFile(&'a str),
    DataTree(&'a str),
    /// Do not bind the host directory or enumerate any of its children.
    LiteralDirectory(&'a str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MountKind {
    SyntheticDirectory,
    ExecutableFile,
    RuntimeFile,
    DataFile,
    DataTree,
    ReadOnlyRoom,
    WritableScratch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MountFlags {
    pub read_only: bool,
    pub no_exec: bool,
    pub no_suid: bool,
    pub no_dev: bool,
}

impl MountKind {
    /// Required effective flags, including all descendants of a tree bind.
    /// Mount `noexec` alone does not enforce `exec_target` below.
    pub fn flags(self) -> MountFlags {
        MountFlags {
            read_only: self != Self::WritableScratch,
            no_exec: !matches!(self, Self::ExecutableFile | Self::RuntimeFile),
            no_suid: true,
            no_dev: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountOperation {
    target: String,
    kind: MountKind,
}

impl MountOperation {
    pub fn kind(&self) -> MountKind {
        self.kind
    }

    pub fn target(&self) -> &str {
        &self.target
    }

    /// The initial profile preserves absolute paths. Synthetic directories
    /// have no host source or host metadata to project.
    pub fn source(&self) -> Option<&str> {
        (self.kind != MountKind::SyntheticDirectory).then_some(self.target())
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct LinuxMountPlan {
    exec_target: String,
    operations: Vec<MountOperation>,
}

impl LinuxMountPlan {
    /// Only this path may be executed; RuntimeFile grants executable mapping,
    /// not process execution. A launcher still needs a separate enforcement layer.
    pub fn exec_target(&self) -> &str {
        &self.exec_target
    }

    /// Parents come first. Synthetic directories may be sealed only after
    /// all child targets/mounts have been created. Consumers must enforce
    /// final flags and confirm identity independently of this pure plan.
    pub fn operations(&self) -> &[MountOperation] {
        &self.operations
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MountPlanError {
    InvalidPath,
    ReservedPath,
    ScratchNotDirectChild,
    TooManyGrants,
    TooManyOperations,
    DuplicateTarget,
    ConflictingTargets,
}

impl std::fmt::Display for MountPlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::InvalidPath => "expected normalized absolute Linux mount path",
            Self::ReservedPath => "Linux mount target is reserved or too broad",
            Self::ScratchNotDirectChild => "writable scratch must be a direct child of the room",
            Self::TooManyGrants => "Linux mount grant limit exceeded",
            Self::TooManyOperations => "Linux mount operation limit exceeded",
            Self::DuplicateTarget => "duplicate Linux mount target",
            Self::ConflictingTargets => "Linux mount grants overlap",
        })
    }
}

impl std::error::Error for MountPlanError {}

type Result<T> = std::result::Result<T, MountPlanError>;

/// Compile policy for a native ELF verifier. No executable directory binds,
/// host-root bind, or writable grant other than scratch can be represented.
/// Paths here are lexically exact; the launcher must pin and verify their
/// physical identities, kinds, immutability/ownership, and mount topology.
/// Scratch and room must be private and created by the Broker. The launcher
/// must reject unsafe sources (including aliases, submounts and special files)
/// and verify the composed mounts before executing anything.
pub fn compile(
    executable: &str,
    room: &str,
    scratch: &str,
    grants: &[Grant<'_>],
) -> Result<LinuxMountPlan> {
    use MountKind::*;
    use MountPlanError as E;
    if grants.len() > MAX_EXPLICIT_GRANTS {
        return Err(E::TooManyGrants);
    }
    let mut explicit = BTreeMap::new();
    for (path, kind) in [
        (executable, ExecutableFile),
        (room, ReadOnlyRoom),
        (scratch, WritableScratch),
    ]
    .into_iter()
    .chain(grants.iter().map(|grant| match *grant {
        Grant::RuntimeFile(path) => (path, RuntimeFile),
        Grant::DataFile(path) => (path, DataFile),
        Grant::DataTree(path) => (path, DataTree),
        Grant::LiteralDirectory(path) => (path, SyntheticDirectory),
    })) {
        validate_path(path, kind)?;
        if explicit.insert(path, kind).is_some() {
            return Err(E::DuplicateTarget);
        }
    }
    if parent(scratch) != Some(room) {
        return Err(E::ScratchNotDirectChild);
    }
    // Only a synthetic directory may surround other grants. The one physical
    // overlay is Scratch over its existing mountpoint in the read-only Room.
    let mut operations = explicit.clone();
    for (&path, &kind) in &explicit {
        let mut ancestor = parent(path);
        while let Some(base) = ancestor {
            match explicit.get(base) {
                Some(SyntheticDirectory) => (),
                Some(ReadOnlyRoom) if kind == WritableScratch && base == room => (),
                Some(_) => return Err(E::ConflictingTargets),
                None => {
                    operations.entry(base).or_insert(SyntheticDirectory);
                    if operations.len() > MAX_OPERATIONS {
                        return Err(E::TooManyOperations);
                    }
                }
            }
            ancestor = parent(base);
        }
    }
    // Component separators are part of the key; lexicographic order places
    // each ancestor before its own descendants, regardless of input order.
    Ok(LinuxMountPlan {
        exec_target: executable.into(),
        operations: operations
            .into_iter()
            .map(|(target, kind)| MountOperation {
                target: target.into(),
                kind,
            })
            .collect(),
    })
}

fn parent(path: &str) -> Option<&str> {
    path.rsplit_once('/')
        .and_then(|(parent, _)| (!parent.is_empty()).then_some(parent))
}

fn validate_path(path: &str, kind: MountKind) -> Result<()> {
    use MountKind::SyntheticDirectory;
    use MountPlanError as E;
    let suffix = path.strip_prefix('/').ok_or(E::InvalidPath)?;
    if path.len() > MAX_PATH_BYTES
        || path.chars().any(char::is_control)
        || suffix
            .split('/')
            .any(|item| matches!(item, "" | "." | ".."))
    {
        return Err(E::InvalidPath);
    }
    let mut components = suffix.split('/');
    let top = components.next().ok_or(E::InvalidPath)?;
    // Preserve the empty namespace's system boundaries. No proc, sysfs,
    // inherited device, service socket tree, or host system configuration.
    if matches!(top, "proc" | "sys" | "dev" | "run" | "etc")
        || (components.next().is_none() && kind != SyntheticDirectory)
        || (matches!(
            kind,
            MountKind::DataTree | MountKind::ReadOnlyRoom | MountKind::WritableScratch
        ) && matches!(
            path,
            "/usr/lib" | "/usr/lib64" | "/usr/local" | "/var/log" | "/var/lib"
        ))
    {
        return Err(E::ReservedPath);
    }
    Ok(())
}

#[cfg(test)]
mod tests;
