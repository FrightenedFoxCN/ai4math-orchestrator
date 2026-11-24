//! Operator-provisioned T0 verification slots. RPC callers supply only a slot ID and proof body.
use ai4math_broker::{
    AgdaMode, Broker, BrokerLoad, ProbeAssessment, ProjectNativeBusy, ProjectNativeGate, Toolchain,
};
use ai4math_core::{Engine, FrozenTarget, ProofBodySubmission};
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    fs::OpenOptions,
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

pub const PROOF_BODY_MAX: usize = 256 * 1024;
const CATALOG_MAX: u64 = 4 * 1024 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Catalog {
    version: u8,
    targets: Vec<TargetConfig>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TargetConfig {
    id: String,
    frozen: FrozenTarget,
    executable: PathBuf,
    #[serde(default)]
    runtime_reads: Vec<PathBuf>,
    agda_data: Option<PathBuf>,
    rocq_data: Option<PathBuf>,
}
struct Target {
    frozen: FrozenTarget,
    toolchain: Toolchain,
}
pub struct Probes {
    targets: BTreeMap<String, Target>,
    broker: Broker,
    native_gate: ProjectNativeGate,
}

pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

impl Probes {
    pub fn open(root: &Path, catalog: Option<&Path>) -> Result<Self> {
        let mut targets = BTreeMap::new();
        if let Some(path) = catalog {
            let input = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(path)?;
            let meta = input.metadata()?;
            ensure!(
                meta.is_file()
                    && meta.uid() == unsafe { libc::geteuid() }
                    && meta.nlink() == 1
                    && meta.mode() & 0o022 == 0
                    && meta.len() <= CATALOG_MAX,
                "untrusted proof catalogue ownership, permissions, or size"
            );
            let mut bytes = Vec::new();
            input.take(CATALOG_MAX + 1).read_to_end(&mut bytes)?;
            ensure!(
                bytes.len() as u64 <= CATALOG_MAX,
                "proof catalogue too large"
            );
            let parsed: Catalog =
                serde_json::from_slice(&bytes).context("parse operator proof catalogue")?;
            ensure!(
                parsed.version == 1 && parsed.targets.len() <= 64,
                "unsupported proof catalogue version or size"
            );
            for config in parsed.targets {
                ensure!(
                    valid_id(&config.id)
                        && config.executable.is_absolute()
                        && config.runtime_reads.len() <= 16,
                    "invalid proof slot metadata"
                );
                let mut toolchain = match (
                    config.frozen.engine,
                    config.agda_data.as_ref(),
                    config.rocq_data.as_ref(),
                ) {
                    (Engine::Lean4, None, None) => Toolchain::lean(config.executable)?,
                    (Engine::Agda, Some(data), None) if data.is_absolute() => {
                        Toolchain::agda(config.executable, AgdaMode::WithoutK)?
                            .with_agda_data(data)?
                    }
                    (Engine::Rocq, None, Some(data)) if data.is_absolute() => {
                        Toolchain::rocq(config.executable, data)?
                    }
                    _ => anyhow::bail!(
                        "proof slot requires one engine and its corresponding absolute data directory (no data directory for Lean)"
                    ),
                };
                for path in config.runtime_reads {
                    toolchain = toolchain.with_runtime_read(path)?;
                }
                ensure!(
                    targets
                        .insert(
                            config.id,
                            Target {
                                frozen: config.frozen,
                                toolchain
                            }
                        )
                        .is_none(),
                    "duplicate proof slot ID"
                );
            }
        }
        Ok(Self {
            targets,
            broker: Broker::new(),
            native_gate: ProjectNativeGate::new(root),
        })
    }
    pub fn has(&self, id: &str) -> bool {
        self.targets.contains_key(id)
    }
    pub fn load(&self) -> Result<Option<BrokerLoad>> {
        if self.targets.is_empty() {
            return Ok(None);
        }
        self.broker.load().map(Some)
    }
    pub fn check(&self, id: &str, proof_body: String) -> Result<ProbeAssessment> {
        let target = self.targets.get(id).context("unknown proof slot")?;
        let _native = match self.native_gate.try_fast() {
            Ok(lease) => lease,
            Err(error) if error.is::<ProjectNativeBusy>() => {
                return Ok(ProbeAssessment::Unavailable);
            }
            Err(error) => return Err(error),
        };
        let submission = ProofBodySubmission {
            target_id: target.frozen.target_id.clone(),
            proof_body,
        };
        Ok(self
            .broker
            .assess_probe(&target.toolchain, &target.frozen, &submission))
    }
}
