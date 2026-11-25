use super::*;
use std::{
    ffi::CString,
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::PathBuf,
    process::{Command, Stdio},
    time::Duration,
};

const BASE: &str = "0123456789abcdef0123456789abcdef01234567";
const DIGEST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const REQUEST: &str = "owner_approval-1";

fn create_key(dir: &Path, name: &str) -> Result<(PathBuf, TrustedOwnerKey)> {
    let private = dir.join(name);
    let output = Command::new(unix::SSH_KEYGEN)
        .env_clear()
        .args([
            "-q",
            "-t",
            "ed25519",
            "-N",
            "",
            "-C",
            "synthetic test fixture",
            "-f",
        ])
        .arg(&private)
        .stdin(Stdio::null())
        .output()?;
    ensure!(
        output.status.success(),
        "generate synthetic fixture key: {:?}",
        output.stderr
    );
    let public = private.with_extension("pub");
    Ok((private, TrustedOwnerKey::read(&public)?))
}

fn sign_fixture(
    dir: &Path,
    key: &Path,
    name: &str,
    namespace: &str,
    data: &[u8],
) -> Result<Vec<u8>> {
    let path = dir.join(name);
    fs::write(&path, data)?;
    let output = Command::new(unix::SSH_KEYGEN)
        .env_clear()
        .args(["-Y", "sign", "-f"])
        .arg(key)
        .args(["-n", namespace])
        .arg(&path)
        .stdin(Stdio::null())
        .output()?;
    ensure!(
        output.status.success(),
        "sign synthetic fixture: {:?}",
        output.stderr
    );
    read_signature(&path.with_extension("sig"))
}

#[test]
fn verifies_exact_archive_context_with_only_the_pinned_key() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let root = tmp.path().join("project");
    let repo = tmp.path().join("repo");
    fs::create_dir(&root)?;
    fs::create_dir(&repo)?;
    let expires = unix_seconds()? + 600;
    let expected = ArchiveApproval::new(&root, &repo, BASE, DIGEST, REQUEST, expires)?;
    let (private, trusted) = create_key(tmp.path(), "trusted_key")?;
    let (_, other) = create_key(tmp.path(), "other_key")?;
    let sig = sign_fixture(
        tmp.path(),
        &private,
        "archive",
        NAMESPACE,
        expected.signing_bytes(),
    )?;
    expected.verify(&trusted, &sig)?;
    assert!(expected.verify(&other, &sig).is_err());
    let expired = expected
        .verify_at(&trusted, &sig, || Ok(expires))
        .unwrap_err();
    assert!(expired.to_string().contains("expired"));
    assert!(expected.verify(&trusted, &[]).is_err());
    assert!(
        expected
            .verify(&trusted, b"malformed synthetic data")
            .is_err()
    );
    assert!(
        expected
            .verify(&trusted, &[b'A'; MAX_SIGNATURE_BYTES + 1])
            .is_err()
    );

    let wrong_base = "f".repeat(40);
    let wrong_digest = "f".repeat(64);
    for (r, g, b, d, id, t) in [
        (tmp.path(), repo.as_path(), BASE, DIGEST, REQUEST, expires),
        (root.as_path(), tmp.path(), BASE, DIGEST, REQUEST, expires),
        (&root, &repo, &wrong_base, DIGEST, REQUEST, expires),
        (&root, &repo, BASE, &wrong_digest, REQUEST, expires),
        (&root, &repo, BASE, DIGEST, "wrong-request", expires),
        (&root, &repo, BASE, DIGEST, REQUEST, expires + 1),
    ] {
        assert!(
            ArchiveApproval::new(r, g, b, d, id, t)?
                .verify(&trusted, &sig)
                .is_err()
        );
    }
    let wrong_namespace = sign_fixture(
        tmp.path(),
        &private,
        "domain",
        "other-domain",
        expected.signing_bytes(),
    )?;
    assert!(expected.verify(&trusted, &wrong_namespace).is_err());
    let resume = std::str::from_utf8(expected.signing_bytes())?.replace(
        "\"action\":\"archive_breaker\"",
        "\"action\":\"resume_as_is\"",
    );
    let resume = sign_fixture(tmp.path(), &private, "resume", NAMESPACE, resume.as_bytes())?;
    assert!(expected.verify(&trusted, &resume).is_err());
    Ok(())
}

