//! Operator-only RFC intake. These inputs and approval cannot run a worker or confer proof authority.
use crate::{
    bound_engine, print_json, promotion::PromotionRepo, worker_dispatch::private_state_path,
};
use ai4math_core::{Engine, InfraRFC};
use ai4math_store::{INFRA_RFC_MAX_BYTES, InfraRfcRecord, StateStore, parse_infra_rfc_draft};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use std::{fs, io::Read, path::Path};

const INPUT_CAP: u64 = INFRA_RFC_MAX_BYTES as u64;

fn read_draft(path: &Path) -> Result<InfraRFC> {
    let before = fs::symlink_metadata(path).context("inspect InfraRFC input")?;
    ensure!(
        before.is_file() && before.len() <= INPUT_CAP,
        "InfraRFC input must be a regular non-link file of at most 16 KiB"
    );
    let mut opts = fs::OpenOptions::new();
    opts.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = opts.open(path).context("open InfraRFC input")?;
    let after = file.metadata()?;
    ensure!(
        after.is_file() && after.len() <= INPUT_CAP,
        "InfraRFC input is not a regular file or exceeds 16 KiB"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ensure!(
            (before.dev(), before.ino()) == (after.dev(), after.ino()),
            "InfraRFC input changed during open"
        );
    }
    let mut data = Vec::new();
    file.take(INPUT_CAP + 1).read_to_end(&mut data)?;
    ensure!(
        data.len() as u64 <= INPUT_CAP,
        "InfraRFC input exceeds 16 KiB"
    );
    parse_infra_rfc_draft(&data).context("invalid InfraRFC draft JSON")
}

fn repo_name(git: &PromotionRepo) -> Result<&str> {
    git.path()
        .to_str()
        .context("managed repository path is not UTF-8")
}

fn at_head(git: &PromotionRepo, reviewed: &str) -> Result<()> {
    ensure!(
        git.main_head()? == reviewed,
        "reviewed main no longer matches current managed Git main"
    );
    Ok(())
}

fn check_tracked(
    root: &Path,
    git: &PromotionRepo,
    module: &str,
    review: &str,
    engine: Engine,
) -> Result<()> {
    at_head(git, review)?;
    ensure!(
        bound_engine(root, module)? == engine,
        "InfraRFC engine must match the project primary engine and target module"
    );
    git.snapshot(module, Some(review))
        .context("validate tracked InfraRFC target module")?;
    Ok(())
}

pub(crate) fn submit(
    root: &Path,
    repo: &Path,
    module: &str,
    reviewed: &str,
    input: &Path,
) -> Result<()> {
    let draft = read_draft(input)?;
    let state = private_state_path(root)
        .context("InfraRFC intake requires an initialized private project")?;
    let git = PromotionRepo::try_lock_existing(repo)?;
    check_tracked(root, &git, module, reviewed, draft.engine)?;
    let mut store = StateStore::open(&state)?;
    print_json(
        store.submit_infra_rfc(repo_name(&git)?, reviewed, module, &draft, || {
            at_head(&git, reviewed)
        })?,
    )
}

pub(crate) fn approve(root: &Path, repo: &Path, id: &str, reviewed: &str, sha: &str) -> Result<()> {
    let state = private_state_path(root)
        .context("InfraRFC approval requires an initialized private project")?;
    let git = PromotionRepo::try_lock_existing(repo)?;
    at_head(&git, reviewed)?;
    let mut store = StateStore::open(&state)?;
    let name = repo_name(&git)?;
    let stored = store.infra_rfc(name, id)?.context("unknown InfraRFC")?;
    check_tracked(
        root,
        &git,
        &stored.module_path,
        reviewed,
        stored.draft.engine,
    )?;
    print_json(store.approve_infra_rfc(name, reviewed, id, sha, || at_head(&git, reviewed))?)
}

#[derive(Serialize)]
struct Inspection {
    #[serde(flatten)]
    record: InfraRfcRecord,
    current_main: String,
    heads_match: bool,
}

pub(crate) fn inspect(root: &Path, repo: &Path, id: &str) -> Result<()> {
    let state = private_state_path(root)
        .context("InfraRFC inspection requires an initialized private project")?;
    let git = PromotionRepo::try_lock_existing(repo)?;
    let current_main = git.main_head()?;
    let store = StateStore::open_readonly(&state)?;
    let record = store
        .infra_rfc(repo_name(&git)?, id)?
        .context("unknown InfraRFC")?;
    at_head(&git, &current_main)?;
    print_json(Inspection {
        heads_match: current_main == record.reviewed_commit,
        current_main,
        record,
    })
}

#[cfg(test)]
mod tests;
