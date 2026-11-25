//! Operator-only observation; no trust transition, cache publish or release gate.
use crate::{print_json, t2_build, trusted_toolchain};
use ai4math_broker::{Broker, ordered_source_set_sha256};
use ai4math_core::Engine;
use anyhow::{Result, ensure};
use std::path::Path;

#[cfg(test)]
mod tests;

pub(crate) struct Inputs<'a> {
    pub compiler: &'a Path,
    pub data: &'a Path,
    pub compiler_sha256: &'a str,
    pub checker: &'a Path,
    pub checker_sha256: &'a str,
}

pub(crate) fn check(
    root: &Path,
    repo: &Path,
    reviewed: &str,
    target: (&str, &str),
    inputs: Inputs<'_>,
) -> Result<()> {
    let (module, theorem) = target;
    let ((checked_modules, sources_sha256), generation) = t2_build::at_engine_snapshot_generation(
        root,
        repo,
        (Engine::Rocq, module),
        reviewed,
        |_, files| {
            ensure!(
                inputs.compiler.is_absolute()
                    && inputs.data.is_absolute()
                    && inputs.checker.is_absolute(),
                "Rocq T2 requires absolute operator toolchain paths"
            );
            let tool = trusted_toolchain(Engine::Rocq, inputs.compiler, inputs.data.to_str())?;
            let count = Broker::new().check_rocq_t2(
                &tool,
                files,
                target,
                inputs.compiler_sha256,
                (inputs.checker, inputs.checker_sha256),
            )?;
            Ok((count, ordered_source_set_sha256(files)))
        },
    )?;
    print_json(serde_json::json!({
        "schema": "ai4math.rocq-t2-object-check.v1",
        "advisory_only": true,
        "reviewed_commit": reviewed,
        "ledger_event_seq": generation,
        "module": module,
        "theorem": theorem,
        "target_assumptions": "closed",
        "checked_modules": checked_modules,
        "sources_sha256": sources_sha256,
        "compiler_sha256": inputs.compiler_sha256,
        "checker_sha256": inputs.checker_sha256,
    }))
}