#[test]
fn signing_bytes_are_canonical_and_directory_paths_are_bound() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let repo = temp.path().join("repo \"β\"");
    fs::create_dir(&repo)?;
    let link = temp.path().join("alias");
    symlink(&repo, &link)?;
    let expires = 1_900_000_000;
    let signing = ArchiveApproval::new(temp.path(), &link, BASE, DIGEST, REQUEST, expires)?;
    let root_json = serde_json::to_string(&temp.path().canonicalize()?)?;
    let repo_json = serde_json::to_string(&repo.canonicalize()?)?;
    let canonical = format!(
        "{{\"schema\":\"ai4math-owner-archive-v1\",\"action\":\"archive_breaker\",\"project_root\":{root_json},\"repo\":{repo_json},\"base_commit\":\"{BASE}\",\"adr_sha256\":\"{DIGEST}\",\"request_id\":\"{REQUEST}\",\"expires_unix\":{expires}}}"
    );
    assert_eq!(signing.signing_bytes(), canonical.as_bytes());
    assert!(ArchiveApproval::new(temp.path(), &repo, "a", DIGEST, REQUEST, expires).is_err());
    assert!(
        ArchiveApproval::new(
            temp.path(),
            &repo,
            BASE,
            &DIGEST.to_uppercase(),
            REQUEST,
            expires
        )
        .is_err()
    );
    assert!(ArchiveApproval::new(temp.path(), &repo, BASE, DIGEST, "id\nbad", expires).is_err());
    assert!(
        ArchiveApproval::new(temp.path(), &repo, BASE, DIGEST, &"a".repeat(129), expires).is_err()
    );
    assert!(ArchiveApproval::new(temp.path(), &repo, BASE, DIGEST, REQUEST, 0).is_err());
    let file = temp.path().join("file");
    fs::write(&file, [])?;
    assert!(ArchiveApproval::new(temp.path(), &file, BASE, DIGEST, REQUEST, expires).is_err());
    assert!(
        ArchiveApproval::new(
            temp.path(),
            &temp.path().join("missing"),
            BASE,
            DIGEST,
            REQUEST,
            expires
        )
        .is_err()
    );
    Ok(())
}

#[test]
fn rejects_nonregular_oversized_and_multiple_key_inputs() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let file = tmp.path().join("input");
    let alias = tmp.path().join("link");
    let fifo = tmp.path().join("fifo");
    let fifo_path = CString::new(fifo.as_os_str().as_encoded_bytes())?;
    // SAFETY: valid, NUL-terminated path in the fixture directory.
    assert_eq!(unsafe { libc::mkfifo(fifo_path.as_ptr(), 0o600) }, 0);
    fs::write(&file, [b'x'; MAX_SIGNATURE_BYTES + 1])?;
    symlink(&file, &alias)?;
    for invalid in [&file, &alias, &fifo, &tmp.path().to_owned()] {
        assert!(read_signature(invalid).is_err());
        assert!(TrustedOwnerKey::read(invalid).is_err());
    }
    let key = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    assert!(TrustedOwnerKey::parse(format!("{key} optional comment\r\n").as_bytes()).is_ok());
    assert!(TrustedOwnerKey::parse(format!("{key}\n{key}\n").as_bytes()).is_err());
    assert!(TrustedOwnerKey::parse(format!("allowed-principal {key}\n").as_bytes()).is_err());
    Ok(())
}

const FIXTURE_KEY: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

#[test]
fn managed_anchor_is_fixed_and_requires_admitted_files_and_directories() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let root = tmp.path().canonicalize()?;
    let runtime = root.join(".runtime");
    let key = runtime.join("owner-archive.pub");
    let alternate = root.join("custom.pub");
    fs::write(&alternate, FIXTURE_KEY)?;
    assert!(TrustedOwnerKey::read_managed(&root).is_err());
    fs::create_dir(&runtime)?;
    assert!(TrustedOwnerKey::read_managed(&root).is_err());

    fs::create_dir(&key)?;
    assert!(TrustedOwnerKey::read_managed(&root).is_err());
    fs::remove_dir(&key)?;
    symlink(&alternate, &key)?;
    assert!(TrustedOwnerKey::read_managed(&root).is_err());
    fs::remove_file(&key)?;
    fs::hard_link(&alternate, &key)?;
    assert!(TrustedOwnerKey::read_managed(&root).is_err());
    fs::remove_file(&key)?;

    fs::write(&key, [b'x'; MAX_KEY_BYTES + 1])?;
    assert!(TrustedOwnerKey::read_managed(&root).is_err());
    fs::write(&key, format!("{FIXTURE_KEY} deployer comment\n"))?;
    let exact = "31ec99886f73eb23c4ea0836841fe7b035c3e1a7b0d5f364019d880abd380cf2";
    assert_eq!(TrustedOwnerKey::read_managed(&root)?.fingerprint(), exact);
    fs::write(&key, format!("{FIXTURE_KEY}\r\n"))?;
    assert_eq!(TrustedOwnerKey::read_managed(&root)?.fingerprint(), exact);

    let root_alias = root.join("alias");
    symlink(&root, &root_alias)?;
    assert!(TrustedOwnerKey::read_managed(&root_alias).is_err());
    assert!(TrustedOwnerKey::read_managed(&root.join(".")).is_err());
    for (path, safe_mode) in [(&key, 0o600), (&runtime, 0o700), (&root, 0o700)] {
        fs::set_permissions(path, fs::Permissions::from_mode(safe_mode | 0o020))?;
        assert!(TrustedOwnerKey::read_managed(&root).is_err());
        fs::set_permissions(path, fs::Permissions::from_mode(safe_mode))?;
    }
    let alternate_runtime = root.join("alternate-runtime");
    fs::rename(&runtime, &alternate_runtime)?;
    symlink(&alternate_runtime, &runtime)?;
    assert!(TrustedOwnerKey::read_managed(&root).is_err());
    Ok(())
}

