//! Shared verification of content-addressed briefing fact packs.

pub use crate::{BreakerProjection as ReviewedBreaker, InboxProjection as ReviewedInbox};
use crate::{Checkpoint, FrontierEdge};
use anyhow::{Context, Result, ensure};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    ffi::{CStr, CString},
    fs::{self, File, Metadata},
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{
            ffi::OsStrExt,
            fs::{MetadataExt, OpenOptionsExt},
        },
    },
    path::{Component, Path, PathBuf},
};

/// Compact JSON budget, leaving room for the daemon JSON-RPC envelope.
pub const MAX_PACK_BYTES: usize = 768 * 1024;
/// On-disk budget, including for legacy pretty-printed JSON.
pub const MAX_LEGACY_FILE_BYTES: u64 = 2 * 1024 * 1024;

/// Reviewed metadata from the verified pack, not from the current database.
#[derive(Debug, Clone)]
pub struct RetainedReview {
    pub event_from_exclusive: i64,
    pub event_to_inclusive: i64,
    /// Latest commit recorded in the event history at the upper bound, not a live Git ref.
    pub to_git_commit: Option<String>,
    pub checkpoint: Option<Checkpoint>,
    /// A displayed page or inbox evidence was truncated. This does not change the event bounds.
    pub presentation_truncated: bool,
    pub breakers: Vec<ReviewedBreaker>,
    pub inbox: Vec<ReviewedInbox>,
    /// Only the displayed edge rows; a route selection must prove completeness against the ledger.
    pub frontier_edges: Vec<FrontierEdge>,
}

impl RetainedReview {
    pub fn reviewed_breaker(&self, id: &str) -> Option<&ReviewedBreaker> {
        self.breakers.iter().find(|item| item.breaker_id == id)
    }

    pub fn reviewed_inbox(&self, id: &str) -> Option<&ReviewedInbox> {
        self.inbox.iter().find(|item| item.inbox_id == id)
    }
}

/// Verify a retained pack and decode the metadata and selected projections needed to bind a review.
pub fn read_retained_review(dir: &Path, id: &str) -> Result<Option<RetainedReview>> {
    read_retained(dir, id)?
        .as_ref()
        .map(review_metadata)
        .transpose()
}

fn review_metadata(pack: &Value) -> Result<RetainedReview> {
    let event_from_exclusive = field(pack, "/event_from_exclusive")?;
    let event_to_inclusive = field(pack, "/event_to_inclusive")?;
    ensure!(
        0 <= event_from_exclusive && event_from_exclusive <= event_to_inclusive,
        "invalid retained event interval"
    );
    let checkpoint: Option<Checkpoint> = field(pack, "/checkpoint")?;
    ensure!(
        checkpoint.as_ref().map_or(0, |cursor| cursor.event_seq) == event_from_exclusive,
        "retained checkpoint does not match the lower event bound"
    );

    // Read known presentation flags, not arbitrary fields in event payloads.
    let mut presentation_truncated = false;
    for pointer in [
        "/events/truncated",
        "/frontier/truncated",
        "/frontier/plans/truncated",
        "/milestones/truncated",
        "/refutations/truncated",
        "/risk_radar/events/truncated",
        "/breakers/truncated",
        "/inbox/truncated",
    ] {
        presentation_truncated |= field::<bool>(pack, pointer)?;
    }
    let breakers: Vec<ReviewedBreaker> = field(pack, "/breakers/items")?;
    let inbox: Vec<ReviewedInbox> = field(pack, "/inbox/items")?;
    verify_unique_ids(breakers.iter().map(|item| item.breaker_id.as_str()))?;
    verify_unique_ids(inbox.iter().map(|item| item.inbox_id.as_str()))?;
    presentation_truncated |= inbox.iter().any(|item| item.payload_truncated);

    Ok(RetainedReview {
        event_from_exclusive,
        event_to_inclusive,
        to_git_commit: field(pack, "/to_git_commit")?,
        checkpoint,
        presentation_truncated,
        breakers,
        inbox,
        frontier_edges: field(pack, "/frontier/plans/items")?,
    })
}

fn field<T: DeserializeOwned>(pack: &Value, pointer: &str) -> Result<T> {
    let value = pack
        .pointer(pointer)
        .with_context(|| format!("missing retained field {pointer}"))?;
    T::deserialize(value).with_context(|| format!("invalid retained field {pointer}"))
}

