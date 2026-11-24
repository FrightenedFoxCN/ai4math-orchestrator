//! Locked Git transactions for bounded, operator-trusted modules and Pivot ADRs.
use anyhow::{Context, Result, bail, ensure};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

const MAIN: &str = "refs/heads/main";
const MODULE_MANIFEST: &str = "ai4math-modules.txt";
const T2_RELEASE_POLICY: &str = "ai4math-t2-release.json";
const MAX_T2_RELEASE_POLICY_BYTES: usize = 8 * 1024;
const ADR_DOCS_DIR: &str = "docs";
const ADR_DIR: &str = "adr";
const MAX_PROJECT_FILES: usize = 129; // Modules plus the optional manifest.
const MAX_ADRS: usize = 256;
const MAX_ADR_BYTES: usize = 64 * 1024;
const MAX_TREE_BYTES: usize = 64 * 1024;

pub struct PromotionRepo {
    path: PathBuf,
    _lock: File,
}

pub struct PendingRef<'repo> {
    repo: &'repo Path,
    name: String,
}

pub struct Promotion<'repo> {
    repo: &'repo Path,
    base: String,
    main_at_lock: String,
    module: String,
    modules: Vec<String>,
    t2_release_policy_oid: Option<String>,
}

fn run_git(
    repo: &Path,
    args: &[&str],
    index: Option<&Path>,
    input: Option<&[u8]>,
) -> Result<Output> {
    let mut cmd = Command::new("/usr/bin/git");
    cmd.env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_AUTHOR_NAME", "AI4Math Broker")
        .env("GIT_AUTHOR_EMAIL", "broker@localhost")
        .env("GIT_COMMITTER_NAME", "AI4Math Broker")
        .env("GIT_COMMITTER_EMAIL", "broker@localhost")
        .arg("--git-dir")
        .arg(repo)
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.fsync=all",
        ])
        .args(args)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(index) = index {
        cmd.env("GIT_INDEX_FILE", index);
    }
    let mut child = cmd.spawn().context("start Git")?;
    let write = input.map(|data| child.stdin.take().expect("piped stdin").write_all(data));
    let out = child.wait_with_output()?;
    if let Some(result) = write.filter(|_| out.status.success()) {
        result?;
    }
    Ok(out)
}

