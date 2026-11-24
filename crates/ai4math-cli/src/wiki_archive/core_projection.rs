//! Explicit operator projection of managed CORE entries into the disposable Wiki index.
//! The pair (reviewed Git HEAD, ledger event sequence) is a visibility gate, not proof authority.
use super::{prefix, require_reviewed_main, sha};
use crate::{
    print_json, promotion::PromotionRepo, reviewed_spec, worker_dispatch::private_state_path,
};
use ai4math_store::{StateStore, Wiki, WikiManagedCoreEntry, WikiManagedGeneration, WikiQuery};
use anyhow::{Context, Result, ensure};
use serde_json::json;
use std::{
    fs,
    io::ErrorKind,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

fn wiki_root(root: &Path) -> Result<PathBuf> {
    let wiki = root.join(".mathwiki");
    let uid = unsafe { libc::geteuid() };
    let dir = fs::symlink_metadata(&wiki)?;
    ensure!(
        dir.is_dir() && dir.uid() == uid && dir.mode() & 0o022 == 0,
        "unsafe managed Wiki directory"
    );
    for name in [
        ".index.sqlite",
        ".index.sqlite-wal",
        ".index.sqlite-shm",
        ".index.sqlite-journal",
    ] {
        match fs::symlink_metadata(wiki.join(name)) {
            Ok(meta) => ensure!(
                meta.is_file()
                    && meta.uid() == uid
                    && meta.nlink() == 1
                    && meta.mode() & 0o022 == 0,
                "unsafe managed Wiki index file"
            ),
            Err(error) if error.kind() == ErrorKind::NotFound => (),
            Err(error) => return Err(error.into()),
        }
    }
    Ok(wiki)
}

fn require_generation(state: &StateStore, git: &PromotionRepo, head: &str, seq: i64) -> Result<()> {
    require_reviewed_main(state, git, head)?;
    ensure!(
        state.status()?.last_event_seq == seq,
        "ledger changed during managed Wiki operation; retry"
    );
    Ok(())
}

fn entries(
    state: &StateStore,
    git: &PromotionRepo,
    head: &str,
) -> Result<Vec<WikiManagedCoreEntry>> {
    let repo = git
        .path()
        .to_str()
        .context("managed repository path is not UTF-8")?;
    let mut generated = Vec::new();
    for evidence in state.current_sediment_lineages()? {
        let lineage = evidence.lineage;
        let verified = &evidence.verified_at_commit;
        ensure!(
            lineage.repo == repo,
            "managed CORE lineage belongs to another repository"
        );
        let Some(origin) = state.claim(&lineage.origin_claim)? else {
            continue;
        };
        if origin.local_trust == ai4math_store::Trust::L2b
            || origin.effective_trust == ai4math_store::Trust::L2b
        {
            continue;
        }
        let approval = state
            .sediment_adaptation(&lineage.approval_sha256)?
            .context("managed CORE lineage lacks approval")?
            .plan;
        let module = &approval.target_module;
        if !git.is_ancestor(&lineage.promoted_commit, verified)?
            || !git.is_ancestor(verified, head)?
            || !git.contains_module_at(head, module)?
        {
            continue;
        }
        let recipe = state
            .managed_proof_recipe_at(&lineage.target_claim, &lineage.promoted_commit)?
            .context("eligible managed CORE lineage lacks fixed promotion recipe")?;
        ensure!(
            recipe.file_path == *module
                && recipe.target_decl == approval.target_decl
                && recipe.base_commit == approval.reviewed_commit,
            "managed CORE lineage differs from fixed promotion recipe"
        );
        let current = git.snapshot(module, Some(head))?;
        let current_rows = current.project()?;
        let (_, source) = current_rows
            .iter()
            .find(|(path, _)| *path == module)
            .context("managed CORE module absent from current manifest")?;
        if sha(source.as_bytes()) != recipe.source_sha256 {
            continue;
        }
        // T3 may have checked changed predecessors. Use the snapshot anchored
        // by the current sealed receipt, never the original T1 predecessor set
        // in that case. The proof module itself is still exactly the T1 recipe.
        let anchored = git.snapshot(module, Some(verified))?;
        let anchored_rows = anchored.project()?;
        let anchor_index = anchored_rows
            .iter()
            .position(|(path, _)| *path == module)
            .context("managed CORE module absent from verification snapshot")?;
        ensure!(
            sha(anchored_rows[anchor_index].1.as_bytes()) == recipe.source_sha256,
            "managed CORE verification anchor does not contain the original verified proof bytes"
        );
        if prefix(&anchored_rows, module)? != prefix(&current_rows, module)? {
            continue;
        }
        let spec = state
            .lean_spec(&lineage.target_claim)?
            .context("managed CORE adaptation lost its reviewed Lean spec binding")?;
        ensure!(
            spec.spec_file == approval.spec_file
                && spec.spec_decl == approval.spec_decl
                && spec.spec_prefix_sha256 == approval.spec_prefix_sha256,
            "managed CORE adaptation does not match the approved Lean spec binding"
        );
        let deps = anchored_rows[..anchor_index]
            .iter()
            .map(|(name, source)| (*name, source.as_str()))
            .collect::<Vec<_>>();
        reviewed_spec::check_snapshot(git, &anchored, &spec)?;
        reviewed_spec::check_prefix(&deps, &spec)?;
        let informal_gloss = state
            .claim_statement_for_dispatch(&lineage.target_claim)?
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .filter(|c| !c.is_control())
            .take(512)
            .collect();
        generated.push(WikiManagedCoreEntry {
            id: lineage.core_entry_id,
            archive_id: lineage.archive_id,
            claim_id: lineage.target_claim,
            origin_claim: lineage.origin_claim,
            title: format!(
                "Verified Lean adaptation: {}",
                approval.target_decl.chars().take(120).collect::<String>()
            ),
            informal_gloss,
            verified_at_commit: evidence.verified_at_commit,
        });
    }
    Ok(generated)
}

pub(crate) fn rebuild(root: &Path, repo: &Path, head: &str) -> Result<usize> {
    let git = PromotionRepo::try_lock(repo)?;
    let state = StateStore::open_readonly(private_state_path(root)?)?;
    require_reviewed_main(&state, &git, head)?;
    let seq = state.status()?.last_event_seq;
    let wiki = wiki_root(root)?;
    let entries = entries(&state, &git, head)?;
    require_generation(&state, &git, head, seq)?;
    let mut index = Wiki::open(wiki.join(".index.sqlite"))?;
    let count = index.rebuild_with_managed_core(
        &wiki,
        WikiManagedGeneration {
            head,
            event_seq: seq,
        },
        &entries,
    )?;
    // Any publish raced by an external Git or ledger writer is unusable through
    // generation-checked readers, even if the older disposable build committed.
    require_generation(&state, &git, head, seq)?;
    Ok(count)
}

pub(super) fn checked_read<T>(
    root: &Path,
    repo: &Path,
    head: &str,
    read: impl FnOnce(&Wiki, WikiManagedGeneration<'_>) -> Result<T>,
) -> Result<(T, i64)> {
    let git = PromotionRepo::try_lock(repo)?;
    let state = StateStore::open_readonly(private_state_path(root)?)?;
    require_reviewed_main(&state, &git, head)?;
    let seq = state.status()?.last_event_seq;
    let index = Wiki::open_readonly(wiki_root(root)?.join(".index.sqlite"))?;
    let result = read(
        &index,
        WikiManagedGeneration {
            head,
            event_seq: seq,
        },
    )?;
    require_generation(&state, &git, head, seq)?;
    Ok((result, seq))
}

pub(crate) fn rebuild_command(root: &Path, repo: &Path, head: &str) -> Result<()> {
    print_json(json!({"indexed_entries":rebuild(root,repo,head)?,"reviewed_main":head}))
}

pub(crate) fn search(root: &Path, repo: &Path, head: &str, text: &str) -> Result<()> {
    ensure!(
        text.len() <= 512 && !text.chars().any(char::is_control),
        "invalid managed Wiki search text"
    );
    let (result, seq) = checked_read(root, repo, head, |wiki, generation| {
        wiki.search_at(WikiQuery::new(text), generation)
    })?;
    print_json(
        json!({"reviewed_main":head,"event_seq":seq,"result":result,"current_proof_authority":false}),
    )
}

pub(crate) fn provenance(root: &Path, repo: &Path, head: &str, id: &str) -> Result<()> {
    ensure!(
        !id.trim().is_empty() && id.len() <= 128 && !id.chars().any(char::is_control),
        "invalid Wiki lineage id"
    );
    let (result, seq) = checked_read(root, repo, head, |wiki, generation| {
        wiki.provenance_at(id, 2, 32, generation)
    })?;
    print_json(
        json!({"reviewed_main":head,"event_seq":seq,"result":result,"current_proof_authority":false}),
    )
}
