#[cfg(not(unix))]
fn main() -> std::process::ExitCode {
    eprintln!("ai4mathd requires a Unix domain socket backend");
    std::process::ExitCode::FAILURE
}

#[cfg(unix)]
mod facts;
#[cfg(unix)]
mod proofs;
#[cfg(unix)]
mod runtime;
#[cfg(unix)]
mod transport;

#[cfg(unix)]
fn main() -> std::process::ExitCode {
    use std::{env, path::PathBuf, process::ExitCode, time::Duration};
    let cmd = || -> anyhow::Result<()> {
        let mut args = env::args_os().skip(1);
        let Some(first) = args.next() else {
            anyhow::bail!("usage: ai4mathd --project ROOT [--tick-ms N] [--proof-targets PATH]");
        };
        if first == "--help" {
            println!("ai4mathd --project ROOT [--tick-ms N] [--proof-targets PATH]");
            return Ok(());
        }
        anyhow::ensure!(first == "--project", "expected --project ROOT");
        let root = PathBuf::from(
            args.next()
                .ok_or_else(|| anyhow::anyhow!("missing project path"))?,
        );
        let mut tick = None;
        let mut catalog = None;
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
            } else {
                anyhow::bail!("unknown or repeated argument");
            }
        }
        runtime::run(
            &root,
            tick.unwrap_or(Duration::from_secs(2)),
            catalog.as_deref(),
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
