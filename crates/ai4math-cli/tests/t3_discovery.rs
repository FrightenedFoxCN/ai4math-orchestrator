#![cfg(unix)]

use ai4math_store::StateStore;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    time::SystemTime,
};

const ANCHOR: &str = "Proof.lean";
const MANIFEST: &str = "ai4math-modules.txt";
const MAIN: &str = "refs/heads/main";
const DB: &str = ".runtime/state.sqlite";

fn utf8(path: &Path) -> &str {
    path.to_str().expect("UTF-8 fixture path")
}

fn success(output: Output, command: &str) -> String {
    assert!(
        output.status.success(),
        "{command}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("/usr/bin/git")
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
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
        .unwrap();
    success(output, &format!("git {args:?}")).trim().to_owned()
}

#[derive(PartialEq, Eq)]
struct FileState {
    bytes: Vec<u8>,
    modified: SystemTime,
}

// Inspect the filesystem directly, including all repo refs, objects and lock
// files. Git status alone would miss an unreferenced object or a pending ref.
fn file_tree(root: &Path) -> BTreeMap<PathBuf, Option<FileState>> {
    fn walk(root: &Path, dir: &Path, entries: &mut BTreeMap<PathBuf, Option<FileState>>) {
        for entry in fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let relative = path.strip_prefix(root).unwrap().to_owned();
            if entry.file_type().unwrap().is_dir() {
                entries.insert(relative, None);
                walk(root, &path, entries);
            } else {
                assert!(entry.file_type().unwrap().is_file(), "{path:?}");
                entries.insert(
                    relative,
                    Some(FileState {
                        bytes: fs::read(&path).unwrap(),
                        modified: entry.metadata().unwrap().modified().unwrap(),
                    }),
                );
            }
        }
    }
    let mut entries = BTreeMap::new();
    walk(root, root, &mut entries);
    entries
}

fn project_tree(root: &Path) -> BTreeMap<PathBuf, Option<FileState>> {
    let mut entries = file_tree(root);
    // SQLite readers can maintain the shared-memory index and create an empty
    // WAL. Compare all durable data, including any nonempty WAL, byte-for-byte.
    entries.remove(Path::new(".runtime/state.sqlite-shm"));
    let wal = Path::new(".runtime/state.sqlite-wal");
    if matches!(entries.get(wal), Some(Some(file)) if file.bytes.is_empty()) {
        entries.remove(wal);
    }
    entries
}