fn git_success(args: &[&str], out: Output) -> Result<Vec<u8>> {
    ensure!(
        out.status.success(),
        "git {}: {}",
        args[0],
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(out.stdout)
}

fn git(repo: &Path, args: &[&str], index: Option<&Path>, input: Option<&[u8]>) -> Result<Vec<u8>> {
    git_success(args, run_git(repo, args, index, input)?)
}

fn reject_symbolic_ref(repo: &Path, name: &str) -> Result<()> {
    let args = ["symbolic-ref", "--no-recurse", "-q", name];
    let out = run_git(repo, &args, None, None)?;
    if out.status.code() == Some(1) {
        return Ok(()); // Direct or absent; the next ref operation distinguishes these.
    }
    git_success(&args, out)?;
    bail!("expected a direct Git ref: {name}");
}

fn text(bytes: Vec<u8>) -> Result<String> {
    Ok(String::from_utf8(bytes)?.trim_end().to_owned())
}

impl PromotionRepo {
    pub fn lock(repo: &Path) -> Result<Self> {
        Self::acquire_lock(repo, false, true)
    }

    /// Do not wait behind a native promotion after issuing an expiring lease.
    pub(crate) fn try_lock(repo: &Path) -> Result<Self> {
        Self::acquire_lock(repo, true, true)
    }

    /// Starter/supervisor paths can observe only a project whose operator
    /// already initialized the coordination file; they must not recreate it.
    pub(crate) fn try_lock_existing(repo: &Path) -> Result<Self> {
        Self::acquire_lock(repo, true, false)
    }

    fn acquire_lock(repo: &Path, no_wait: bool, create: bool) -> Result<Self> {
        let path = repo.canonicalize()?;
        ensure!(
            text(git(
                &path,
                &["rev-parse", "--is-bare-repository"],
                None,
                None
            )?)? == "true",
            "promotion requires an operator-owned bare Git repository"
        );
        let mut options = OpenOptions::new();
        options.write(true).create(create).truncate(false);
        #[cfg(unix)]
        if !create {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        let lock = options.open(path.join("ai4math-promotion.lock"))?;
        if !create {
            let metadata = lock.metadata()?;
            ensure!(
                metadata.is_file(),
                "expected an existing regular Git coordination file"
            );
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                ensure!(
                    metadata.nlink() == 1
                        && metadata.uid() == unsafe { libc::geteuid() }
                        && metadata.mode() & 0o022 == 0,
                    "unsafe managed Git coordination file"
                );
            }
        }
        if no_wait {
            fs2::FileExt::try_lock_exclusive(&lock).context("managed Git coordination is busy")?;
        } else {
            fs2::FileExt::lock_exclusive(&lock)?;
        }
        Ok(Self { path, _lock: lock })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn main_head(&self) -> Result<String> {
        reject_symbolic_ref(self.path(), MAIN)?;
        let head = text(git(
            self.path(),
            &["rev-parse", "--verify", MAIN],
            None,
            None,
        )?)?;
        self.require_commit(&head)?;
        Ok(head)
    }

    pub fn require_commit(&self, commit: &str) -> Result<()> {
        ensure!(valid_commit(commit), "provide a full Git object ID");
        let spec = format!("{commit}^{{commit}}");
        let found = text(git(
            &self.path,
            &["rev-parse", "--verify", &spec],
            None,
            None,
        )?)?;
        ensure!(found == commit, "Git object is not the requested commit");
        Ok(())
    }

    pub fn is_ancestor(&self, old: &str, new: &str) -> Result<bool> {
        self.require_commit(old)?;
        self.require_commit(new)?;
        let args = ["merge-base", "--is-ancestor", old, new];
        let output = run_git(&self.path, &args, None, None)?;
        if output.status.code() == Some(1) {
            return Ok(false);
        }
        git_success(&args, output)?;
        Ok(true)
    }

    pub fn pending_ref(&self, state_path: &Path) -> Result<PendingRef<'_>> {
        self.scoped_pending_ref("pending", state_path)
    }

    pub fn pivot_pending_ref(&self, state_path: &Path) -> Result<PendingRef<'_>> {
        self.scoped_pending_ref("pivot-pending", state_path)
    }

    fn scoped_pending_ref(&self, scope: &str, state_path: &Path) -> Result<PendingRef<'_>> {
        // Each workflow and ledger has its own slot; one recovery cannot unpin another.
        let state_path = state_path.canonicalize()?;
        let key = text(git(
            self.path(),
            &["hash-object", "--stdin"],
            None,
            Some(state_path.as_os_str().as_encoded_bytes()),
        )?)?;
        ensure!(valid_commit(&key), "invalid pending ref key");
        Ok(PendingRef {
            repo: self.path(),
            name: format!("refs/ai4math/{scope}/{key}"),
        })
    }

    /// Prepare an additive ADR against the exact current main, without moving any ref.
    pub fn prepare_adr(&self, base: &str, approved_id: &str, markdown: &str) -> Result<String> {
        ensure!(
            self.main_head()? == base,
            "main changed since the Pivot preview"
        );
        let name = adr_filename(approved_id)?;
        check_adr_text(markdown)?;
        let before = self.project_tree(base)?;
        before.check_adr_contents(self.path())?;
        before.can_add_adr(&name)?;
        let path = format!("{ADR_DOCS_DIR}/{ADR_DIR}/{name}");
        let tree = tree_with_blob(self.path(), base, &path, markdown.as_bytes())?;
        let commit = text(git(
            self.path(),
            &[
                "commit-tree",
                "--no-gpg-sign",
                &tree,
                "-p",
                base,
                "-m",
                "AI4Math: approved Pivot ADR",
            ],
            None,
            None,
        )?)?;
        self.check_adr_commit(&before, base, &commit, &name, markdown)?;
        Ok(commit)
    }

    /// Validate the journal's exact single-parent commit and bytes without changing Git.
    /// This also works after the commit has been published and other commits follow it.
    pub fn validate_adr(
        &self,
        base: &str,
        commit: &str,
        approved_id: &str,
        markdown: &str,
    ) -> Result<()> {
        let name = adr_filename(approved_id)?;
        check_adr_text(markdown)?;
        let before = self.project_tree(base)?;
        before.check_adr_contents(self.path())?;
        before.can_add_adr(&name)?;
        self.check_adr_commit(&before, base, commit, &name, markdown)
    }

    /// Only a validated ADR can advance main, and only while main still equals base.
    pub fn advance_adr(
        &self,
        base: &str,
        commit: &str,
        approved_id: &str,
        markdown: &str,
    ) -> Result<()> {
        self.validate_adr(base, commit, approved_id, markdown)?;
        ensure!(
            self.main_head()? == base,
            "main changed before ADR publication"
        );
        git(
            self.path(),
            &["update-ref", "--no-deref", MAIN, commit, base],
            None,
            None,
        )
        .context("main did not advance; ADR commit was not published")?;
        Ok(())
    }

    pub(crate) fn contains_module_at(&self, commit: &str, module: &str) -> Result<bool> {
        module_extension(module).context("invalid root-level project module name")?;
        let tree = self.project_tree(commit)?;
        tree.check_adr_contents(self.path())?;
        Ok(tree.files.contains_key(module))
    }

    /// Check an already-published single-module promotion using only existing objects.
    /// Unlike candidate-tree construction, this does not create a Git object or index.
    pub(crate) fn assert_exact_proof_commit(
        &self,
        base: &str,
        commit: &str,
        module: &str,
        source: &[u8],
    ) -> Result<()> {
        module_extension(module).context("invalid managed proof module")?;
        let mut before = self.project_tree(base)?;
        self.require_commit(commit)?;
        let raw = read_object(self.path(), "commit", commit, MAX_TREE_BYTES)?;
        let mut headers = raw
            .split(|&b| b == b'\n')
            .take_while(|line| !line.is_empty());
        let tree = headers
            .next()
            .and_then(|line| line.strip_prefix(b"tree "))
            .context("missing promoted proof tree")?;
        ensure!(
            headers.next() == Some(format!("parent {base}").as_bytes())
                && !headers.any(|line| line.starts_with(b"parent ") || line.starts_with(b"tree ")),
            "managed proof must have the exact approved base as its only parent"
        );
        let mut after = ProjectTree::read(self.path(), std::str::from_utf8(tree)?)?;
        let old = before
            .files
            .remove(module)
            .context("frozen module is missing from the approved proof base")?;
        let new = after
            .files
            .remove(module)
            .context("promoted proof module is missing from Git")?;
        ensure!(
            old != new && before.files == after.files && before.adrs == after.adrs,
            "managed proof commit must change only the promoted module"
        );
        ensure!(
            read_object(self.path(), "blob", &new, 2 * 1024 * 1024)? == source,
            "managed proof module does not match the retained recipe bytes"
        );
        Ok(())
    }

    fn project_tree(&self, commit: &str) -> Result<ProjectTree> {
        self.require_commit(commit)?;
        let spec = format!("{commit}^{{tree}}");
        let tree = text(git(
            self.path(),
            &["rev-parse", "--verify", &spec],
            None,
            None,
        )?)?;
        ProjectTree::read(self.path(), &tree)
    }

    fn check_adr_commit(
        &self,
        before: &ProjectTree,
        base: &str,
        commit: &str,
        name: &str,
        markdown: &str,
    ) -> Result<()> {
        self.require_commit(commit)?;
        // Inspect raw headers: the parent must be exact, not a history view via a graft.
        let raw = read_object(self.path(), "commit", commit, MAX_TREE_BYTES)?;
        let mut headers = raw
            .split(|&b| b == b'\n')
            .take_while(|line| !line.is_empty());
        let tree = headers
            .next()
            .and_then(|line| line.strip_prefix(b"tree "))
            .context("missing prepared ADR tree")?;
        ensure!(
            headers.next() == Some(format!("parent {base}").as_bytes())
                && !headers.any(|line| line.starts_with(b"parent ") || line.starts_with(b"tree ")),
            "prepared ADR must have the exact base as its only parent"
        );
        let mut after = ProjectTree::read(self.path(), std::str::from_utf8(tree)?)?;
        let added = after
            .adrs
            .remove(name)
            .context("prepared ADR path is missing")?;
        // Trees are canonical and shape-checked, so comparing these complete sets
        // accounts for every entry, including directory modes and empty subtrees.
        ensure!(
            before.files == after.files && before.adrs == after.adrs,
            "prepared commit must add only the approved ADR"
        );
        ensure!(
            read_object(self.path(), "blob", &added, MAX_ADR_BYTES)? == markdown.as_bytes(),
            "prepared ADR bytes do not match approval"
        );
        Ok(())
    }

    pub fn snapshot(&self, module: &str, prior: Option<&str>) -> Result<Promotion<'_>> {
        let ext = module_extension(module)
            .context("invalid root-level Lean, Agda, or Rocq module name")?;
        let repo = self.path();
        let main_at_lock = self.main_head()?;
        let base = prior.unwrap_or(&main_at_lock).to_owned();
        let mut tree = self.project_tree(&base)?;
        tree.check_adr_contents(repo)?;
        // The policy is part of the exact project tree, not a proof input.
        let t2_release_policy_oid = tree.files.remove(T2_RELEASE_POLICY);
        let mut tracked: BTreeSet<_> = tree.files.into_keys().collect();
        let modules: Vec<String> = if tracked.remove(MODULE_MANIFEST) {
            let list = Promotion::read_blob(repo, &base, MODULE_MANIFEST)?;
            list.lines()
                .map(|line| line.trim().to_owned())
                .filter(|line| !line.is_empty())
                .collect()
        } else {
            ensure!(
                tracked.len() == 1,
                "multiple modules require a tracked ai4math-modules.txt in import order"
            );
            tracked.iter().cloned().collect()
        };
        let declared: BTreeSet<_> = modules.iter().cloned().collect();
        ensure!(
            declared == tracked && declared.len() == modules.len() && declared.contains(module),
            "module list must cover all tracked modules exactly once and include the target"
        );
        ensure!(
            modules.iter().all(|p| module_extension(p) == Some(ext)),
            "all modules must be root-level and use the same proof engine"
        );
        Ok(Promotion {
            repo,
            base,
            main_at_lock,
            module: module.to_owned(),
            modules,
            t2_release_policy_oid,
        })
    }
}

