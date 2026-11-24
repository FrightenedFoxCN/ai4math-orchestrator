//! Independent Lean batch checks and the reviewed T2 release gate.
use crate::{
    promotion::{PromotionRepo, validate_release_label},
    t2_build::{at_generation, at_snapshot_generation_then},
    trusted_toolchain,
};
use ai4math_broker::{Broker, MAX_MANAGED_T2_THEOREMS, ManagedLeanBatchAudit};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Deserializer, Serialize, de};
use sha2::{Digest, Sha256};
use std::{fmt, fs, io::Read, path::Path};

const MAX_MANIFEST_BYTES: u64 = 8 * 1024;

pub(crate) struct BatchRequest<'a> {
    pub root: &'a Path,
    pub repo: &'a Path,
    pub anchor: &'a str,
    pub reviewed: &'a str,
    pub executable: &'a Path,
    pub exporter_sha256: &'a str,
    pub checker_sha256: &'a str,
    pub targets: &'a Path,
}

#[derive(Serialize)]
pub(crate) struct ManagedBatchReport {
    schema: &'static str,
    advisory_only: bool,
    reviewed_commit: String,
    ledger_event_seq: i64,
    #[serde(flatten)]
    result: ManagedLeanBatchAudit,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Target {
    target_module: String,
    declaration: String,
}

struct ObjectTarget(Target);

impl<'de> Deserialize<'de> for ObjectTarget {
    fn deserialize<D: Deserializer<'de>>(input: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> de::Visitor<'de> for Visitor {
            type Value = ObjectTarget;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an object with target_module and declaration")
            }
            fn visit_map<M: de::MapAccess<'de>>(self, map: M) -> Result<Self::Value, M::Error> {
                Target::deserialize(de::value::MapAccessDeserializer::new(map)).map(ObjectTarget)
            }
        }
        input.deserialize_map(Visitor)
    }
}

fn read_targets(path: &Path) -> Result<Vec<ObjectTarget>> {
    let before = fs::symlink_metadata(path)?;
    ensure!(
        before.is_file(),
        "T2 target manifest must be a regular, non-symlink file"
    );
    ensure!(
        before.len() <= MAX_MANIFEST_BYTES,
        "T2 target manifest exceeds 8 KiB"
    );
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    let opened = file.metadata()?;
    ensure!(
        opened.is_file() && opened.len() <= MAX_MANIFEST_BYTES,
        "invalid or oversized T2 target manifest"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ensure!(
            (opened.dev(), opened.ino()) == (before.dev(), before.ino()),
            "T2 target manifest changed during open"
        );
    }
    let mut bytes = Vec::new();
    file.take(MAX_MANIFEST_BYTES + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_MANIFEST_BYTES,
        "T2 target manifest exceeds 8 KiB"
    );
    parse_targets(&bytes)
}

fn parse_targets(bytes: &[u8]) -> Result<Vec<ObjectTarget>> {
    let targets: Vec<ObjectTarget> =
        serde_json::from_slice(bytes).context("invalid T2 target manifest JSON")?;
    ensure!(
        (1..=MAX_MANAGED_T2_THEOREMS).contains(&targets.len()),
        "T2 target manifest must contain 1..={MAX_MANAGED_T2_THEOREMS} targets"
    );
    Ok(targets)
}

pub(crate) fn run(request: BatchRequest<'_>) -> Result<ManagedBatchReport> {
    let targets = read_targets(request.targets).context("read selected T2 batch targets")?;
    let targets: Vec<_> = targets
        .iter()
        .map(|ObjectTarget(target)| (target.target_module.as_str(), target.declaration.as_str()))
        .collect();
    let (result, (), ledger_event_seq, _) = at_generation(
        request.root,
        request.repo,
        request.anchor,
        request.reviewed,
        |_, _| Ok(()),
        |engine, modules| {
            let toolchain = trusted_toolchain(engine, request.executable, None)?;
            Broker::new()
                .audit_lean_theorems(
                    &toolchain,
                    modules,
                    &targets,
                    request.exporter_sha256,
                    request.checker_sha256,
                )
                .context("managed Lean independent batch check failed")
        },
    )?;
    Ok(ManagedBatchReport {
        schema: "ai4math.t2.managed-batch-check.v1",
        advisory_only: true,
        reviewed_commit: request.reviewed.into(),
        ledger_event_seq,
        result,
    })
}

pub(crate) struct ReleaseRequest<'a> {
    pub root: &'a Path,
    pub repo: &'a Path,
    pub anchor: &'a str,
    pub reviewed: &'a str,
    pub executable: &'a Path,
    pub policy_sha256: &'a str,
    pub compiler_sha256: &'a str,
    pub exporter_sha256: &'a str,
    pub checker_sha256: &'a str,
}

#[derive(Serialize)]
pub(crate) struct ReleaseReport {
    schema: &'static str,
    gate_passed: bool,
    reviewed_commit: String,
    ledger_event_seq: i64,
    policy_sha256: String,
    trust_debt_findings: usize,
    #[serde(flatten)]
    result: ManagedLeanBatchAudit,
}