fn verify_unique_ids<'a>(ids: impl Iterator<Item = &'a str>) -> Result<()> {
    let mut seen = std::collections::BTreeSet::new();
    for id in ids {
        ensure!(
            !id.is_empty() && seen.insert(id),
            "invalid or duplicate retained item ID"
        );
    }
    Ok(())
}

/// Count compact JSON bytes without allocating the encoded value.
pub fn compact_len(value: &impl Serialize, max: usize) -> Result<Option<usize>> {
    // Stop as soon as an entry cannot fit, without allocating its JSON output.
    struct Counter {
        len: usize,
        max: usize,
        overflow: bool,
    }
    impl Write for Counter {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            if data.len() > self.max - self.len {
                self.overflow = true;
                return Err(std::io::Error::other("compact byte budget exceeded"));
            }
            self.len += data.len();
            Ok(data.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter {
        len: 0,
        max,
        overflow: false,
    };
    let encoded = serde_json::to_writer(&mut counter, value);
    if counter.overflow {
        return Ok(None);
    }
    encoded?;
    Ok(Some(counter.len))
}

/// Digest of the canonical JSON body, excluding the embedded fact ID.
pub fn hash_fact(value: &Value) -> Result<String> {
    Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(value)?)))
}

pub fn valid_id(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Read the content-addressed JSON from a trusted retained directory.
/// The digest verifies canonical content (not formatting or who produced it).
/// Returns `Ok(None)` only when the file is absent.
pub fn read_retained(dir: &Path, id: &str) -> Result<Option<Value>> {
    ensure!(valid_id(id), "invalid fact ID");
    let pinned = match open_retained_dir(dir) {
        Ok(dir) => dir,
        Err(err)
            if err
                .downcast_ref::<io::Error>()
                .is_some_and(|err| err.kind() == io::ErrorKind::NotFound) =>
        {
            return Ok(None);
        }
        Err(err) => return Err(err),
    };
    let name = CString::new(format!("{id}.json"))?;
    let value = read_retained_file(&pinned, &name, MAX_LEGACY_FILE_BYTES)?
        .map(|bytes| decode_retained(id, &bytes))
        .transpose()?;
    verify_retained_dir(dir, &pinned)?;
    Ok(value)
}

/// Check the size, content-address and compact wire budget of retained JSON.
/// The caller must separately ensure the bytes came from an admitted file.
pub fn decode_retained(id: &str, bytes: &[u8]) -> Result<Value> {
    ensure!(valid_id(id), "invalid fact ID");
    ensure!(
        bytes.len() as u64 <= MAX_LEGACY_FILE_BYTES,
        "retained file exceeds limit"
    );
    let mut value: Value = serde_json::from_slice(bytes)?;
    ensure!(
        compact_len(&value, MAX_PACK_BYTES)?.is_some(),
        "retained fact pack exceeds compact byte budget"
    );
    let body = value
        .as_object_mut()
        .context("retained payload is not an object")?;
    let id_field = body.remove("fact_sha256").context("retained ID mismatch")?;
    ensure!(id_field.as_str() == Some(id), "retained ID mismatch");
    ensure!(hash_fact(&value)? == id, "retained digest mismatch");
    value["fact_sha256"] = id_field;
    Ok(value)
}

/// Validate the effective permission boundary of an already-open inode on
/// systems whose extended ACLs are not reflected in the Unix group mode mask.
pub fn validate_artifact_acl(file: &File) -> Result<()> {
    crate::wiki::scan_acl::validate(file.as_raw_fd())
}

/// Pin the final directory component. Parent path admission is the caller's
/// responsibility; in production this sits below the admitted project root.
pub fn open_retained_dir(path: &Path) -> Result<File> {
    let path: PathBuf = path.components().collect();
    ensure!(
        matches!(
            path.components().next_back(),
            Some(Component::Normal(_) | Component::CurDir)
        ),
        "invalid retained directory path"
    );
    let dir = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&path)
        .with_context(|| format!("open retained directory {}", path.display()))?;
    let meta = dir.metadata()?;
    ensure!(
        meta.is_dir() && meta.uid() == unsafe { libc::geteuid() } && meta.mode() & 0o022 == 0,
        "unsafe retained directory"
    );
    validate_artifact_acl(&dir)?;
    Ok(dir)
}

/// Reject a path that no longer names the pinned retained directory.
pub fn verify_retained_dir(path: &Path, dir: &File) -> Result<()> {
    let observed = open_retained_dir(path)?.metadata()?;
    let pinned = dir.metadata()?;
    ensure!(
        observed.dev() == pinned.dev() && observed.ino() == pinned.ino(),
        "retained directory changed during operation"
    );
    Ok(())
}