fn adr_filename(approved_id: &str) -> Result<String> {
    let digest = approved_id
        .strip_prefix("ADR-PIVOT-")
        .context("invalid Pivot ADR ID")?;
    ensure!(
        digest.len() == 64 && digest.bytes().all(lower_hex),
        "invalid Pivot ADR ID"
    );
    Ok(format!("{approved_id}.md"))
}

fn check_adr_text(markdown: &str) -> Result<()> {
    ensure!(
        !markdown.is_empty() && markdown.len() <= MAX_ADR_BYTES && !markdown.contains('\0'),
        "ADR must be nonempty UTF-8 text of at most {MAX_ADR_BYTES} bytes without NULs"
    );
    Ok(())
}

fn module_extension(name: &str) -> Option<&str> {
    let (stem, ext) = name.split_once('.')?;
    (name.len() <= 255
        && matches!(ext, "lean" | "agda" | "v")
        && stem.starts_with(|c: char| c.is_ascii_uppercase())
        && stem.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_'))
    .then_some(ext)
}

fn read_object(repo: &Path, kind: &str, oid: &str, max_bytes: usize) -> Result<Vec<u8>> {
    ensure!(valid_commit(oid), "invalid Git object ID");
    ensure!(
        text(git(repo, &["cat-file", "-t", oid], None, None)?)? == kind,
        "expected Git {kind} object"
    );
    let size = text(git(repo, &["cat-file", "-s", oid], None, None)?)?.parse::<usize>()?;
    ensure!(size <= max_bytes, "Git {kind} exceeds the supported size");
    let bytes = git(repo, &["cat-file", kind, oid], None, None)?;
    ensure!(bytes.len() == size, "Git {kind} size changed while reading");
    Ok(bytes)
}

#[derive(PartialEq, Eq)]
enum TreeMode {
    File,
    Dir,
}

struct TreeEntry {
    name: String,
    mode: TreeMode,
    oid: String,
}

fn read_tree(repo: &Path, oid: &str, max_entries: usize) -> Result<Vec<TreeEntry>> {
    // The raw form lets us reject noncanonical modes and see even empty subtrees.
    // ls-tree's recursive file listing misses empty trees and may normalize modes.
    let raw = read_object(repo, "tree", oid, MAX_TREE_BYTES)?;
    let mut remaining = raw.as_slice();
    let mut entries = Vec::<TreeEntry>::new();
    let mut folded = BTreeSet::new();
    while !remaining.is_empty() {
        ensure!(entries.len() < max_entries, "unsupported project tree size");
        let nul = remaining
            .iter()
            .position(|&b| b == 0)
            .context("malformed Git tree")?;
        let (mode, name) = std::str::from_utf8(&remaining[..nul])?
            .split_once(' ')
            .context("malformed Git tree entry")?;
        let mode = match mode {
            "100644" => TreeMode::File,
            "40000" => TreeMode::Dir,
            _ => {
                bail!("project tree requires non-executable regular files and approved directories")
            }
        };
        ensure!(
            !name.is_empty() && folded.insert(name.to_ascii_lowercase()),
            "invalid or case-fold colliding project tree name"
        );
        // The admitted path grammar has no directory/file prefix pairs, so ordinary
        // byte order is also Git tree order for all valid entries.
        ensure!(
            entries
                .last()
                .is_none_or(|previous| previous.name.as_str() < name),
            "project tree entries are not in canonical order"
        );
        let end = nul + 1 + oid.len() / 2;
        let hash = remaining
            .get(nul + 1..end)
            .context("truncated Git tree entry")?;
        let oid = hash.iter().map(|b| format!("{b:02x}")).collect();
        entries.push(TreeEntry {
            name: name.to_owned(),
            mode,
            oid,
        });
        remaining = &remaining[end..];
    }
    Ok(entries)
}

struct ProjectTree {
    files: BTreeMap<String, String>,
    adrs: BTreeMap<String, String>,
}

impl ProjectTree {
    fn read(repo: &Path, oid: &str) -> Result<Self> {
        let mut files = BTreeMap::new();
        let mut adrs = BTreeMap::new();
        // The root may additionally contain the ADR directory and the T2 policy.
        for entry in read_tree(repo, oid, MAX_PROJECT_FILES + 2)? {
            if entry.name == ADR_DOCS_DIR {
                ensure!(
                    entry.mode == TreeMode::Dir,
                    "docs must be the approved ADR directory tree"
                );
                let docs = read_tree(repo, &entry.oid, 1)?;
                let [adr] = docs.as_slice() else {
                    bail!("docs must contain only the approved ADR directory");
                };
                ensure!(
                    adr.name == ADR_DIR && adr.mode == TreeMode::Dir,
                    "docs must contain only the approved ADR directory"
                );
                for file in read_tree(repo, &adr.oid, MAX_ADRS)? {
                    let id = file
                        .name
                        .strip_suffix(".md")
                        .context("invalid Pivot ADR file")?;
                    adr_filename(id)?;
                    ensure!(
                        file.mode == TreeMode::File,
                        "Pivot ADR must be a regular file"
                    );
                    adrs.insert(file.name, file.oid);
                }
                ensure!(!adrs.is_empty(), "empty ADR directory is not supported");
            } else {
                ensure!(
                    entry.mode == TreeMode::File
                        && (entry.name == MODULE_MANIFEST
                            || entry.name == T2_RELEASE_POLICY
                            || module_extension(&entry.name).is_some()),
                    "project tree permits only root-level proof modules, their manifest, the T2 release policy, and Pivot ADRs"
                );
                files.insert(entry.name, entry.oid);
            }
        }
        let project_inputs = files.len() - usize::from(files.contains_key(T2_RELEASE_POLICY));
        ensure!(
            (1..=MAX_PROJECT_FILES).contains(&project_inputs),
            "unsupported project tree size"
        );
        Ok(Self { files, adrs })
    }

    fn can_add_adr(&self, name: &str) -> Result<()> {
        ensure!(!self.adrs.contains_key(name), "approved ADR already exists");
        ensure!(self.adrs.len() < MAX_ADRS, "too many project ADRs");
        Ok(())
    }

    fn check_adr_contents(&self, repo: &Path) -> Result<()> {
        for blob in self.adrs.values() {
            let bytes = read_object(repo, "blob", blob, MAX_ADR_BYTES)?;
            check_adr_text(std::str::from_utf8(&bytes).context("Pivot ADR is not UTF-8")?)?;
        }
        Ok(())
    }
}

impl PendingRef<'_> {
    pub fn current(&self) -> Result<Option<String>> {
        reject_symbolic_ref(self.repo, &self.name)?;
        let rows = String::from_utf8(git(
            self.repo,
            &[
                "for-each-ref",
                "--format=%(refname)%09%(objectname)%09%(symref)",
                &self.name,
            ],
            None,
            None,
        )?)?;
        let prefix = format!("{}\t", self.name);
        if let Some(row) = rows.lines().find_map(|row| row.strip_prefix(&prefix)) {
            let (commit, symref) = row.split_once('\t').context("malformed pending Git ref")?;
            ensure!(
                symref.is_empty() && valid_commit(commit),
                "invalid pending Git ref"
            );
            return Ok(Some(commit.to_owned()));
        }
        Ok(None)
    }

    pub fn ensure_pinned(&self, commit: &str) -> Result<()> {
        ensure!(valid_commit(commit), "invalid prepared commit ID");
        ensure!(
            text(
                git(self.repo, &["cat-file", "-t", commit], None, None)
                    .context("prepared promotion commit is unavailable")?
            )? == "commit",
            "prepared promotion object is not a commit"
        );
        if let Some(current) = self.current()? {
            ensure!(
                current == commit,
                "pending Git ref disagrees with the promotion journal"
            );
            return Ok(());
        }
        let absent = "0".repeat(commit.len());
        git(
            self.repo,
            &["update-ref", "--no-deref", &self.name, commit, &absent],
            None,
            None,
        )
        .context("could not pin prepared promotion commit")?;
        Ok(())
    }

    pub fn remove(&self, commit: &str) -> Result<()> {
        ensure!(valid_commit(commit), "invalid prepared commit ID");
        reject_symbolic_ref(self.repo, &self.name)?;
        git(
            self.repo,
            &["update-ref", "--no-deref", "-d", &self.name, commit],
            None,
            None,
        )
        .context("could not remove pending Git ref")?;
        Ok(())
    }

    pub fn clear(&self) -> Result<()> {
        // Only safe after the coordinator has confirmed that this ledger has no intent.
        if let Some(commit) = self.current()? {
            self.remove(&commit)?;
        }
        Ok(())
    }
}

