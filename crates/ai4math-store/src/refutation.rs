//! Content-addressed Graveyard artifact generation for kernel-authorized refutations.
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::{Read, Write},
    path::Path,
};

pub struct WitnessInput<'a> {
    pub project_root: &'a Path,
    pub repository: &'a Path,
    pub claim_id: &'a str,
    pub committed_source: &'a str,
    pub proof_body: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WitnessSpecBinding {
    pub repo: String,
    pub commit_id: String,
    pub file_path: String,
    pub proposition_name: String,
}

#[derive(Debug, Serialize)]
pub struct RefutationRecord {
    pub wiki_id: String,
    pub witness_sha256: String,
    pub event_seq: i64,
    pub already_recorded: bool,
}

#[derive(Serialize)]
struct Frontmatter<'a> {
    id: &'a str,
    stratum: &'static str,
    engine: &'static str,
    trust_level: &'static str,
    status: &'static str,
    pinned: bool,
    title: &'a str,
    verified_at_commit: &'a str,
    origin_claim: &'a str,
    informal_gloss: &'a str,
}

pub(crate) fn wiki_id(claim: &str, witness_sha256: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(b"ai4math-graveyard-v1\0");
    hash.update(witness_sha256.as_bytes());
    hash.update(claim.as_bytes());
    format!("GRV-{:x}", hash.finalize())
}

pub(crate) fn materialize(
    input: &WitnessInput<'_>,
    id: &str,
    base: &str,
    module: &str,
    proposition: &str,
    source_sha: &str,
    witness_sha: &str,
    checker: &str,
) -> Result<()> {
    let wiki = input.project_root.join(".mathwiki");
    let graveyard = wiki.join("graveyard");
    for dir in [&wiki, &graveyard] {
        ensure!(
            fs::symlink_metadata(dir)
                .with_context(|| format!("open Wiki path {}", dir.display()))?
                .is_dir(),
            "Wiki archive path must be a real directory"
        );
    }
    let title = format!("Kernel refutation: {proposition}");
    let gloss = format!("Kernel-verified negation of {module}#{proposition} at commit {base}.");
    let fm = Frontmatter {
        id,
        stratum: "graveyard",
        engine: "lean4",
        trust_level: "L2b",
        status: "refuted",
        pinned: true,
        title: &title,
        verified_at_commit: base,
        origin_claim: input.claim_id,
        informal_gloss: &gloss,
    };
    let yaml = serde_yaml::to_string(&fm)?;
    let mut text = format!(
        "---\n{yaml}---\n\n## Kernel Refutation\n\n- Claim: {}\n- Spec: {module}#{proposition}\n- Source SHA-256: {source_sha}\n- Witness SHA-256: {witness_sha}\n\n## Verified Lean proof body\n\n",
        input.claim_id
    );
    // Indent rather than interpolate a code fence; proof text cannot terminate the code block.
    for line in input.proof_body.lines() {
        text.push_str("    ");
        text.push_str(line);
        text.push('\n');
    }
    text.push_str("\n## Broker checker module\n\n");
    for line in checker.lines() {
        text.push_str("    ");
        text.push_str(line);
        text.push('\n');
    }
    let target = graveyard.join(format!("{id}.md"));
    let mut staged = tempfile::NamedTempFile::new_in(&graveyard)?;
    staged.write_all(text.as_bytes())?;
    staged.as_file().sync_all()?;
    match staged.persist_noclobber(&target) {
        Ok(_) => {}
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
            let m = fs::symlink_metadata(&target)?;
            ensure!(
                m.is_file() && m.len() == text.len() as u64,
                "existing Graveyard archive does not match verified witness"
            );
            let mut actual = Vec::new();
            File::open(&target)?
                .take(text.len() as u64 + 1)
                .read_to_end(&mut actual)?;
            ensure!(
                actual == text.as_bytes(),
                "existing Graveyard archive does not match verified witness"
            );
        }
        Err(error) => return Err(error.error.into()),
    }
    File::open(&graveyard)?.sync_all()?;
    Ok(())
}