fn assert_same_tree(
    root: &Path,
    before: BTreeMap<PathBuf, Option<FileState>>,
    after: BTreeMap<PathBuf, Option<FileState>>,
) {
    assert_eq!(
        before.keys().collect::<Vec<_>>(),
        after.keys().collect::<Vec<_>>(),
        "entries changed in {root:?}"
    );
    for (path, before) in before {
        assert!(before == after[&path], "modified {:?}", root.join(path));
    }
}

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    work: PathBuf,
    bare: PathBuf,
    base: String,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("project");
        let bare = temp.path().join("repo.git");
        let work = temp.path().join("work");
        fs::create_dir(&work).unwrap();
        git(&work, &["init", "--quiet", "--initial-branch=main"]);
        fs::write(work.join("Defs.lean"), "def x : Nat := 2\n").unwrap();
        fs::write(work.join("OtherDefs.lean"), "def y : Nat := 3\n").unwrap();
        fs::write(
            work.join(ANCHOR),
            "import Defs\nimport OtherDefs\ntheorem target : x = 2 := {{PROOF_BODY}}\n",
        )
        .unwrap();
        fs::write(
            work.join(MANIFEST),
            "Defs.lean\nOtherDefs.lean\nProof.lean\n",
        )
        .unwrap();
        git(&work, &["add", "."]);
        git(&work, &["commit", "--quiet", "-m", "Baseline"]);
        let base = git(&work, &["rev-parse", "HEAD"]);
        git(
            temp.path(),
            &["clone", "--bare", "--quiet", "work", "repo.git"],
        );
        let f = Self {
            _temp: temp,
            root,
            work,
            bare,
            base,
        };
        success(f.ai("init", &["lean4"]), "init");
        for (claim, module, target) in [
            ("Definition", "Defs.lean", "x"),
            ("Proof", ANCHOR, "target"),
            ("Outside", "Outside.lean", "outside"),
        ] {
            success(
                f.ai(
                    "declare-claim",
                    &[claim, module, target, claim, "Fixture statement"],
                ),
                "declare-claim",
            );
        }
        assert_eq!(
            f.json("task-bind", &[utf8(&f.bare), ANCHOR, &f.base]),
            json!({"bound": true, "base_commit": f.base})
        );
        f
    }

    fn ai(&self, command: &str, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_ai4math"))
            .arg(command)
            .arg(&self.root)
            .args(args)
            .output()
            .unwrap()
    }

    fn json(&self, command: &str, args: &[&str]) -> Value {
        let stdout = success(self.ai(command, args), command);
        serde_json::from_str(&stdout).unwrap_or_else(|err| panic!("{command}: {err}: {stdout}"))
    }

    fn state(&self) -> StateStore {
        StateStore::open_readonly(self.root.join(DB)).unwrap()
    }

    fn event_seq(&self) -> i64 {
        self.state().status().unwrap().last_event_seq
    }

    fn commit_to_main(&self, message: &str) -> String {
        git(&self.work, &["add", "."]);
        git(
            &self.work,
            &["commit", "--quiet", "--allow-empty", "-m", message],
        );
        git(
            &self.work,
            &["push", "--quiet", utf8(&self.bare), "HEAD:refs/heads/main"],
        );
        git(&self.bare, &["rev-parse", MAIN])
    }

    fn readonly_discover(&self, repo: &Path, anchor: &str, baseline: &str) -> Output {
        let project_before = project_tree(&self.root);
        let repo_before = file_tree(repo);
        let output = self.ai("t3-discover", &[utf8(repo), anchor, baseline]);
        assert_same_tree(&self.root, project_before, project_tree(&self.root));
        assert_same_tree(repo, repo_before, file_tree(repo));
        output
    }

    fn discover(&self, baseline: &str) -> Value {
        let output = self.readonly_discover(&self.bare, ANCHOR, baseline);
        serde_json::from_str(&success(output, "t3-discover")).unwrap()
    }

    fn assert_no_plan(&self, baseline: &str, head: &str, status: &str) {
        assert_eq!(
            self.discover(baseline),
            json!({
                "status": status,
                "base_commit": baseline,
                "observed_head": head,
                "ledger_event_seq": self.event_seq(),
                "changed_modules": [],
                "module_order_changed": false,
                "plan": null,
                "advisory_only": true,
            })
        );
    }

    fn structural_plan(&self, head: &str, modules: &[&str], reorder: bool) -> Value {
        let observation = self.discover(&self.base);
        let preview = self.json("t3-preview", &[utf8(&self.bare), ANCHOR, &self.base, head]);
        assert_eq!(
            observation,
            json!({
                "status": "structural_change",
                "base_commit": self.base,
                "observed_head": head,
                "ledger_event_seq": self.event_seq(),
                "changed_modules": modules,
                "module_order_changed": reorder,
                "plan": preview,
                "advisory_only": true,
            })
        );
        assert_eq!(preview["base_commit"], self.base);
        assert_eq!(preview["reviewed_commit"], head);
        assert_eq!(preview["ledger_event_seq"], self.event_seq());
        assert_eq!(preview["changed_modules"], json!(modules));
        observation["plan"].clone()
    }
}

#[test]
fn equal_empty_and_metadata_only_main_are_unchanged() {
    let f = Fixture::new();
    f.assert_no_plan(&f.base, &f.base, "unchanged");
    let empty = f.commit_to_main("No tree change");
    assert_ne!(empty, f.base);
    f.assert_no_plan(&f.base, &empty, "unchanged");

    // Only sanctioned metadata can appear alongside the bounded source tree.
    let adrs = f.work.join("docs/adr");
    fs::create_dir_all(&adrs).unwrap();
    fs::write(
        adrs.join(format!("ADR-PIVOT-{}.md", "a".repeat(64))),
        "# Fixture decision\n",
    )
    .unwrap();
    let metadata = f.commit_to_main("Record a decision");
    f.assert_no_plan(&f.base, &metadata, "unchanged");
}