#[test]
#[cfg(target_os = "macos")]
fn managed_anchor_rejects_extended_write_acl_with_private_mode() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let root = tmp.path().canonicalize()?;
    let runtime = root.join(".runtime");
    fs::create_dir(&runtime)?;
    let key = runtime.join("owner-archive.pub");
    fs::write(&key, FIXTURE_KEY)?;
    fs::set_permissions(&key, fs::Permissions::from_mode(0o600))?;
    assert!(TrustedOwnerKey::read_managed(&root).is_ok());
    ensure!(
        Command::new("/bin/chmod")
            .args(["+a", "everyone allow write,delete"])
            .arg(&key)
            .status()?
            .success(),
        "set synthetic extended ACL fixture"
    );
    assert_eq!(fs::metadata(&key)?.permissions().mode() & 0o777, 0o600);
    let err = TrustedOwnerKey::read_managed(&root)
        .err()
        .context("ACL must deny admission")?;
    assert!(format!("{err:#}").contains("ACL grants"), "{err:#}");
    Ok(())
}

#[test]
fn managed_verification_checks_full_expiry_window_on_both_sides() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let root = tmp.path().canonicalize()?;
    let runtime = root.join(".runtime");
    let repo = root.join("repo");
    fs::create_dir(&runtime)?;
    fs::create_dir(&repo)?;
    let (private, diagnostic_key) = create_key(tmp.path(), "managed-fixture")?;
    fs::copy(
        private.with_extension("pub"),
        runtime.join("owner-archive.pub"),
    )?;
    let key = TrustedOwnerKey::read_managed(&root)?;
    let expires = unix_seconds()? + 300;
    let expected = ArchiveApproval::new(&root, &repo, BASE, DIGEST, REQUEST, expires)?;
    let sig = sign_fixture(
        tmp.path(),
        &private,
        "managed",
        NAMESPACE,
        expected.signing_bytes(),
    )?;
    assert_eq!(
        expected.message_sha256(),
        format!("{:x}", Sha256::digest(expected.signing_bytes()))
    );
    expected.check_fresh()?;
    expected.verify_fresh(&key, &sig)?;
    let verify_with = |first, second| {
        let mut times = [first, second].into_iter();
        expected.verify_fresh_at(&key, &sig, || times.next().context("unexpected clock read"))
    };
    verify_with(expires - 600, expires - 1)?;
    assert!(
        verify_with(expires - 601, expires - 1)
            .unwrap_err()
            .to_string()
            .contains("more than 10 minutes")
    );
    assert!(
        verify_with(expires - 1, expires)
            .unwrap_err()
            .to_string()
            .contains("expired")
    );
    assert!(
        verify_with(expires - 600, expires - 601)
            .unwrap_err()
            .to_string()
            .contains("more than 10 minutes")
    );
    assert!(
        verify_with(expires, expires)
            .unwrap_err()
            .to_string()
            .contains("expired")
    );
    // Diagnostic mode still verifies the exact signed bytes regardless of the
    // upper horizon. Managed mode accepts keys only from the signed root.
    expected.verify_at(&diagnostic_key, &sig, || Ok(expires - 601))?;
    let other_runtime = repo.join(".runtime");
    fs::create_dir(&other_runtime)?;
    fs::copy(
        private.with_extension("pub"),
        other_runtime.join("owner-archive.pub"),
    )?;
    let other_key = TrustedOwnerKey::read_managed(&repo)?;
    assert_eq!(key.fingerprint(), other_key.fingerprint());
    assert!(
        expected
            .verify_fresh(&other_key, &sig)
            .unwrap_err()
            .to_string()
            .contains("different managed root")
    );
    Ok(())
}

#[test]
fn timed_out_child_is_terminated_and_reaped() -> Result<()> {
    let mut child = Command::new("/bin/sleep")
        .arg("30")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let err = unix::wait_bounded(&mut child, Duration::from_millis(30)).unwrap_err();
    assert!(err.to_string().contains("timed out"));
    assert!(!child.try_wait()?.context("child must be reaped")?.success());
    Ok(())
}