fn require_pin(value: &str, label: &str) -> Result<()> {
    ensure!(
        value.len() == 64
            && value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "expected {label} SHA-256 must be 64 lowercase hex digits"
    );
    Ok(())
}

fn release_targets(bytes: &[u8], pinned: &str) -> Result<Vec<ObjectTarget>> {
    ensure!(
        format!("{:x}", Sha256::digest(bytes)) == pinned,
        "T2 release policy SHA-256 mismatch"
    );
    parse_targets(bytes).context("invalid reviewed T2 release policy")
}

fn release_debt(state: &ai4math_store::StateStore, seq: i64) -> Result<usize> {
    let report = state
        .scan_trust_debt()
        .context("T2 release trust debt scan failed")?;
    ensure!(
        report.ledger_event_seq == seq,
        "managed ledger changed before T2 release check; retry"
    );
    ensure!(
        report.finding_count == 0,
        "T2 release requires zero audited L4 trust debt findings (found {})",
        report.finding_count
    );
    Ok(report.finding_count)
}

pub(crate) fn release(request: ReleaseRequest<'_>) -> Result<ReleaseReport> {
    release_with(&request, ReleaseMode::Operator, |_, _, _, _, _| Ok(())).map(|(report, ())| report)
}

#[derive(Serialize)]
pub(crate) struct LocalReleaseReport {
    schema: &'static str,
    tag_ref: String,
    tag_oid: String,
    reviewed_commit: String,
    ledger_event_seq: i64,
}

#[derive(Serialize)]
struct LocalReleaseEvidence<'a> {
    schema: &'static str,
    reviewed_commit: &'a str,
    ledger_event_seq: i64,
    policy_sha256: &'a str,
    trust_debt_findings: usize,
    source_set_sha256: &'a str,
    compiler_sha256: &'a str,
    exporter_sha256: &'a str,
    checker_sha256: &'a str,
    checks: Vec<LocalReleaseCheck<'a>>,
}

#[derive(Serialize)]
struct LocalReleaseCheck<'a> {
    target_module: &'a str,
    declaration: &'a str,
    export_sha256: &'a str,
}

pub(crate) fn release_tag(request: ReleaseRequest<'_>, label: &str) -> Result<LocalReleaseReport> {
    validate_release_label(label)?;
    // Re-run the gate rather than accepting an operator- or daemon-supplied report.
    let (report, (tag_ref, tag_oid)) = release_with(
        &request,
        ReleaseMode::Operator,
        |git, path, seq, build, debt| {
            let evidence = LocalReleaseEvidence {
                schema: "ai4math.t2.local-release-evidence.v1",
                reviewed_commit: request.reviewed,
                ledger_event_seq: seq,
                policy_sha256: request.policy_sha256,
                trust_debt_findings: *debt,
                source_set_sha256: &build.build.source_set_sha256,
                compiler_sha256: &build.build.compiler_sha256,
                exporter_sha256: &build.exporter_sha256,
                checker_sha256: request.checker_sha256,
                checks: build
                    .checks
                    .iter()
                    .map(|check| LocalReleaseCheck {
                        target_module: &check.target_module,
                        declaration: &check.declaration,
                        export_sha256: &check.independent_check.export_sha256,
                    })
                    .collect(),
            };
            let evidence = serde_json::to_vec(&evidence)?;
            let repo = git
                .path()
                .to_str()
                .context("invalid managed release repository path")?;
            ai4math_store::StateStore::with_managed_generation_reservation(path, repo, seq, || {
                git.create_release_tag(label, request.reviewed, &evidence)
            })
        },
    )?;
    Ok(LocalReleaseReport {
        schema: "ai4math.t2.local-release.v1",
        tag_ref,
        tag_oid,
        reviewed_commit: report.reviewed_commit,
        ledger_event_seq: report.ledger_event_seq,
    })
}

#[cfg(unix)]
pub(crate) fn release_for_daemon(request: ReleaseRequest<'_>) -> Result<()> {
    crate::t3::arm_daemon_lifeline("T2")?;
    let (report, ()) = release_with(&request, ReleaseMode::Daemon, |_, _, _, _, _| Ok(()))?;
    // Keep proof identities, project paths, build hashes and native diagnostics
    // out of the private worker protocol. The daemon checks the policy pin.
    crate::print_json(serde_json::json!({
        "schema": "ai4math.t2.resident-sample.v2",
        "advisory_only": true,
        "reviewed_commit": report.reviewed_commit,
        "ledger_event_seq": report.ledger_event_seq,
        "compile_ms": report.result.build.compile_ms,
        "module_count": report.result.build.modules.len(),
        "debt_findings": report.trust_debt_findings,
        "independent_checks": report.result.checks.len(),
        "policy_sha256": report.policy_sha256,
    }))
}

