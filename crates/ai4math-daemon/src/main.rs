#[cfg(not(unix))]
fn main() -> std::process::ExitCode {
    eprintln!("ai4mathd requires a Unix domain socket backend");
    std::process::ExitCode::FAILURE
}

#[cfg(unix)]
mod facts;
#[cfg(unix)]
mod native_rfc;
#[cfg(unix)]
mod owner_promotion;
#[cfg(unix)]
mod proofs;
#[cfg(unix)]
mod resident_paths;
#[cfg(unix)]
mod resident_process;
#[cfg(unix)]
mod runtime;
#[cfg(unix)]
mod t2_audit;
#[cfg(unix)]
mod t3_discovery;
#[cfg(unix)]
mod t3_worker;
#[cfg(unix)]
mod transport;

#[cfg(unix)]
fn main() -> std::process::ExitCode {
    use std::{env, path::PathBuf, process::ExitCode, time::Duration};
    const USAGE: &str = "ai4mathd --project ROOT [--tick-ms N] [--proof-targets PATH] [--t3-autosweep PATH] [--t3-discovery PATH] [--t2-audit PATH] [--rfc-cli PATH --rfc-lean PATH [--rfc-timeout-secs N]]";
    let cmd = || -> anyhow::Result<()> {
        let mut args = env::args_os().skip(1);
        let Some(first) = args.next() else {
            anyhow::bail!("usage: {USAGE}");
        };
        if first == "--help" {
            println!("{USAGE}");
            return Ok(());
        }
        anyhow::ensure!(first == "--project", "expected --project ROOT");
        let root = PathBuf::from(
            args.next()
                .ok_or_else(|| anyhow::anyhow!("missing project path"))?,
        );
        let mut tick = None;
        let mut catalog = None;
        let mut t3_config = None;
        let mut discovery_config = None;
        let mut audit_config = None;
        let mut rfc_cli = None;
        let mut rfc_lean = None;
        let mut rfc_timeout = None;
        while let Some(flag) = args.next() {
            if flag == "--tick-ms" && tick.is_none() {
                let ms: u64 = args
                    .next()
                    .and_then(|x| x.to_str().and_then(|x| x.parse().ok()))
                    .ok_or_else(|| anyhow::anyhow!("invalid --tick-ms"))?;
                anyhow::ensure!(
                    (100..=60_000).contains(&ms),
                    "--tick-ms must be in [100, 60000]"
                );
                tick = Some(Duration::from_millis(ms));
            } else if flag == "--proof-targets" && catalog.is_none() {
                catalog =
                    Some(PathBuf::from(args.next().ok_or_else(|| {
                        anyhow::anyhow!("missing proof catalogue path")
                    })?));
            } else if flag == "--t3-autosweep" && t3_config.is_none() {
                t3_config = Some(PathBuf::from(args.next().ok_or_else(|| {
                    anyhow::anyhow!("missing --t3-autosweep configuration path")
                })?));
            } else if flag == "--t3-discovery" && discovery_config.is_none() {
                discovery_config = Some(PathBuf::from(args.next().ok_or_else(|| {
                    anyhow::anyhow!("missing --t3-discovery configuration path")
                })?));
            } else if flag == "--t2-audit" && audit_config.is_none() {
                audit_config =
                    Some(PathBuf::from(args.next().ok_or_else(|| {
                        anyhow::anyhow!("missing --t2-audit configuration path")
                    })?));
            } else if flag == "--rfc-cli" && rfc_cli.is_none() {
                rfc_cli = Some(PathBuf::from(
                    args.next()
                        .ok_or_else(|| anyhow::anyhow!("missing --rfc-cli path"))?,
                ));
            } else if flag == "--rfc-lean" && rfc_lean.is_none() {
                rfc_lean = Some(PathBuf::from(
                    args.next()
                        .ok_or_else(|| anyhow::anyhow!("missing --rfc-lean path"))?,
                ));
            } else if flag == "--rfc-timeout-secs" && rfc_timeout.is_none() {
                rfc_timeout = Some(
                    args.next()
                        .and_then(|x| x.to_str().and_then(|x| x.parse().ok()))
                        .ok_or_else(|| anyhow::anyhow!("invalid --rfc-timeout-secs"))?,
                );
            } else {
                anyhow::bail!("unknown or repeated argument");
            }
        }
        let worker = t3_config
            .as_deref()
            .map(t3_worker::Config::load)
            .transpose()?;
        let discovery = discovery_config
            .as_deref()
            .map(t3_discovery::Config::load)
            .transpose()?;
        let audit = audit_config
            .as_deref()
            .map(t2_audit::Config::load)
            .transpose()?;
        anyhow::ensure!(
            rfc_cli.is_some() == rfc_lean.is_some(),
            "--rfc-cli and --rfc-lean must be configured together"
        );
        let native_rfc = rfc_cli
            .zip(rfc_lean)
            .map(|(cli, lean)| native_rfc::Config::new(cli, lean, rfc_timeout.unwrap_or(600)))
            .transpose()?;
        runtime::run(
            &root,
            tick.unwrap_or(Duration::from_secs(2)),
            catalog.as_deref(),
            worker,
            discovery,
            audit,
            native_rfc,
        )
    };
    match cmd() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ai4mathd: {e:#}");
            ExitCode::FAILURE
        }
    }
}
