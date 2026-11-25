use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

fn ai(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ai4math"))
        .args(args)
        .output()
        .unwrap()
}

fn success(command: Output) -> String {
    assert!(
        command.status.success(),
        "{}",
        String::from_utf8_lossy(&command.stderr)
    );
    String::from_utf8(command.stdout).unwrap()
}

fn git(dir: &Path, args: &[&str]) -> String {
    success(
        Command::new("/usr/bin/git")
            .current_dir(dir)
            .args([
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@localhost",
                "-c",
                "commit.gpgSign=false",
                "-c",
                "core.hooksPath=/dev/null",
            ])
            .args(args)
            .output()
            .unwrap(),
    )
    .trim()
    .into()
}

fn prepare(tmp: &Path, files: &[(&str, &str)]) -> (String, std::path::PathBuf) {
    let repo = tmp.join("repo.git");
    let source = tmp.join("source");
    fs::create_dir(&source).unwrap();
    git(&source, &["init", "--initial-branch=main", "--quiet"]);
    for &(name, body) in files {
        fs::write(source.join(name), body).unwrap();
    }
    git(&source, &["add", "."]);
    git(&source, &["commit", "--quiet", "-m", "freeze theorem"]);
    let sha = git(&source, &["rev-parse", "HEAD"]);
    git(
        tmp,
        &[
            "clone",
            "--bare",
            "--quiet",
            source.to_str().unwrap(),
            repo.to_str().unwrap(),
        ],
    );
    (sha, repo)
}

#[test]
fn init_status_and_wiki_index() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_str().unwrap();
    success(ai(&["init", root, "agda"]));
    let st: serde_json::Value = serde_json::from_str(&success(ai(&["status", root]))).unwrap();
    assert_eq!(st["primary_engine"], "agda");
    success(ai(&["wiki-rebuild", root]));
    assert!(!ai(&["init", root, "lean4"]).status.success());
}

#[cfg(unix)]
#[test]
fn wiki_rebuild_refuses_index_symlink_without_touching_target() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_str().unwrap();
    success(ai(&["init", root, "lean4"]));
    let index = tmp.path().join(".mathwiki/.index.sqlite");
    let unrelated = tmp.path().join("sentinel");
    std::fs::rename(&index, &unrelated).unwrap();
    let before = fs::read(&unrelated).unwrap();
    std::os::unix::fs::symlink(&unrelated, &index).unwrap();
    let output = ai(&["wiki-rebuild", root]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unsafe managed Wiki index file"));
    assert_eq!(fs::read(unrelated).unwrap(), before);
}

#[test]
fn refuses_unlisted_modules_without_advancing_main() {
    let tmp = tempfile::tempdir().unwrap();
    let (base, repo) = prepare(
        tmp.path(),
        &[
            ("Proof.lean", "theorem target : True := {{PROOF_BODY}}\n"),
            ("Extra.lean", "theorem e : True := True.intro\n"),
        ],
    );
    let outcome = ai(&[
        "promote",
        repo.to_str().unwrap(),
        "Proof.lean",
        "target",
        "unused",
        "unused",
    ]);
    assert!(!outcome.status.success());
    assert!(String::from_utf8_lossy(&outcome.stderr).contains("ai4math-modules.txt"));
    assert_eq!(git(&repo, &["rev-parse", "refs/heads/main"]), base);
    assert!(!repo.join("ai4math-golden").exists());
}