#[test]
fn source_plan_matches_preview_and_is_only_applied_explicitly() {
    let f = Fixture::new();
    let before_seq = f.event_seq();
    fs::write(f.work.join("Defs.lean"), "def x : Nat := 3\n").unwrap();
    let head = f.commit_to_main("Change Defs source");
    let preview = f.structural_plan(&head, &["Defs.lean"], false);
    assert_eq!(
        preview["affected_modules"],
        json!(["Defs.lean", "OtherDefs.lean", "Proof.lean"])
    );
    let claims = preview["claims"].as_array().unwrap();
    assert_eq!(claims.len(), 2);
    assert_eq!(claims[0]["claim_id"], "Definition");
    assert_eq!(claims[1]["claim_id"], "Proof");
    assert!(claims.iter().all(|c| c["old_proof"] == "not_l5"));
    let hash = preview["plan_sha256"].as_str().unwrap();
    assert_eq!(hash.len(), 64);
    {
        let state = f.state();
        assert_eq!(state.status().unwrap().last_event_seq, before_seq);
        assert!(state.applied_t3_plan(hash).unwrap().is_none());
        for claim in ["Definition", "Proof", "Outside"] {
            assert!(!state.claim(claim).unwrap().unwrap().stale);
        }
    }

    let git_before = file_tree(&f.bare);
    let applied = f.json("t3-apply", &[utf8(&f.bare), ANCHOR, &f.base, &head, hash]);
    assert_same_tree(&f.bare, git_before, file_tree(&f.bare));
    assert_eq!(
        applied["approval"],
        json!({
            "plan_sha256": hash,
            "base_commit": f.base,
            "reviewed_commit": head,
            "anchor_module": ANCHOR,
            "preview_event_seq": before_seq,
        })
    );
    let state = f.state();
    assert!(state.status().unwrap().last_event_seq > before_seq);
    assert!(state.applied_t3_plan(hash).unwrap().is_some());
    for claim in ["Definition", "Proof"] {
        let claim = state.claim(claim).unwrap().unwrap();
        assert!(claim.stale);
        assert_eq!(claim.taint_root_commit.as_deref(), Some(head.as_str()));
    }
    assert!(!state.claim("Outside").unwrap().unwrap().stale);
}

#[test]
fn manifest_order_alone_is_structural() {
    let f = Fixture::new();
    // Defs and OtherDefs are independent; both remain before their importer.
    fs::write(
        f.work.join(MANIFEST),
        "OtherDefs.lean\nDefs.lean\nProof.lean\n",
    )
    .unwrap();
    let head = f.commit_to_main("Reorder independent modules");
    f.structural_plan(&head, &[], true);
}

#[test]
fn sibling_of_main_is_diverged_even_when_its_tree_is_identical() {
    let f = Fixture::new();
    let head = f.commit_to_main("Main descendant");
    git(&f.work, &["checkout", "--quiet", "-b", "sibling", &f.base]);
    git(
        &f.work,
        &[
            "commit",
            "--quiet",
            "--allow-empty",
            "-m",
            "Sibling descendant",
        ],
    );
    let sibling = git(&f.work, &["rev-parse", "HEAD"]);
    git(
        &f.work,
        &["push", "--quiet", utf8(&f.bare), "HEAD:refs/heads/sibling"],
    );
    assert_ne!(sibling, head);
    assert_eq!(
        git(&f.bare, &["rev-parse", "refs/heads/sibling^{tree}"]),
        git(&f.bare, &["rev-parse", "refs/heads/main^{tree}"])
    );
    f.assert_no_plan(&sibling, &head, "diverged");
}

#[cfg(unix)]
#[test]
fn daemon_summary_refuses_a_direct_cli_call_without_lifeline() {
    use std::os::unix::process::CommandExt;
    let f = Fixture::new();
    let output = Command::new(env!("CARGO_BIN_EXE_ai4math"))
        .arg("t3-discover")
        .arg(&f.root)
        .arg(&f.bare)
        .arg(ANCHOR)
        .arg(&f.base)
        .arg("--daemon-parent")
        .process_group(0)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("lifeline"));
}

#[test]
fn discovery_rejects_inputs_outside_its_managed_scope_without_writes() {
    let f = Fixture::new();
    let other = f._temp.path().join("unbound.git");
    git(
        f._temp.path(),
        &["clone", "--bare", "--quiet", "work", "unbound.git"],
    );
    assert!(!other.join("ai4math-promotion.lock").exists());
    let missing = "f".repeat(f.base.len());
    for (repo, anchor, baseline, diagnostic) in [
        (other.as_path(), ANCHOR, f.base.as_str(), "not bound"),
        (
            f.bare.as_path(),
            "Wrong.agda",
            f.base.as_str(),
            "primary engine",
        ),
        (
            f.bare.as_path(),
            "Outside.lean",
            f.base.as_str(),
            "include the target",
        ),
        (f.bare.as_path(), ANCHOR, "HEAD", "full Git object ID"),
        (
            f.bare.as_path(),
            ANCHOR,
            &f.base[..12],
            "full Git object ID",
        ),
        (f.bare.as_path(), ANCHOR, &missing, "rev-parse"),
    ] {
        let output = f.readonly_discover(repo, anchor, baseline);
        assert!(output.stdout.is_empty());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !output.status.success() && stderr.contains(diagnostic),
            "{repo:?} {anchor} {baseline}: {stderr}"
        );
    }
}