impl Promotion<'_> {
    pub fn base(&self) -> &str {
        &self.base
    }

    pub(crate) fn module(&self) -> &str {
        &self.module
    }

    pub fn repo(&self) -> &Path {
        self.repo
    }
    pub fn main_at_lock(&self) -> &str {
        &self.main_at_lock
    }

    /// Read the exact raw policy bytes selected by the pinned project tree.
    pub fn t2_release_policy(&self) -> Result<Vec<u8>> {
        let oid = self
            .t2_release_policy_oid
            .as_deref()
            .with_context(|| format!("pinned project is missing {T2_RELEASE_POLICY}"))?;
        read_object(self.repo, "blob", oid, MAX_T2_RELEASE_POLICY_BYTES).with_context(|| {
            format!("read pinned {T2_RELEASE_POLICY} (limit {MAX_T2_RELEASE_POLICY_BYTES} bytes)")
        })
    }

    fn read_blob(repo: &Path, base: &str, filename: &str) -> Result<String> {
        let spec = format!("{base}:{filename}");
        let size = text(git(repo, &["cat-file", "-s", &spec], None, None)?)?.parse::<u64>()?;
        ensure!(
            size <= 2 * 1024 * 1024,
            "tracked blob exceeds limits: {filename}"
        );
        Ok(String::from_utf8(git(
            repo,
            &["cat-file", "blob", &spec],
            None,
            None,
        )?)?)
    }

    pub fn project(&self) -> Result<Vec<(&str, String)>> {
        self.modules
            .iter()
            .map(|path| Ok((path.as_str(), Self::read_blob(self.repo, &self.base, path)?)))
            .collect()
    }

    pub fn prepare_commit(
        &self,
        source: &[u8],
        receipt: &ai4math_broker::VerificationReceipt,
    ) -> Result<String> {
        ensure!(
            receipt.tier() == ai4math_broker::ReceiptTier::T1Promotion
                && receipt.matches(&self.base, &self.module, source),
            "receipt does not match base or candidate"
        );
        let tree = self.candidate_tree(source)?;
        let receipt_json = serde_json::to_string(receipt)?;
        text(git(
            self.repo,
            &[
                "commit-tree",
                &tree,
                "-p",
                &self.base,
                "-m",
                "AI4Math: verified proof body",
                "-m",
                &receipt_json,
            ],
            None,
            None,
        )?)
    }

    fn candidate_tree(&self, source: &[u8]) -> Result<String> {
        tree_with_blob(self.repo, &self.base, &self.module, source)
    }

    pub fn assert_prepared_commit(&self, commit: &str, source: &[u8]) -> Result<()> {
        ensure!(valid_commit(commit), "invalid prepared commit ID");
        let info = text(git(
            self.repo,
            &["show", "-s", "--format=%T %P", commit],
            None,
            None,
        )?)?;
        let expected = format!("{} {}", self.candidate_tree(source)?, self.base);
        ensure!(
            info == expected,
            "prepared commit no longer matches the re-verified source and exact parent tree"
        );
        Ok(())
    }

    pub fn published(&self, commit: &str) -> Result<bool> {
        ensure!(valid_commit(commit), "invalid prepared commit ID");
        self.includes_commit(&self.main_at_lock, commit)
    }

    pub fn currently_published(&self, commit: &str) -> Result<bool> {
        let head = text(git(
            self.repo,
            &["rev-parse", "--verify", MAIN],
            None,
            None,
        )?)?;
        self.includes_commit(&head, commit)
    }

    fn includes_commit(&self, head: &str, commit: &str) -> Result<bool> {
        ensure!(valid_commit(commit), "invalid prepared commit ID");
        let args = ["merge-base", "--is-ancestor", commit, head];
        let out = run_git(self.repo, &args, None, None)?;
        if out.status.code() == Some(1) {
            return Ok(false);
        }
        git_success(&args, out)?;
        Ok(true)
    }

    pub fn advance(&self, commit: &str) -> Result<()> {
        ensure!(valid_commit(commit), "invalid prepared commit ID");
        git(
            self.repo,
            &["update-ref", "--no-deref", MAIN, commit, &self.base],
            None,
            None,
        )
        .context("main did not advance; candidate commit was not promoted")?;
        Ok(())
    }

    pub fn commit(
        &self,
        source: &[u8],
        receipt: &ai4math_broker::VerificationReceipt,
    ) -> Result<String> {
        let commit = self.prepare_commit(source, receipt)?;
        self.advance(&commit)?;
        Ok(commit)
    }
}

