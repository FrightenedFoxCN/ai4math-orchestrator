//! Offline operator input for T2 telemetry comparisons. No project or ledger is opened.
use crate::{print_json, t2_compare, t2_heartbeat_compare};
use anyhow::{Context, Result, ensure};
use std::{fs, io::Read, path::Path};

// 9 runs × 128 modules × a bounded path, SHA-256 and integers fits comfortably.
const INPUT_CAP: u64 = 1024 * 1024;

fn read_samples(path: &Path) -> Result<Vec<u8>> {
    let before = fs::symlink_metadata(path)?;
    ensure!(
        before.is_file(),
        "T2 sample input must be a regular file, not a link"
    );
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.len() <= INPUT_CAP,
        "T2 sample input exceeds the 1 MiB limit or is not a regular file"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ensure!(
            (before.dev(), before.ino()) == (metadata.dev(), metadata.ino()),
            "T2 sample input changed during open"
        );
    }
    let mut data = Vec::new();
    file.take(INPUT_CAP + 1).read_to_end(&mut data)?;
    ensure!(
        data.len() as u64 <= INPUT_CAP,
        "T2 sample input exceeds the 1 MiB limit"
    );
    Ok(data)
}

fn threshold(value: &str) -> Result<u64> {
    ensure!(
        !value.is_empty() && !value.starts_with('0') && value.bytes().all(|b| b.is_ascii_digit()),
        "T2 comparison threshold must be a canonical positive integer"
    );
    value
        .parse()
        .context("T2 comparison threshold must be a canonical positive integer")
}

pub(crate) fn run_heartbeats(
    baseline: &Path,
    candidate: &Path,
    delta: &str,
    percent: &str,
) -> Result<()> {
    let absolute = threshold(delta)?;
    let relative = threshold(percent)?;
    let baseline = read_samples(baseline).context("read T2 heartbeat baseline samples")?;
    let candidate = read_samples(candidate).context("read T2 heartbeat candidate samples")?;
    let report = t2_heartbeat_compare::compare(&baseline, &candidate, absolute, relative)?;
    let regressed = report.regressed;
    print_json(report)?;
    ensure!(
        !regressed,
        "measured T2 heartbeat increase meets both comparison thresholds"
    );
    Ok(())
}

pub(crate) fn run(baseline: &Path, candidate: &Path, delta: &str, percent: &str) -> Result<()> {
    let absolute = threshold(delta)?;
    let relative = threshold(percent)?;
    let baseline = read_samples(baseline).context("read T2 baseline samples")?;
    let candidate = read_samples(candidate).context("read T2 candidate samples")?;
    let report = t2_compare::compare(&baseline, &candidate, absolute, relative)?;
    let regressed = report.regressed;
    print_json(report)?;
    ensure!(
        !regressed,
        "measured T2 build slowdown meets both comparison thresholds"
    );
    Ok(())
}
