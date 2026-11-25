#![cfg(unix)]
use anyhow::{Result, ensure};
use std::{
    fs,
    path::Path,
    process::{Command, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};

fn run(program: impl AsRef<Path>, args: &[&str]) -> Result<Vec<u8>> {
    let result = Command::new(program.as_ref())
        .args(args)
        .stdin(Stdio::null())
        .output()?;
    ensure!(
        result.status.success(),
        "{}: {}",
        args[0],
        String::from_utf8_lossy(&result.stderr)
    );
    Ok(result.stdout)
}

#[test]
fn offline_archive_signature_binds_request_and_does_not_apply() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let root = tmp.path().join("project");
    let repo = tmp.path().join("project.git");
    fs::create_dir(&root)?;
    run("/usr/bin/git", &["init", "--bare", repo.to_str().unwrap()])?;
    let key = tmp.path().join("key");
    let public = tmp.path().join("key.pub");
    let message = tmp.path().join("message");
    let signature = tmp.path().join("message.sig");
    let cli = env!("CARGO_BIN_EXE_ai4math");
    let expires = (SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() + 180).to_string();
    let base = "a".repeat(40);
    let digest = "b".repeat(64);
    let fields = [
        root.to_str().unwrap(),
        repo.to_str().unwrap(),
        &base,
        &digest,
        "owner-1",
        &expires,
    ];
    let mut arguments = vec!["owner-archive-signing-message"];
    arguments.extend(fields);
    let bytes = run(cli, &arguments)?;
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&bytes)?["action"],
        "archive_breaker"
    );
    fs::write(&message, bytes)?;
    run(
        "/usr/bin/ssh-keygen",
        &["-q", "-t", "ed25519", "-N", "", "-f", key.to_str().unwrap()],
    )?;
    run(
        "/usr/bin/ssh-keygen",
        &[
            "-Y",
            "sign",
            "-n",
            "ai4math-owner-archive-v1",
            "-f",
            key.to_str().unwrap(),
            message.to_str().unwrap(),
        ],
    )?;
    arguments[0] = "owner-archive-verify";
    arguments.extend([public.to_str().unwrap(), signature.to_str().unwrap()]);
    let result = serde_json::from_slice::<serde_json::Value>(&run(cli, &arguments)?)?;
    assert_eq!(
        result,
        serde_json::json!({"signature_valid":true,"diagnostic_only":true,"applied":false})
    );
    arguments[5] = "owner-2";
    assert!(run(cli, &arguments).is_err());
    assert!(!root.join(".runtime").exists());
    assert!(!repo.join("ai4math-promotion.lock").exists());
    Ok(())
}