fn native(
    engine: &str,
    module: &str,
    prefix: &str,
    valid: &str,
    invalid: &str,
    data: Option<String>,
    downstream: Option<(&str, &str)>,
) {
    let binary =
        std::env::var(engine).expect("set the native compiler executable; do not use a shim");
    let tmp = tempfile::tempdir().unwrap();
    let template = format!("{prefix}{{{{PROOF_BODY}}}}\n");
    let mut tracked = vec![(module, template.as_str())];
    let list;
    if let Some(file) = downstream {
        list = format!("{module}\n{}\n", file.0);
        tracked.extend([file, ("ai4math-modules.txt", &list)]);
    }
    let (base, repo) = prepare(tmp.path(), &tracked);
    let body = tmp.path().join("body");
    let mut args = vec![
        "promote",
        repo.to_str().unwrap(),
        module,
        "target",
        &binary,
        body.to_str().unwrap(),
    ];
    if let Some(data) = &data {
        args.push(data);
    }
    fs::write(&body, invalid).unwrap();
    assert!(!ai(&args).status.success());
    assert_eq!(git(&repo, &["rev-parse", "refs/heads/main"]), base);
    fs::write(&body, valid).unwrap();
    let result: serde_json::Value = serde_json::from_str(&success(ai(&args))).unwrap();
    let next = git(&repo, &["rev-parse", "refs/heads/main"]);
    assert_ne!(next, base);
    assert_eq!(git(&repo, &["rev-parse", "refs/heads/main^"]), base);
    assert_eq!(result["commit"], next);
    assert_eq!(result["receipt"]["base_commit"], base);
    assert_eq!(result["golden_published"], true);
    let clone = tmp.path().join("clone");
    success(ai(&[
        "cache-clone",
        repo.to_str().unwrap(),
        &next,
        clone.to_str().unwrap(),
    ]));
    if engine == "AI4MATH_TEST_LEAN" && downstream.is_some() {
        let mut names = fs::read_dir(&clone)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect::<Vec<_>>();
        names.sort_unstable();
        assert_eq!(names, ["Consumer.olean", "Proof.olean"]);
    }
}

#[test]
#[ignore = "requires native unsandboxed macOS host and AI4MATH_TEST_LEAN"]
fn native_lean_promotion_and_downstream_regression() {
    native(
        "AI4MATH_TEST_LEAN",
        "Proof.lean",
        "theorem target : (2 : Nat) = 2 := ",
        "by\n  rfl",
        "False.elim",
        None,
        Some((
            "Consumer.lean",
            "import Proof\ntheorem useProof : (2 : Nat) = 2 := target\n",
        )),
    );
}

#[test]
#[ignore = "requires native unsandboxed macOS host and AI4MATH_TEST_LEAN"]
fn native_downstream_failure_does_not_publish() {
    let lean = std::env::var("AI4MATH_TEST_LEAN").unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let (base, repo) = prepare(
        tmp.path(),
        &[
            ("Proof.lean", "theorem target : True := {{PROOF_BODY}}\n"),
            (
                "Consumer.lean",
                "import Proof\ntheorem downstream : False := target\n",
            ),
            ("ai4math-modules.txt", "Proof.lean\nConsumer.lean\n"),
        ],
    );
    let body = tmp.path().join("body");
    fs::write(&body, "True.intro").unwrap();
    let result = ai(&[
        "promote",
        repo.to_str().unwrap(),
        "Proof.lean",
        "target",
        &lean,
        body.to_str().unwrap(),
    ]);
    assert!(!result.status.success());
    let detail = String::from_utf8_lossy(&result.stderr);
    assert!(
        detail.contains("tracked-project regression failed") && detail.contains("Consumer.lean"),
        "{detail}"
    );
    assert_eq!(git(&repo, &["rev-parse", "refs/heads/main"]), base);
    assert!(!repo.join("ai4math-golden").exists());
}

#[test]
#[ignore = "requires native unsandboxed macOS host, AI4MATH_TEST_AGDA and AI4MATH_TEST_AGDA_DATA"]
fn native_agda_promotion() {
    let data = std::env::var("AI4MATH_TEST_AGDA_DATA").unwrap();
    native(
        "AI4MATH_TEST_AGDA",
        "Proof.agda",
        "module Proof where\nopen import Agda.Builtin.Nat\nopen import Agda.Builtin.Equality\ntarget : zero ≡ zero\ntarget = ",
        "refl",
        "zero",
        Some(data),
        None,
    );
}