enum ReleaseMode {
    Operator,
    #[cfg(unix)]
    Daemon,
}

fn release_with<T>(
    request: &ReleaseRequest<'_>,
    mode: ReleaseMode,
    finish: impl FnOnce(&PromotionRepo, &Path, i64, &ManagedLeanBatchAudit, &usize) -> Result<T>,
) -> Result<(ReleaseReport, T)> {
    for (value, label) in [
        (request.policy_sha256, "T2 release policy"),
        (request.compiler_sha256, "Lean compiler"),
        (request.exporter_sha256, "Lean exporter"),
        (request.checker_sha256, "Nanoda checker"),
    ] {
        require_pin(value, label)?;
    }
    let (result, trust_debt_findings, ledger_event_seq, _, finished) = at_snapshot_generation_then(
        request.root,
        request.repo,
        request.anchor,
        request.reviewed,
        release_debt,
        |engine, snapshot, modules| {
            let bytes = snapshot.t2_release_policy()?;
            let targets = release_targets(&bytes, request.policy_sha256)?;
            let targets: Vec<_> = targets
                .iter()
                .map(|ObjectTarget(target)| {
                    (target.target_module.as_str(), target.declaration.as_str())
                })
                .collect();
            let toolchain = trusted_toolchain(engine, request.executable, None)?;
            ensure!(
                toolchain.compiler_sha256() == request.compiler_sha256,
                "Lean compiler SHA-256 mismatch"
            );
            let toolchain = match mode {
                ReleaseMode::Operator => toolchain,
                #[cfg(unix)]
                ReleaseMode::Daemon => toolchain.with_supervised_process_group()?,
            };
            Broker::new()
                .audit_lean_theorems(
                    &toolchain,
                    modules,
                    &targets,
                    request.exporter_sha256,
                    request.checker_sha256,
                )
                .context("T2 release independent theorem checks failed")
        },
        finish,
    )?;
    Ok((
        ReleaseReport {
            schema: "ai4math.t2.release-check.v1",
            gate_passed: true,
            reviewed_commit: request.reviewed.into(),
            ledger_event_seq,
            policy_sha256: request.policy_sha256.into(),
            trust_debt_findings,
            result,
        },
        finished,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn manifest_requires_exact_objects_and_preserves_review_order() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let file = temp.path().join("targets.json");
        let pair = json!([
            {"target_module":"Z.lean", "declaration":"Z.last"},
            {"declaration":"A.first", "target_module":"A.lean"}
        ]);
        fs::write(&file, serde_json::to_vec(&pair)?)?;
        let targets = read_targets(&file)?;
        assert_eq!(targets[0].0.target_module, "Z.lean");
        assert_eq!(targets[1].0.declaration, "A.first");
        let over = serde_json::to_string(&vec![pair[0].clone(); MAX_MANAGED_T2_THEOREMS + 1])?;
        for invalid in [
            "null",
            "{}",
            "[]",
            "[[\"A.lean\",\"A.x\"]]",
            r#"[{"target_module":"A.lean","declaration":"A.x","extra":true}]"#,
            r#"[{"target_module":"A.lean","declaration":"A.x","declaration":"B.x"}]"#,
            r#"[{"target_module":"A.lean"}]"#,
            r#"[{"target_module":"A.lean","declaration":null}]"#,
            "[{}] []",
            &over,
        ] {
            fs::write(&file, invalid)?;
            assert!(
                read_targets(&file).is_err(),
                "accepted invalid manifest: {invalid}"
            );
        }
        Ok(())
    }

    #[test]
    fn release_policy_pin_commits_to_exact_raw_bytes_before_decoding() -> Result<()> {
        let clean = br#"[{"target_module":"A.lean","declaration":"A.x"}]"#;
        let pin = format!("{:x}", Sha256::digest(clean));
        assert_eq!(release_targets(clean, &pin)?.len(), 1);
        let whitespace = [clean.as_slice(), b"\n"].concat();
        let err = release_targets(&whitespace, &pin).err().expect("stale pin");
        assert!(err.to_string().contains("SHA-256 mismatch"));
        let invalid = b"[]";
        let pin = format!("{:x}", Sha256::digest(invalid));
        assert!(release_targets(invalid, &pin).is_err());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn manifest_rejects_links_and_caps_bytes() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let file = temp.path().join("targets.json");
        let link = temp.path().join("alias.json");
        let mut valid = br#"[{"target_module":"A.lean","declaration":"A.x"}]"#.to_vec();
        valid.resize(MAX_MANIFEST_BYTES as usize, b' ');
        fs::write(&file, &valid)?;
        assert_eq!(read_targets(&file)?.len(), 1);
        std::os::unix::fs::symlink(&file, &link)?;
        assert!(read_targets(&link).is_err());
        assert!(read_targets(temp.path()).is_err());
        valid.push(b' ');
        fs::write(&file, &valid)?;
        assert!(read_targets(&file).is_err());
        Ok(())
    }
}