/// Open an existing basename without following links or blocking on a FIFO.
/// `dir` must be an admitted and pinned retained directory.
pub fn open_retained_file(dir: &File, name: &CStr, max_len: u64) -> io::Result<File> {
    let bytes = name.to_bytes();
    if bytes.is_empty() || bytes == b"." || bytes == b".." || bytes.contains(&b'/') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid retained basename",
        ));
    }
    let mut named = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: dir and name are live, and named points to writable storage.
    if unsafe {
        libc::fstatat(
            dir.as_raw_fd(),
            name.as_ptr(),
            named.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful fstatat initialized named.
    let named = unsafe { named.assume_init() };
    if named.st_mode & libc::S_IFMT != libc::S_IFREG {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid retained file type",
        ));
    }
    // SAFETY: dir is live and name is a validated single C string component.
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful openat returned a fresh, owned descriptor.
    let file = unsafe { File::from_raw_fd(fd) };
    let mut opened = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: fd is owned by file; opened points to writable storage.
    if unsafe { libc::fstat(fd, opened.as_mut_ptr()) } < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful fstat initialized opened.
    let opened = unsafe { opened.assume_init() };
    if opened.st_mode & libc::S_IFMT != libc::S_IFREG
        || opened.st_dev != named.st_dev
        || opened.st_ino != named.st_ino
        || opened.st_uid != unsafe { libc::geteuid() }
        || opened.st_nlink != 1
        || opened.st_mode & 0o022 != 0
        || u64::try_from(opened.st_size).map_or(true, |len| len > max_len)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid retained file",
        ));
    }
    validate_artifact_acl(&file)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, format!("{err:#}")))?;
    Ok(file)
}

fn file_stamp(meta: &Metadata) -> (u64, u64, u64, i64, i64, i64, i64) {
    (
        meta.dev(),
        meta.ino(),
        meta.len(),
        meta.mtime(),
        meta.mtime_nsec(),
        meta.ctime(),
        meta.ctime_nsec(),
    )
}

fn read_retained_file(dir: &File, name: &CStr, max_len: u64) -> Result<Option<Vec<u8>>> {
    let mut file = match open_retained_file(dir, name, max_len) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    let before = file.metadata()?;
    let limit = max_len
        .checked_add(1)
        .context("invalid retained byte limit")?;
    let mut bytes = Vec::new();
    (&mut file).take(limit).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= max_len, "retained file exceeds limit");
    let named = open_retained_file(dir, name, max_len)?.metadata()?;
    ensure!(
        bytes.len() as u64 == before.len()
            && file_stamp(&before) == file_stamp(&named)
            && file_stamp(&before) == file_stamp(&file.metadata()?),
        "retained file changed during read"
    );
    Ok(Some(bytes))
}

