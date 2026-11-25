use super::{
    Grant::*,
    MountKind::{ExecutableFile, ReadOnlyRoom, SyntheticDirectory, WritableScratch},
    MountPlanError as E, *,
};

const EXE: &str = "/toolchain/bin/verifier";
const ROOM: &str = "/tmp/ai4math-room";
const OUT: &str = "/tmp/ai4math-room/out";

#[test]
fn literal_directories_are_synthetic_with_only_explicit_descendants() {
    let grants = [
        LiteralDirectory("/toolchain/runtime"),
        DataTree("/toolchain/runtime/objects"),
        DataFile("/toolchain/runtime/catalog"),
        RuntimeFile("/toolchain/lib/libverify.so"),
    ];
    let plan = compile(EXE, ROOM, OUT, &grants).unwrap();
    assert_eq!(plan.exec_target(), EXE);
    let paths = plan
        .operations()
        .iter()
        .map(|op| (op.target(), op.kind()))
        .collect::<Vec<_>>();
    assert_eq!(
        paths,
        [
            ("/tmp", SyntheticDirectory),
            (ROOM, ReadOnlyRoom),
            (OUT, WritableScratch),
            ("/toolchain", SyntheticDirectory),
            ("/toolchain/bin", SyntheticDirectory),
            (EXE, ExecutableFile),
            ("/toolchain/lib", SyntheticDirectory),
            ("/toolchain/lib/libverify.so", MountKind::RuntimeFile),
            ("/toolchain/runtime", SyntheticDirectory),
            ("/toolchain/runtime/catalog", MountKind::DataFile),
            ("/toolchain/runtime/objects", MountKind::DataTree),
        ]
    );
    for op in plan.operations() {
        assert_eq!(
            op.source(),
            (op.kind() != SyntheticDirectory).then_some(op.target())
        );
        let flags = op.kind().flags();
        assert!(flags.no_suid && flags.no_dev);
        assert_eq!(flags.read_only, op.target() != OUT);
        assert_eq!(
            flags.no_exec,
            op.target() != EXE && op.target() != "/toolchain/lib/libverify.so"
        );
    }
    let reversed = grants.into_iter().rev().collect::<Vec<_>>();
    assert_eq!(plan, compile(EXE, ROOM, OUT, &reversed).unwrap());
}

#[test]
fn duplicate_or_conflicting_mounts_are_never_promoted_to_a_wider_grant() {
    for grants in [
        vec![DataTree("/toolchain/bin")],
        vec![RuntimeFile("/toolchain/bin/verifier/lib")],
        vec![
            DataTree("/toolchain/objects"),
            LiteralDirectory("/toolchain/objects/partial"),
        ],
        vec![
            DataTree("/toolchain/objects"),
            RuntimeFile("/toolchain/objects/lib.so"),
        ],
        vec![DataTree("/tmp/ai4math-room/input")],
        vec![DataFile("/tmp/ai4math-room/out/extra")],
    ] {
        assert_eq!(
            compile(EXE, ROOM, OUT, &grants),
            Err(E::ConflictingTargets),
            "{grants:?}"
        );
    }
    assert_eq!(
        compile(EXE, ROOM, OUT, &[DataFile(EXE)]),
        Err(E::DuplicateTarget)
    );
    assert_eq!(
        compile(EXE, ROOM, "/tmp/ai4math-room-other/out", &[]),
        Err(E::ScratchNotDirectChild)
    );
    assert_eq!(
        compile(EXE, ROOM, "/tmp/ai4math-room/nested/out", &[]),
        Err(E::ScratchNotDirectChild)
    );
    assert!(compile(EXE, ROOM, OUT, &[DataTree("/tmp/ai4math-room-other")]).is_ok());
}

#[test]
fn broad_paths_aliases_and_reserved_roots_are_rejected() {
    for path in [
        "usr/lib",
        "/",
        "/toolchain//lib",
        "/toolchain/./lib",
        "/toolchain/../lib",
        "/toolchain/lib/",
        "/bad\n/path",
        "/bad\0/path",
    ] {
        assert_eq!(
            compile(EXE, ROOM, OUT, &[LiteralDirectory(path)]),
            Err(E::InvalidPath),
            "{path:?}"
        );
    }
    for path in [
        "/usr",
        "/usr/lib",
        "/usr/local",
        "/proc/something",
        "/sys/fs",
        "/etc/ssl",
        "/run/user",
        "/dev/null",
        "/home",
    ] {
        assert_eq!(
            compile(EXE, ROOM, OUT, &[DataTree(path)]),
            Err(E::ReservedPath),
            "{path:?}"
        );
    }
}

#[test]
fn both_user_grants_and_synthetic_parents_are_bounded() {
    let grants = vec![LiteralDirectory("/a"); MAX_EXPLICIT_GRANTS + 1];
    assert_eq!(compile(EXE, ROOM, OUT, &grants), Err(E::TooManyGrants));
    let deep = format!("/x{}", "/a".repeat(MAX_OPERATIONS + 1));
    assert_eq!(
        compile(EXE, ROOM, OUT, &[DataFile(&deep)]),
        Err(E::TooManyOperations)
    );
    let oversize = format!("/a/{}", "b".repeat(MAX_PATH_BYTES));
    assert_eq!(
        compile(EXE, ROOM, OUT, &[DataFile(&oversize)]),
        Err(E::InvalidPath)
    );
}
