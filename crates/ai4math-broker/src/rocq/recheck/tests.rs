use super::*;

#[test]
fn plans_only_explicit_recursive_checks_in_a_fixed_library_scope() -> Result<()> {
    let objects = Path::new("/build with spaces/output");
    let roots = vec![
        (PathBuf::from("/paired/theories"), "Corelib"),
        (PathBuf::from("/paired/user-contrib/Stdlib"), "Stdlib"),
    ];
    let files = [objects.join("Fixture.vo"), objects.join("Later.vo")];
    assert_eq!(
        args(&roots, objects, &files)?,
        [
            "-boot",
            "-silent",
            "-bytecode-compiler",
            "no",
            "-Q",
            "/paired/theories",
            "Corelib",
            "-Q",
            "/paired/user-contrib/Stdlib",
            "Stdlib",
            "-Q",
            "/build with spaces/output",
            "Ai4mathProject",
            "/build with spaces/output/Fixture.vo",
            "/build with spaces/output/Later.vo",
        ]
    );
    Ok(())
}

#[test]
fn rejects_unexpected_missing_empty_oversize_or_aliased_objects_before_launch() -> Result<()> {
    let room = tempfile::tempdir()?;
    let dir = room.path();
    let fixture = dir.join("Fixture.vo");
    fs::write(
        &fixture,
        b"not an object; only validating the filesystem plan",
    )?;
    assert_eq!(
        object_files(dir, &["Fixture.v"])?,
        std::slice::from_ref(&fixture)
    );
    for sources in [
        &[][..],
        &["Fixture.v", "Missing.v"],
        &["Fixture.v", "FIXTURE.v"],
        &["Fixture.v", "Ai4mathRocqAudit.v"],
        &["Fixture.v", "Other/Fixture.v"],
    ] {
        assert!(object_files(dir, sources).is_err());
    }
    let unexpected = dir.join("unexpected.txt");
    fs::write(&unexpected, b"extra")?;
    assert!(object_files(dir, &["Fixture.v"]).is_err());
    fs::remove_file(unexpected)?;
    fs::File::create(&fixture)?;
    assert!(object_files(dir, &["Fixture.v"]).is_err());
    fs::OpenOptions::new()
        .write(true)
        .open(&fixture)?
        .set_len(golden::ARTIFACT_CAP + 1)?;
    assert!(object_files(dir, &["Fixture.v"]).is_err());
    #[cfg(unix)]
    {
        let other = tempfile::tempdir()?;
        let alias = other.path().join("alias.vo");
        fs::hard_link(&fixture, &alias)?;
        fs::write(&fixture, b"object")?;
        assert!(object_files(dir, &["Fixture.v"]).is_err());
        fs::remove_file(&fixture)?;
        std::os::unix::fs::symlink(alias, fixture)?;
        assert!(object_files(dir, &["Fixture.v"]).is_err());
    }
    Ok(())
}

#[test]
fn fails_closed_on_malformed_or_mismatched_checker_pin() -> Result<()> {
    let binary = tempfile::NamedTempFile::new()?;
    fs::write(binary.path(), b"test pin bytes")?;
    let actual = file_digest(binary.path())?;
    assert_eq!(pin_checker(binary.path(), &actual)?, actual);
    for bad in [
        "",
        &actual.to_ascii_uppercase(),
        &"x".repeat(64),
        &"0".repeat(64),
    ] {
        assert!(pin_checker(binary.path(), bad).is_err());
    }
    Ok(())
}

#[test]
fn accepts_only_the_paired_native_checker_path() -> Result<()> {
    let prefix = tempfile::tempdir()?;
    let prefix = prefix.path().canonicalize()?;
    let bin = prefix.join("bin");
    fs::create_dir(&bin)?;
    let install = Install {
        prefix: prefix.clone(),
        data: prefix.join("lib/coq"),
        runtime: prefix.join("lib/rocq-runtime"),
    };
    let paired = bin.join("coqchk");
    let wrong = bin.join("rocqworker");
    // Header-only verifier-path fixtures. Never executed.
    let mach_o_magic = [0xcf, 0xfa, 0xed, 0xfe];
    fs::write(&paired, mach_o_magic)?;
    fs::write(&wrong, mach_o_magic)?;
    assert_eq!(resolve_checker(&install, &paired)?, paired);
    assert!(resolve_checker(&install, &wrong).is_err());
    assert!(resolve_checker(&install, Path::new("coqchk")).is_err());
    fs::write(&paired, b"#!/bin/sh\nexit 0\n")?;
    assert!(resolve_checker(&install, &paired).is_err());
    Ok(())
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "requires native Rocq 9.3 with Stdlib and Seatbelt; set AI4MATH_TEST_ROCQ{,_DATA,_READS,_CHECKER,_CHECKER_SHA256}"]
fn installed_coqchk_rechecks_a_fresh_build_via_seatbelt() -> Result<()> {
    use crate::Broker;
    use std::{env, time::Duration};

    let driver = env::var_os("AI4MATH_TEST_ROCQ").context("native Rocq 9.3 driver path")?;
    let data = env::var_os("AI4MATH_TEST_ROCQ_DATA").context("trusted Rocq data path")?;
    let checker =
        PathBuf::from(env::var_os("AI4MATH_TEST_ROCQ_CHECKER").context("native coqchk path")?);
    let pin = env::var("AI4MATH_TEST_ROCQ_CHECKER_SHA256").context("trusted checker's pin")?;
    let mut tool = Toolchain::rocq(driver, data)?;
    if let Some(reads) = env::var_os("AI4MATH_TEST_ROCQ_READS") {
        for read in env::split_paths(&reads) {
            tool = tool.with_runtime_read(read)?;
        }
    }
    let deadline = Some(Instant::now() + Duration::from_secs(60));
    let install = tool.rocq_install.as_ref().unwrap();
    let cli = resolve_checker(install, &checker)?;
    pin_checker(&cli, &pin)?;
    let probe_room = tempfile::tempdir()?;
    let probe_scratch = tempfile::tempdir_in(probe_room.path())?;
    let version = sandbox::run_grouped_until(
        sandbox::Invocation {
            executable: &cli,
            args: &["-v".into()],
            room: probe_room.path(),
            writable: probe_scratch.path(),
            reads: &[],
            env: &[],
            timeout: TIMEOUT,
        },
        &[],
        &[],
        tool.sandbox_group,
        deadline,
    )
    .context("query paired coqchk CLI version using Seatbelt")?;
    assert!(
        version.starts_with("The Rocq Proof Checker, version 9.3."),
        "{version}"
    );

    let room = tempfile::tempdir()?;
    let modules = [
        (
            "Dep.v",
            "From Stdlib Require Import Arith.PeanoNat.\nDefinition closed : True := I.\n",
        ),
        (
            "Fixture.v",
            "Require Import Dep.\nTheorem target : True.\nProof. exact Dep.closed. Qed.\n",
        ),
        ("Unrelated.v", "Definition standalone : True := I.\n"),
    ];
    crate::validate_modules_for_tool(&tool, &modules, None)?;
    let broker = Broker::new();
    let mut build = ModuleBuild::new(&tool, room.path())?;
    build.compile_all(&broker, &modules)?;
    let expected = modules.map(|(source, _)| source);
    let result = recheck_generated_objects(&build, &expected, &checker, &pin, deadline)?;
    assert_eq!(result.checker_sha256(), pin);
    super::super::audit_target(&tool, &build.output, "Fixture.v", "target", deadline)?;
    Ok(())
}