/// Compare an already-retained artifact with the bytes about to be retained.
pub fn matches_retained_file(path: &Path, bytes: &[u8]) -> Result<bool> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let dir = open_retained_dir(parent)?;
    let name = CString::new(
        path.file_name()
            .context("missing retained basename")?
            .as_bytes(),
    )?;
    let existing =
        read_retained_file(&dir, &name, bytes.len() as u64)?.context("retained file absent")?;
    verify_retained_dir(parent, &dir)?;
    Ok(existing == bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StateStore;
    use serde_json::json;

    fn retain_test_pack(dir: &Path, mut body: Value) -> Result<String> {
        let id = hash_fact(&body)?;
        body["fact_sha256"] = json!(id);
        fs::write(
            dir.join(format!("{id}.json")),
            serde_json::to_vec_pretty(&body)?,
        )?;
        Ok(id)
    }

    #[test]
    fn retained_reads_reject_aliases_and_directory_drift() -> Result<()> {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir()?;
        let dir = temp.path().join("briefings");
        fs::create_dir(&dir)?;
        let id = retain_test_pack(&dir, json!({"payload":"intact"}))?;
        assert!(read_retained(&dir, &id)?.is_some());

        let alias = temp.path().join("alias");
        symlink(&dir, &alias)?;
        assert!(read_retained(&alias.join("."), &id).is_err());

        let pinned = open_retained_dir(&dir)?;
        fs::rename(&dir, temp.path().join("former"))?;
        fs::create_dir(&dir)?;
        assert!(verify_retained_dir(&dir, &pinned).is_err());
        assert!(read_retained(&dir, &id)?.is_none());
        assert!(read_retained(&temp.path().join("missing"), &id)?.is_none());
        Ok(())
    }

    #[cfg(target_vendor = "apple")]
    #[test]
    fn retained_reads_reject_mutating_extended_acl() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let id = retain_test_pack(dir.path(), json!({"payload":"intact"}))?;
        let artifact = dir.path().join(format!("{id}.json"));
        assert!(
            std::process::Command::new("/bin/chmod")
                .args(["+a", "everyone allow write,delete"])
                .arg(&artifact)
                .status()?
                .success()
        );
        let err = read_retained(dir.path(), &id).unwrap_err();
        assert!(format!("{err:#}").contains("ACL grants"), "{err:#}");
        Ok(())
    }

    #[test]
    fn decodes_event_bounds_checkpoint_and_exact_presentation() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut store = StateStore::open(":memory:")?;
        let mut body = store.briefing_facts()?;
        let checkpoint = Checkpoint {
            cursor_id: "checkpoint-7".into(),
            event_seq: 7,
            git_commit: "older".into(),
            last_adr_id: Some("ADR-1".into()),
        };
        let breaker = ReviewedBreaker {
            breaker_id: "break-1".into(),
            target_definition_id: "target".into(),
            module_path: "Math.lean".into(),
            engine: "lean4".into(),
            state: "TRIPPED".into(),
            updated_at: "2026-10-10 11:22:33".into(),
            probe_status: None,
            probe_passed: None,
            probe_required: None,
        };
        let inbox = ReviewedInbox {
            inbox_id: "inbox-1".into(),
            priority: "P0".into(),
            category: "test".into(),
            summary: "Review the target".into(),
            // Default JSON decoding rounds this score differently and changes the digest.
            leverage_score: 0.42925555022643347,
            created_at: "2026-10-10 11:22:34".into(),
            payload: None,
            payload_truncated: true,
        };
        body.event_from_exclusive = 7;
        body.event_to_inclusive = 11;
        body.to_git_commit = Some("newer".into());
        body.checkpoint = Some(checkpoint.clone());
        body.breakers.items.push(breaker.clone());
        body.inbox.items.push(inbox.clone());
        let id = retain_test_pack(dir.path(), serde_json::to_value(&body)?)?;
        let review = read_retained_review(dir.path(), &id)?.unwrap();
        assert_eq!(review.event_from_exclusive, 7);
        assert_eq!(review.event_to_inclusive, 11);
        assert_eq!(review.to_git_commit.as_deref(), Some("newer"));
        assert_eq!(review.checkpoint, Some(checkpoint));
        assert!(review.presentation_truncated);
        assert_eq!(review.reviewed_breaker("break-1"), Some(&breaker));
        assert_eq!(review.reviewed_inbox("inbox-1"), Some(&inbox));
        assert!(review.reviewed_breaker("not-shown").is_none());
        assert!(review.reviewed_inbox("not-shown").is_none());
        Ok(())
    }

    #[test]
    fn verifies_digest_before_extracting_metadata() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut store = StateStore::open(":memory:")?;
        let mut body = serde_json::to_value(store.briefing_facts()?)?;
        body["frontier"]["plans"]["truncated"] = json!(true);
        let id = retain_test_pack(dir.path(), body.clone())?;
        assert!(
            read_retained_review(dir.path(), &id)?
                .unwrap()
                .presentation_truncated
        );
        body["fact_sha256"] = json!(id);
        body["event_to_inclusive"] = json!(1);
        fs::write(
            dir.path().join(format!("{id}.json")),
            serde_json::to_vec(&body)?,
        )?;
        let err = read_retained_review(dir.path(), &id).unwrap_err();
        assert!(err.to_string().contains("digest mismatch"));
        Ok(())
    }

    #[test]
    fn digest_valid_pack_does_not_bypass_review_checks() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut store = StateStore::open(":memory:")?;
        let mut body = serde_json::to_value(store.briefing_facts()?)?;
        let id = retain_test_pack(dir.path(), body.clone())?;
        assert!(
            !read_retained_review(dir.path(), &id)?
                .unwrap()
                .presentation_truncated
        );
        body["event_from_exclusive"] = json!(1);
        let id = retain_test_pack(dir.path(), body.clone())?;
        assert!(read_retained(dir.path(), &id)?.is_some());
        let err = read_retained_review(dir.path(), &id).unwrap_err();
        assert!(err.to_string().contains("event interval"));
        body["event_to_inclusive"] = json!(1);
        let id = retain_test_pack(dir.path(), body)?;
        let err = read_retained_review(dir.path(), &id).unwrap_err();
        assert!(err.to_string().contains("checkpoint does not match"));
        Ok(())
    }
}
