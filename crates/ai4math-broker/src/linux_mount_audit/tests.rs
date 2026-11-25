use super::*;
use crate::linux_mount_plan::{Grant, compile};

const ROOT: &str = "10 9 0:70 / / ro,nosuid,nodev,noexec - tmpfs tmpfs rw,size=256k";
const ROOM: &str = "11 10 8:1 /fixtures/room /work/room ro,nosuid,nodev,noexec - ext4 /dev/test rw";
const SCRATCH: &str =
    "12 11 8:1 /fixtures/write /work/room/scratch rw,nosuid,nodev,noexec - ext4 /dev/test rw";
const EXEC: &str = "13 10 8:1 /bin/verify /opt/bin/verify ro,nosuid,nodev - ext4 /dev/test rw";
const LIB: &str = "14 10 8:1 /lib/a.so /opt/lib/a.so ro,nodev,nosuid - ext4 /dev/test rw";
const DATA: &str = r"15 10 8:1 /fixture/a\040b\134c /work/data/a\040b\134c ro,nosuid,nodev,noexec unbindable - ext4 /dev/test rw";
const VALID: [&str; 6] = [ROOT, ROOM, SCRATCH, EXEC, LIB, DATA];

fn plan() -> LinuxMountPlan {
    compile(
        "/opt/bin/verify",
        "/work/room",
        "/work/room/scratch",
        &[
            Grant::RuntimeFile("/opt/lib/a.so"),
            Grant::LiteralDirectory("/work/data"),
            Grant::DataTree("/work/data/a b\\c"),
        ],
    )
    .unwrap()
}

fn audit(lines: &[&str]) -> Result<()> {
    verify_mount_snapshot(&plan(), (lines.join("\n") + "\n").as_bytes())
}

fn replace(at: usize, from: &str, to: &str) -> Result<()> {
    let mut lines = VALID;
    let modified = lines[at].replace(from, to);
    assert_ne!(modified, lines[at], "check fixture mutation");
    lines[at] = &modified;
    audit(&lines)
}

#[test]
fn exact_mount_set_passes_regardless_of_record_order() {
    audit(&VALID).unwrap();
    let mut shuffled = VALID;
    shuffled.reverse();
    audit(&shuffled).unwrap();
    assert!(replace(0, "10 9", "10 10").is_ok());
}

#[test]
fn no_extra_mounts_or_stacking_including_under_a_tree() {
    for hidden in [
        "16 10 0:5 / /work/data ro,nosuid,nodev,noexec - tmpfs tmpfs rw",
        r"16 15 0:5 / /work/data/a\040b\134c/intruder ro,nosuid,nodev,noexec - tmpfs tmpfs rw",
    ] {
        let mut lines = VALID.to_vec();
        lines.push(hidden);
        assert_eq!(audit(&lines), Err(E::UnexpectedMount));
    }
    let mut lines = VALID.to_vec();
    lines.push(EXEC);
    assert_eq!(audit(&lines), Err(E::DuplicateMount));
    assert_eq!(replace(4, "14 10", "13 10"), Err(E::DuplicateMount));
    assert_eq!(audit(&VALID[..5]), Err(E::MissingMount));
}

#[test]
fn root_and_transitive_shape_cannot_be_weakened() {
    assert_eq!(replace(0, "tmpfs tmpfs", "ext4 /dev/test"), Err(E::BadRoot));
    assert_eq!(replace(0, "0:70 / /", "0:70 /host /"), Err(E::BadRoot));
    assert_eq!(replace(0, "ro,nosuid", "rw,nosuid"), Err(E::BadRoot));
    assert_eq!(replace(0, "10 9", "10 12"), Err(E::BadRoot));
    assert_eq!(replace(2, "12 11", "12 10"), Err(E::WrongParent));
    assert_eq!(replace(4, "14 10", "14 14"), Err(E::WrongParent));
    assert_eq!(replace(0, " - ", " shared:7 - "), Err(E::UnsafePropagation));
    assert_eq!(
        replace(5, "unbindable", "master:7"),
        Err(E::UnsafePropagation)
    );
}

#[test]
fn effective_flags_cannot_expand_permissions_or_disable_expected_exec() {
    for (line, from, to) in [
        (1, ",noexec", ""),
        (2, "rw,nosuid", "ro,nosuid"),
        (2, " /dev/test rw", " /dev/test ro"),
        (2, ",nodev", ""),
        (3, "nodev -", "nodev,noexec -"),
        (4, ",nosuid", ""),
        (5, " ro,nosuid", " rw,nosuid"),
    ] {
        assert_eq!(
            replace(line, from, to),
            Err(E::WrongFlags),
            "{line}: {from}"
        );
    }
    assert_eq!(
        replace(5, "ro,nosuid", "ro,rw,nosuid"),
        Err(E::InvalidRecord)
    );
}

#[test]
fn rejects_malformed_truncated_and_oversized_records() {
    assert_eq!(
        verify_mount_snapshot(&plan(), VALID.join("\n").as_bytes()),
        Err(E::InvalidRecord)
    );
    assert_eq!(
        verify_mount_snapshot(&plan(), &vec![b'x'; MAX_BYTES + 1][..]),
        Err(E::Oversize)
    );
    assert_eq!(
        verify_mount_snapshot(&plan(), &[0xff, b'\n'][..]),
        Err(E::InvalidRecord)
    );
    assert_eq!(replace(1, "8:1", "8:1:2"), Err(E::InvalidRecord));
    assert_eq!(
        replace(1, "/work/room", "/work/./room"),
        Err(E::InvalidRecord)
    );
    assert_eq!(replace(5, r"\040", r"\011"), Err(E::InvalidRecord));
    assert_eq!(replace(5, r"\040", r"\999"), Err(E::InvalidRecord));
}
