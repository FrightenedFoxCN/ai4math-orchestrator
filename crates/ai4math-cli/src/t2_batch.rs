//! Operator-only manifest for a selected, independently checked Lean batch.
use crate::{t2_build::at_generation, trusted_toolchain};
use ai4math_broker::{Broker, MAX_MANAGED_T2_THEOREMS, ManagedLeanBatchAudit};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Deserializer, Serialize, de};
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
    let targets: Vec<ObjectTarget> =
        serde_json::from_slice(&bytes).context("invalid T2 target manifest JSON")?;
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
