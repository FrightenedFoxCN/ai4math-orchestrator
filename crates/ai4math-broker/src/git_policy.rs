use std::process::Command;

/// Append an ancestry probe that ignores replace refs, grafts, and commit graphs.
/// The caller configures the trusted Git executable and repository, validates
/// the commit IDs, decides whether equality is allowed, and runs the command.
pub fn physical_ancestry<'a>(git: &'a mut Command, older: &str, newer: &str) -> &'a mut Command {
    git.env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_GRAFT_FILE", "/dev/null")
        .args([
            "-c",
            "core.commitGraph=false",
            "merge-base",
            "--is-ancestor",
            older,
            newer,
        ])
}