// The only callers derive the path from a vetted root-level module or Pivot ADR ID.
fn tree_with_blob(repo: &Path, base: &str, path: &str, source: &[u8]) -> Result<String> {
    let scratch = tempfile::tempdir_in(repo)?;
    let index = scratch.path().join("index");
    git(repo, &["read-tree", base], Some(&index), None)?;
    let blob = text(git(
        repo,
        &["hash-object", "-w", "--stdin"],
        None,
        Some(source),
    )?)?;
    git(
        repo,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("100644,{blob},{path}"),
        ],
        Some(&index),
        None,
    )?;
    text(git(repo, &["write-tree"], Some(&index), None)?)
}

fn lower_hex(byte: u8) -> bool {
    byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)
}

fn valid_commit(commit: &str) -> bool {
    matches!(commit.len(), 40 | 64)
        && commit.bytes().any(|b| b != b'0')
        && commit.bytes().all(lower_hex)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bare() -> Result<(tempfile::TempDir, PromotionRepo)> {
        let temp = tempfile::tempdir()?;
        git(
            temp.path(),
            &["init", "--bare", "--initial-branch=main"],
            None,
            None,
        )?;
        let repo = PromotionRepo::lock(temp.path())?;
        Ok((temp, repo))
    }

    fn branch(repo: &PromotionRepo) -> Result<String> {
        let tree = text(git(repo.path(), &["mktree"], None, Some(b""))?)?;
        let commit = text(git(
            repo.path(),
            &["commit-tree", &tree, "-m", "empty"],
            None,
            None,
        )?)?;
        let tree = tree_with_blob(
            repo.path(),
            &commit,
            "B.lean",
            b"theorem b : True := trivial\n",
        )?;
        let commit = commit_tree(repo, &tree, &commit)?;
        let tree = tree_with_blob(repo.path(), &commit, "A.lean", b"import B\n")?;
        let commit = commit_tree(repo, &tree, &commit)?;
        let tree = tree_with_blob(repo.path(), &commit, MODULE_MANIFEST, b"B.lean\nA.lean\n")?;
        let commit = commit_tree(repo, &tree, &commit)?;
        git(repo.path(), &["update-ref", MAIN, &commit], None, None)?;
        Ok(commit)
    }

    fn commit_tree(repo: &PromotionRepo, tree: &str, parent: &str) -> Result<String> {
        text(git(
            repo.path(),
            &["commit-tree", tree, "-p", parent, "-m", "fixture"],
            None,
            None,
        )?)
    }

    fn id(digit: char) -> String {
        format!("ADR-PIVOT-{}", digit.to_string().repeat(64))
    }

    #[test]
    fn t2_release_policy_reads_pinned_raw_bytes_and_preserves_module_order() -> Result<()> {
        let (_temp, repo) = bare()?;
        let base = branch(&repo)?;
        let raw = b"\xff\0\r\n ";
        let tree = tree_with_blob(repo.path(), &base, T2_RELEASE_POLICY, raw)?;
        let reviewed = commit_tree(&repo, &tree, &base)?;
        git(
            repo.path(),
            &["update-ref", MAIN, &reviewed, &base],
            None,
            None,
        )?;
        let project = repo.snapshot("A.lean", None)?;
        let tree = tree_with_blob(repo.path(), &reviewed, T2_RELEASE_POLICY, b"current")?;
        let current = commit_tree(&repo, &tree, &reviewed)?;
        git(
            repo.path(),
            &["update-ref", MAIN, &current, &reviewed],
            None,
            None,
        )?;

        assert_eq!(project.t2_release_policy()?, raw);
        assert_eq!(
            repo.snapshot("A.lean", Some(&reviewed))?
                .t2_release_policy()?,
            raw
        );
        assert_eq!(
            repo.snapshot("A.lean", None)?.t2_release_policy()?,
            b"current"
        );
        assert_eq!(
            project.project()?,
            repo.snapshot("A.lean", Some(&base))?.project()?
        );
        assert_eq!(project.modules, ["B.lean", "A.lean"]);
        assert!(
            repo.snapshot("A.lean", Some(&base))?
                .t2_release_policy()
                .unwrap_err()
                .to_string()
                .contains("missing ai4math-t2-release.json")
        );
        Ok(())
    }

    #[test]
    fn t2_release_policy_enforces_the_inclusive_byte_limit() -> Result<()> {
        let (_temp, repo) = bare()?;
        let base = branch(&repo)?;
        let mut bytes = vec![b'x'; 8192];
        let tree = tree_with_blob(repo.path(), &base, T2_RELEASE_POLICY, &bytes)?;
        let boundary = commit_tree(&repo, &tree, &base)?;
        assert_eq!(
            repo.snapshot("A.lean", Some(&boundary))?
                .t2_release_policy()?,
            bytes
        );
        bytes.push(b'x');
        let tree = tree_with_blob(repo.path(), &base, T2_RELEASE_POLICY, &bytes)?;
        let oversized = commit_tree(&repo, &tree, &base)?;
        let error = repo
            .snapshot("A.lean", Some(&oversized))?
            .t2_release_policy()
            .unwrap_err();
        assert!(format!("{error:#}").contains("Git blob exceeds the supported size"));
        Ok(())
    }

    #[test]
    fn t2_release_policy_rejects_symlinks_executables_and_other_paths() -> Result<()> {
        let (_temp, repo) = bare()?;
        let base = branch(&repo)?;
        let blob = text(git(
            repo.path(),
            &["hash-object", "-w", "--stdin"],
            None,
            Some(b"A.lean"),
        )?)?;
        // Construct modes in Git, independent of the host filesystem's capabilities.
        for mode in ["120000", "100755"] {
            let mut rows = git(repo.path(), &["ls-tree", "-z", &base], None, None)?;
            rows.extend_from_slice(format!("{mode} blob {blob}\t{T2_RELEASE_POLICY}\0").as_bytes());
            let tree = text(git(repo.path(), &["mktree", "-z"], None, Some(&rows))?)?;
            let candidate = commit_tree(&repo, &tree, &base)?;
            assert!(
                repo.snapshot("A.lean", Some(&candidate)).is_err(),
                "accepted {mode}"
            );
        }
        for path in [
            "AI4Math-t2-release.json",
            "ai4math-t2-release.json/contents",
            "docs/adr/ai4math-t2-release.json",
        ] {
            let tree = tree_with_blob(repo.path(), &base, path, b"{}")?;
            let candidate = commit_tree(&repo, &tree, &base)?;
            assert!(
                repo.snapshot("A.lean", Some(&candidate)).is_err(),
                "accepted {path}"
            );
        }
        Ok(())
    }

    #[test]
    fn t2_release_policy_cannot_be_smuggled_into_proof_or_adr_commits() -> Result<()> {
        let (_temp, repo) = bare()?;
        let absent = branch(&repo)?;
        let tree = tree_with_blob(repo.path(), &absent, T2_RELEASE_POLICY, b"before")?;
        let present = commit_tree(&repo, &tree, &absent)?;
        let tree = tree_with_blob(repo.path(), &present, T2_RELEASE_POLICY, b"after")?;
        let changed = commit_tree(&repo, &tree, &present)?;
        let source = b"import B\n-- promoted module\n";
        let approved = id('a');
        let markdown = "# Reviewed ADR\n";
        let adr_path = format!("docs/adr/{approved}.md");

        for (base, extra) in [
            (&absent, &present),
            (&present, &absent),
            (&present, &changed),
        ] {
            let project = repo.snapshot("A.lean", Some(base))?;
            let tree = project.candidate_tree(source)?;
            let proof = commit_tree(&repo, &tree, base)?;
            repo.assert_exact_proof_commit(base, &proof, "A.lean", source)?;
            project.assert_prepared_commit(&proof, source)?;
            let tree = tree_with_blob(repo.path(), extra, "A.lean", source)?;
            let smuggled = commit_tree(&repo, &tree, base)?;
            assert!(
                repo.assert_exact_proof_commit(base, &smuggled, "A.lean", source)
                    .is_err()
            );
            assert!(project.assert_prepared_commit(&smuggled, source).is_err());

            let tree = tree_with_blob(repo.path(), base, &adr_path, markdown.as_bytes())?;
            let adr = commit_tree(&repo, &tree, base)?;
            repo.validate_adr(base, &adr, &approved, markdown)?;
            let tree = tree_with_blob(repo.path(), extra, &adr_path, markdown.as_bytes())?;
            let smuggled = commit_tree(&repo, &tree, base)?;
            assert!(
                repo.validate_adr(base, &smuggled, &approved, markdown)
                    .is_err()
            );
        }
        Ok(())
    }

    #[test]
    fn t2_release_policy_does_not_change_project_file_count_limits() -> Result<()> {
        let (_temp, repo) = bare()?;
        let parent = branch(&repo)?;
        let blob = text(git(
            repo.path(),
            &["hash-object", "-w", "--stdin"],
            None,
            Some(b"{}"),
        )?)?;
        let mut rows = format!("100644 blob {blob}\t{T2_RELEASE_POLICY}\0");
        let only_policy = text(git(
            repo.path(),
            &["mktree", "-z"],
            None,
            Some(rows.as_bytes()),
        )?)?;
        assert!(ProjectTree::read(repo.path(), &only_policy).is_err());
        rows.push_str(&format!("100644 blob {blob}\tA000.lean\0"));
        let single = text(git(
            repo.path(),
            &["mktree", "-z"],
            None,
            Some(rows.as_bytes()),
        )?)?;
        let single = commit_tree(&repo, &single, &parent)?;
        assert_eq!(
            repo.snapshot("A000.lean", Some(&single))?.modules,
            ["A000.lean"]
        );

        let mut modules: Vec<_> = (0..128).map(|i| format!("A{i:03}.lean")).collect();
        for name in &modules[1..] {
            rows.push_str(&format!("100644 blob {blob}\t{name}\0"));
        }
        modules.reverse();
        let manifest = text(git(
            repo.path(),
            &["hash-object", "-w", "--stdin"],
            None,
            Some(modules.join("\n").as_bytes()),
        )?)?;
        rows.push_str(&format!("100644 blob {manifest}\t{MODULE_MANIFEST}\0"));
        let tree = text(git(
            repo.path(),
            &["mktree", "-z"],
            None,
            Some(rows.as_bytes()),
        )?)?;
        let base = commit_tree(&repo, &tree, &parent)?;
        git(
            repo.path(),
            &["update-ref", MAIN, &base, &parent],
            None,
            None,
        )?;
        let adr = repo.prepare_adr(&base, &id('a'), "# Fits with policy at the module limit\n")?;
        assert_eq!(repo.snapshot("A000.lean", Some(&adr))?.modules, modules);

        // With no ADR, the extra root slot must not allow one more proof input.
        rows.push_str(&format!("100644 blob {blob}\tZ.lean\0"));
        let too_many = text(git(
            repo.path(),
            &["mktree", "-z"],
            None,
            Some(rows.as_bytes()),
        )?)?;
        assert!(ProjectTree::read(repo.path(), &too_many).is_err());
        Ok(())
    }

    #[test]
    fn read_only_proof_commit_validation_rejects_an_extra_edit_and_wrong_parent() -> Result<()> {
        let (_temp, repo) = bare()?;
        let base = branch(&repo)?;
        let source = b"import B\n-- promoted module\n";
        let tree = tree_with_blob(repo.path(), &base, "A.lean", source)?;
        let proof = commit_tree(&repo, &tree, &base)?;
        repo.assert_exact_proof_commit(&base, &proof, "A.lean", source)?;
        assert!(
            repo.assert_exact_proof_commit(&base, &proof, "A.lean", b"not those bytes")
                .is_err()
        );
        assert!(
            repo.assert_exact_proof_commit(&base, &proof, "B.lean", source)
                .is_err()
        );
        let extra = tree_with_blob(repo.path(), &proof, "B.lean", b"-- unwanted edit")?;
        let extra = commit_tree(&repo, &extra, &base)?;
        assert!(
            repo.assert_exact_proof_commit(&base, &extra, "A.lean", source)
                .is_err()
        );
        let sidelined = commit_tree(&repo, &tree, &proof)?;
        assert!(
            repo.assert_exact_proof_commit(&base, &sidelined, "A.lean", source)
                .is_err()
        );
        assert_eq!(repo.main_head()?, base);
        Ok(())
    }

    #[test]
    fn adr_round_trip_does_not_change_proof_inputs() -> Result<()> {
        let (_temp, repo) = bare()?;
        let base = branch(&repo)?;
        let approved = id('a');
        let markdown = "# 新路线\r\n\n不改证明 ✓  ";
        let commit = repo.prepare_adr(&base, &approved, markdown)?;
        assert_eq!(repo.main_head()?, base);
        repo.validate_adr(&base, &commit, &approved, markdown)?;
        repo.advance_adr(&base, &commit, &approved, markdown)?;
        assert_eq!(repo.main_head()?, commit);
        let project = repo.snapshot("A.lean", None)?;
        assert_eq!(
            project.project()?,
            vec![
                ("B.lean", "theorem b : True := trivial\n".into()),
                ("A.lean", "import B\n".into())
            ]
        );
        let proof_tree = project.candidate_tree(b"import B\n-- proof edit\n")?;
        let after = ProjectTree::read(repo.path(), &proof_tree)?;
        assert_eq!(after.adrs, repo.project_tree(&commit)?.adrs);
        assert!(repo.prepare_adr(&commit, &approved, "duplicate").is_err());
        let second = id('b');
        let next = repo.prepare_adr(&commit, &second, "# Next decision\n")?;
        repo.advance_adr(&commit, &next, &second, "# Next decision\n")?;
        repo.validate_adr(&base, &commit, &approved, markdown)?;
        assert!(repo.is_ancestor(&commit, &next)?);
        Ok(())
    }

    #[test]
    fn adr_validation_requires_exact_parent_path_tree_and_bytes() -> Result<()> {
        let (_temp, repo) = bare()?;
        let base = branch(&repo)?;
        let approved = id('b');
        let markdown = "# Approved decision\n";
        let commit = repo.prepare_adr(&base, &approved, markdown)?;
        assert!(
            repo.validate_adr(&base, &commit, &approved, "# Approved decision")
                .is_err()
        );
        assert!(
            repo.validate_adr(&base, &commit, &id('c'), markdown)
                .is_err()
        );
        let adr_tree = text(git(
            repo.path(),
            &["rev-parse", &format!("{commit}^{{tree}}")],
            None,
            None,
        )?)?;
        let no_parent = text(git(
            repo.path(),
            &["commit-tree", &adr_tree, "-m", "fixture"],
            None,
            None,
        )?)?;
        assert!(
            repo.validate_adr(&base, &no_parent, &approved, markdown)
                .is_err()
        );
        let merge = text(git(
            repo.path(),
            &[
                "commit-tree",
                &adr_tree,
                "-p",
                &base,
                "-p",
                &no_parent,
                "-m",
                "fixture",
            ],
            None,
            None,
        )?)?;
        assert!(
            repo.validate_adr(&base, &merge, &approved, markdown)
                .is_err()
        );
        let extra = tree_with_blob(repo.path(), &commit, "B.lean", b"-- unrelated edit")?;
        let extra = commit_tree(&repo, &extra, &base)?;
        assert!(
            repo.advance_adr(&base, &extra, &approved, markdown)
                .is_err()
        );
        assert_eq!(repo.main_head()?, base);
        Ok(())
    }

    #[test]
    fn adr_publication_rejects_stale_main_and_unapproved_inputs() -> Result<()> {
        let (_temp, repo) = bare()?;
        let base = branch(&repo)?;
        let approved = id('d');
        for invalid in ["ADR-0001", "../ADR-0001", "docs/adr/file.md", &id('A')] {
            assert!(repo.prepare_adr(&base, invalid, "# Decision\n").is_err());
        }
        for invalid in ["", "bad\0text", &"é".repeat(MAX_ADR_BYTES / 2 + 1)] {
            assert!(repo.prepare_adr(&base, &approved, invalid).is_err());
        }
        let candidate = repo.prepare_adr(&base, &approved, "# Decision\n")?;
        let other = repo.prepare_adr(&base, &id('e'), "# Different decision\n")?;
        repo.advance_adr(&base, &other, &id('e'), "# Different decision\n")?;
        assert!(repo.prepare_adr(&base, &approved, "# Decision\n").is_err());
        assert!(
            repo.advance_adr(&base, &candidate, &approved, "# Decision\n")
                .is_err()
        );
        assert_eq!(repo.main_head()?, other);
        Ok(())
    }

    #[test]
    fn snapshots_reject_non_adr_paths_and_malformed_directory_entries() -> Result<()> {
        let (_temp, repo) = bare()?;
        let base = branch(&repo)?;
        for path in [
            "Docs/adr/note.md".to_owned(),
            "docs/adr/Bad.lean".to_owned(),
            format!("docs/adr/nested/{}.md", id('a')),
            "docs".to_owned(),
        ] {
            let tree = tree_with_blob(repo.path(), &base, &path, b"# Note")?;
            let candidate = commit_tree(&repo, &tree, &base)?;
            assert!(
                repo.snapshot("A.lean", Some(&candidate)).is_err(),
                "accepted {path}"
            );
        }
        let adr_path = format!("docs/adr/{}.md", id('a'));
        let binary = tree_with_blob(repo.path(), &base, &adr_path, &[0xff])?;
        let candidate = commit_tree(&repo, &binary, &base)?;
        assert!(repo.snapshot("A.lean", Some(&candidate)).is_err());
        let empty = text(git(repo.path(), &["mktree"], None, Some(b""))?)?;
        let content = git(repo.path(), &["ls-tree", "-z", &base], None, None)?;
        let mut rows = content;
        rows.extend_from_slice(format!("040000 tree {empty}\textra\0").as_bytes());
        let unexpected_empty = text(git(repo.path(), &["mktree", "-z"], None, Some(&rows))?)?;
        let candidate = commit_tree(&repo, &unexpected_empty, &base)?;
        assert!(repo.snapshot("A.lean", Some(&candidate)).is_err());
        Ok(())
    }

    #[test]
    fn pending_refs_are_scoped_by_workflow_and_canonical_ledger() -> Result<()> {
        let (temp, repo) = bare()?;
        let base = branch(&repo)?;
        let approved = id('f');
        let commit = repo.prepare_adr(&base, &approved, "# Decision\n")?;
        let ledger = temp.path().join("ledger");
        let other = temp.path().join("other");
        File::create(&ledger)?;
        File::create(&other)?;
        let promotion = repo.pending_ref(&ledger)?;
        let pivot = repo.pivot_pending_ref(&ledger)?;
        let second_pivot = repo.pivot_pending_ref(&other)?;
        promotion.ensure_pinned(&base)?;
        pivot.ensure_pinned(&commit)?;
        second_pivot.ensure_pinned(&base)?;
        assert_eq!(
            repo.pivot_pending_ref(&temp.path().join("./ledger"))?
                .current()?
                .as_deref(),
            Some(commit.as_str())
        );
        assert!(pivot.ensure_pinned(&base).is_err());
        assert!(pivot.remove(&base).is_err());
        pivot.clear()?;
        assert!(pivot.current()?.is_none());
        assert_eq!(promotion.current()?.as_deref(), Some(base.as_str()));
        assert_eq!(second_pivot.current()?.as_deref(), Some(base.as_str()));

        // A misconfigured symbolic slot must not alias promotion's ref.
        git(
            repo.path(),
            &["symbolic-ref", &pivot.name, &promotion.name],
            None,
            None,
        )?;
        assert!(pivot.ensure_pinned(&base).is_err());
        assert!(pivot.remove(&base).is_err());
        assert_eq!(promotion.current()?.as_deref(), Some(base.as_str()));
        Ok(())
    }
}
