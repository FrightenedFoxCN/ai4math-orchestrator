#![cfg(unix)]

use ai4math_evidence::WitnessEvidence;
use ai4math_store::{
    Change, NewClaim, RefutationOutboxPhase, RefutationRecord, StateStore, Trust, Wiki, WikiQuery,
    WikiStratum, WitnessInput,
};
use anyhow::{Context, Result, ensure};
use rusqlite::Connection;
use sha2::{Digest, Sha256};
use std::{
    ffi::CString,
    fs,
    os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt, symlink},
    path::PathBuf,
};
#[cfg(target_vendor = "apple")]
use std::{path::Path, process::Command};

const BASE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const SOURCE: &str = "def target : Prop := False\n";
const PROOF: &str = "fun h => h\n";

fn digest(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}

struct Witness {
    base: String,
    path: String,
    proposition: String,
    source: String,
    proof: String,
    source_sha: String,
    proof_sha: String,
    witness_sha: String,
    receipt: String,
}

impl Witness {
    fn new(proof: &str, nonce: u8) -> Self {
        let source_sha = digest(SOURCE);
        let proof_sha = digest(proof);
        let mut witness = Sha256::new();
        witness.update(b"test-witness\0");
        witness.update(BASE.as_bytes());
        witness.update(SOURCE.as_bytes());
        witness.update(proof.as_bytes());
        Self {
            base: BASE.into(),
            path: "Spec.lean".into(),
            proposition: "target".into(),
            source: SOURCE.into(),
            proof: proof.into(),
            source_sha,
            proof_sha,
            witness_sha: format!("{:x}", witness.finalize()),
            receipt: format!(
                r#"{{"nonce":{nonce},"kind":"lean-witness","issued_unix_seconds":1}}"#
            ),
        }
    }
}

// SAFETY: this test-only value models the sealed broker port. It is never used
// by production code to construct or authorize evidence.
unsafe impl WitnessEvidence for Witness {
    fn base_commit(&self) -> &str {
        &self.base
    }

    fn file_path(&self) -> &str {
        &self.path
    }

    fn proposition_id(&self) -> &str {
        &self.proposition
    }

    fn source_sha256(&self) -> &str {
        &self.source_sha
    }

    fn proof_body_sha256(&self) -> &str {
        &self.proof_sha
    }

    fn witness_sha256(&self) -> &str {
        &self.witness_sha
    }

    fn source(&self) -> &str {
        &self.source
    }

    fn matches(
        &self,
        base_commit: &str,
        file_path: &str,
        proposition_id: &str,
        committed_source: &[u8],
        proof_body: &[u8],
    ) -> bool {
        base_commit == self.base
            && file_path == self.path
            && proposition_id == self.proposition
            && committed_source == self.source.as_bytes()
            && proof_body == self.proof.as_bytes()
    }

    fn receipt_json(&self) -> Result<String> {
        Ok(self.receipt.clone())
    }
}

struct Fixture {
    _root: tempfile::TempDir,
    root: PathBuf,
    repo: PathBuf,
    db_path: PathBuf,
    store: StateStore,
    witness: Witness,
}

impl Fixture {
    fn new() -> Result<Self> {
        let temp = tempfile::tempdir()?;
        let root = fs::canonicalize(temp.path())?;
        let repo = root.join("repo");
        fs::create_dir(&repo)?;
        let db_path = root.join(".runtime/state.sqlite");
        let mut store = StateStore::open(&db_path)?;
        store.create_claim(
            NewClaim {
                id: "C",
                title: "Counterexample",
                informal_statement: "target",
                engine: "lean4",
                formal_target: Some("Spec.lean#target"),
                module_path: None,
                critical: true,
            },
            Change {
                actor: "fixture",
                git_commit: Some(BASE),
            },
        )?;
        store.bind_witness_spec(
            "C",
            &ai4math_store::WitnessSpecBinding {
                repo: repo.to_str().context("repo UTF-8")?.into(),
                commit_id: BASE.into(),
                file_path: "Spec.lean".into(),
                proposition_name: "target".into(),
            },
        )?;
        fs::create_dir_all(root.join(".mathwiki/graveyard"))?;
        Ok(Self {
            _root: temp,
            root,
            repo,
            db_path,
            store,
            witness: Witness::new(PROOF, 1),
        })
    }

    fn record(&mut self) -> Result<RefutationRecord> {
        self.store.record_refutation(
            WitnessInput {
                project_root: &self.root,
                repository: &self.repo,
                claim_id: "C",
                committed_source: SOURCE,
                proof_body: &self.witness.proof,
            },
            &self.witness,
        )
    }

    fn graveyard(&self) -> PathBuf {
        self.root.join(".mathwiki/graveyard")
    }

    fn archive(&self, wiki_id: &str) -> PathBuf {
        self.graveyard().join(format!("{wiki_id}.md"))
    }

    fn phase(&self) -> Result<RefutationOutboxPhase> {
        let phase: String = self.db().query_row(
            "SELECT phase FROM refutation_outbox WHERE claim_id='C'",
            [],
            |r| r.get(0),
        )?;
        Ok(match phase.as_str() {
            "PENDING_FILE" => RefutationOutboxPhase::PendingFile,
            "STAGED" => RefutationOutboxPhase::Staged,
            "DB_COMMITTED" => RefutationOutboxPhase::DbCommitted,
            "COMPLETE" => RefutationOutboxPhase::Complete,
            _ => anyhow::bail!("invalid phase {phase}"),
        })
    }

    fn db(&self) -> Connection {
        Connection::open(&self.db_path).expect("open fixture ledger")
    }

    fn count(&self, sql: &str) -> Result<i64> {
        Ok(self.db().query_row(sql, [], |r| r.get(0))?)
    }

    fn witness_events(&self) -> Result<i64> {
        self.count("SELECT count(*) FROM events WHERE event_type='WITNESS_VERIFIED'")
    }

    fn markdown(&self) -> Result<String> {
        Ok(self.db().query_row(
            "SELECT markdown FROM refutation_outbox WHERE claim_id='C'",
            [],
            |r| r.get(0),
        )?)
    }

    fn rebuild_count(&self) -> Result<usize> {
        let mut wiki = Wiki::open(self.root.join(".mathwiki/.index.sqlite"))?;
        wiki.rebuild(self.root.join(".mathwiki"))
    }
}

#[test]
fn staged_markdown_is_hidden_when_final_sql_fails_and_reconcile_is_exactly_once() -> Result<()> {
    let mut fixture = Fixture::new()?;
    fixture.db().execute_batch(
        "CREATE TRIGGER fail_refutation_insert BEFORE INSERT ON verified_refutations
         BEGIN SELECT RAISE(ABORT, 'injected final SQL failure'); END;",
    )?;

    let error = fixture.record().unwrap_err();
    assert!(error.to_string().contains("record refutation"), "{error:#}");
    assert_eq!(fixture.phase()?, RefutationOutboxPhase::Staged);
    assert_eq!(
        fixture.count("SELECT count(*) FROM verified_refutations")?,
        0
    );
    assert_eq!(fixture.witness_events()?, 0);
    assert_eq!(fixture.store.claim("C")?.unwrap().local_trust, Trust::L0);
    assert_eq!(
        fixture.rebuild_count()?,
        0,
        "private stage must not be indexed"
    );
    let pending = fixture
        .store
        .pending_refutation()?
        .context("pending intent")?;
    assert_eq!(pending.phase, RefutationOutboxPhase::Staged);
    assert!(!fixture.archive(&pending.wiki_id).exists());
    assert!(
        fixture
            .root
            .join(".mathwiki/.refutation-outbox")
            .join(format!("{}.pending", pending.wiki_id))
            .is_file()
    );

    fixture
        .db()
        .execute_batch("DROP TRIGGER fail_refutation_insert;")?;
    let first = fixture.store.reconcile_refutation("C")?;
    assert!(!first.already_recorded);
    assert_eq!(fixture.phase()?, RefutationOutboxPhase::Complete);
    assert_eq!(fixture.witness_events()?, 1);
    assert_eq!(fixture.store.claim("C")?.unwrap().local_trust, Trust::L2b);
    assert_eq!(
        fs::read_to_string(fixture.archive(&first.wiki_id))?,
        fixture.markdown()?
    );
    assert_eq!(fixture.rebuild_count()?, 1);

    let seq = fixture.store.status()?.last_event_seq;
    let second = fixture.store.reconcile_refutation("C")?;
    assert!(second.already_recorded);
    assert_eq!(second.event_seq, first.event_seq);
    let report = fixture.store.reconcile_refutations()?;
    assert!(report.failures.is_empty());
    assert_eq!(report.records.len(), 1);
    assert!(report.records[0].already_recorded);
    assert_eq!(fixture.witness_events()?, 1);
    assert_eq!(fixture.store.status()?.last_event_seq, seq);
    assert_eq!(fs::read_dir(fixture.graveyard())?.count(), 1);
    Ok(())
}

#[test]
fn crash_before_intent_commit_leaves_no_outbox_markdown_or_event() -> Result<()> {
    let mut fixture = Fixture::new()?;
    fixture.db().execute_batch(
        "CREATE TRIGGER fail_outbox_insert BEFORE INSERT ON refutation_outbox
         BEGIN SELECT RAISE(ABORT, 'injected prepare crash'); END;",
    )?;
    assert!(fixture.record().is_err());
    assert_eq!(fixture.count("SELECT count(*) FROM refutation_outbox")?, 0);
    assert_eq!(
        fixture.count("SELECT count(*) FROM verified_refutations")?,
        0
    );
    assert_eq!(fixture.witness_events()?, 0);
    assert!(fixture.store.pending_refutation()?.is_none());
    assert_eq!(fs::read_dir(fixture.graveyard())?.count(), 0);
    assert!(!fixture.root.join(".mathwiki/.refutation-outbox").exists());
    Ok(())
}

#[test]
fn truncated_private_stage_resumes_without_becoming_visible() -> Result<()> {
    let mut fixture = Fixture::new()?;
    fs::remove_dir(fixture.graveyard())?;
    assert!(fixture.record().is_err());
    let pending = fixture
        .store
        .pending_refutation()?
        .context("pending intent")?;
    let markdown = fixture.markdown()?;
    fs::create_dir(fixture.graveyard())?;
    let outbox = fixture.root.join(".mathwiki/.refutation-outbox");
    fs::create_dir(&outbox)?;
    fs::set_permissions(&outbox, fs::Permissions::from_mode(0o700))?;
    let stage = outbox.join(format!("{}.pending", pending.wiki_id));
    fs::write(&stage, &markdown[..97])?;
    fs::set_permissions(&stage, fs::Permissions::from_mode(0o600))?;
    assert_eq!(fixture.rebuild_count()?, 0);

    let record = fixture.store.reconcile_refutation("C")?;
    assert!(!record.already_recorded);
    assert_eq!(fixture.phase()?, RefutationOutboxPhase::Complete);
    assert_eq!(
        fs::read_to_string(fixture.archive(&record.wiki_id))?,
        markdown
    );
    assert!(!stage.exists());
    assert_eq!(fixture.witness_events()?, 1);
    Ok(())
}

#[test]
fn ledger_commit_before_publication_failure_is_visible_and_reconciled() -> Result<()> {
    let mut fixture = Fixture::new()?;
    // Determine the deterministic archive ID from a prepared-but-not-staged intent
    // by making staging fail first, then retry with the final path occupied.
    fs::remove_dir(fixture.graveyard())?;
    assert!(fixture.record().is_err());
    let pending = fixture
        .store
        .pending_refutation()?
        .context("pending intent")?;
    assert_eq!(pending.phase, RefutationOutboxPhase::PendingFile);
    assert_eq!(fixture.witness_events()?, 0);
    fs::create_dir(fixture.graveyard())?;
    let archive = fixture.archive(&pending.wiki_id);
    let victim = fixture.root.join("victim.md");
    fs::write(&victim, "operator evidence must not be overwritten")?;
    symlink(&victim, &archive)?;

    let error = fixture.record().unwrap_err();
    let message = format!("{error:#}");
    assert!(
        message.contains("ledger committed; reconcile required"),
        "{message}"
    );
    assert_eq!(fixture.phase()?, RefutationOutboxPhase::DbCommitted);
    assert_eq!(
        fixture.count("SELECT count(*) FROM verified_refutations")?,
        1
    );
    assert_eq!(fixture.witness_events()?, 1);
    assert_eq!(fixture.store.claim("C")?.unwrap().local_trust, Trust::L2b);
    assert!(
        fixture.store.managed_wiki_stamp()?.is_none(),
        "a generation with missing final Markdown must not be cached as current"
    );
    assert_eq!(
        fs::read_to_string(&victim)?,
        "operator evidence must not be overwritten"
    );
    assert!(fs::symlink_metadata(&archive)?.file_type().is_symlink());

    // The operator resolves the conflict without deleting evidence blindly. The
    // DB was already authoritative, so reconciliation only publishes the recipe.
    fs::remove_file(&archive)?;
    let record = fixture.store.reconcile_refutation("C")?;
    assert!(record.already_recorded);
    assert_eq!(fixture.phase()?, RefutationOutboxPhase::Complete);
    assert_eq!(fixture.witness_events()?, 1);
    assert_eq!(
        fixture.store.managed_wiki_stamp()?,
        Some((
            fixture.repo.to_str().context("repo UTF-8")?.into(),
            fixture.store.status()?.last_event_seq
        ))
    );
    assert_eq!(
        fs::read_to_string(fixture.archive(&record.wiki_id))?,
        fixture.markdown()?
    );
    assert_eq!(fixture.rebuild_count()?, 1);
    Ok(())
}

#[test]
fn completed_recipe_repairs_missing_archive_without_duplicate_events() -> Result<()> {
    let mut fixture = Fixture::new()?;
    let record = fixture.record()?;
    let archive = fixture.archive(&record.wiki_id);
    let expected = fixture.markdown()?;
    fs::remove_file(&archive)?;
    let seq = fixture.store.status()?.last_event_seq;

    let repaired = fixture.store.reconcile_refutation("C")?;
    assert!(repaired.already_recorded);
    assert_eq!(repaired.event_seq, record.event_seq);
    assert_eq!(fs::read_to_string(archive)?, expected);
    assert_eq!(fixture.witness_events()?, 1);
    assert_eq!(fixture.store.status()?.last_event_seq, seq);
    assert_eq!(fixture.phase()?, RefutationOutboxPhase::Complete);
    Ok(())
}

#[test]
fn legacy_verified_row_adoption_waits_for_archive_before_complete() -> Result<()> {
    let mut fixture = Fixture::new()?;
    let original = fixture.record()?;
    let seq = fixture.store.status()?.last_event_seq;
    // Simulate a pre-outbox database: authoritative evidence/event remain, while
    // both the archive and its retained recipe are absent.
    fixture
        .db()
        .execute("DELETE FROM refutation_outbox WHERE claim_id='C'", [])?;
    fs::remove_file(fixture.archive(&original.wiki_id))?;
    fs::remove_dir(fixture.graveyard())?;

    let error = fixture.record().unwrap_err();
    assert!(format!("{error:#}").contains("record refutation"));
    assert_eq!(fixture.phase()?, RefutationOutboxPhase::PendingFile);
    let completed: Option<i64> = fixture.db().query_row(
        "SELECT completed_event_seq FROM refutation_outbox WHERE claim_id='C'",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(completed, None);
    assert_eq!(
        fixture.count("SELECT count(*) FROM verified_refutations")?,
        1
    );
    assert_eq!(fixture.witness_events()?, 1);
    assert!(fixture.store.managed_wiki_stamp()?.is_none());

    fs::create_dir(fixture.graveyard())?;
    let adopted = fixture.store.reconcile_refutation("C")?;
    assert!(adopted.already_recorded);
    assert_eq!(adopted.event_seq, original.event_seq);
    assert_eq!(fixture.phase()?, RefutationOutboxPhase::Complete);
    assert_eq!(fixture.witness_events()?, 1);
    assert_eq!(fixture.store.status()?.last_event_seq, seq);
    assert_eq!(
        fs::read_to_string(fixture.archive(&adopted.wiki_id))?,
        fixture.markdown()?
    );
    assert!(fixture.store.managed_wiki_stamp()?.is_some());
    Ok(())
}

#[test]
fn duplicate_and_conflicting_evidence_never_overwrite_the_first_witness() -> Result<()> {
    let mut fixture = Fixture::new()?;
    let first = fixture.record()?;
    let first_bytes = fs::read(fixture.archive(&first.wiki_id))?;
    let seq = fixture.store.status()?.last_event_seq;
    let first_receipt = fixture.witness.receipt.clone();
    fixture.witness.receipt = r#"{"nonce":1,"kind":"lean-witness","issued_unix_seconds":2}"#.into();
    let retry = fixture.record()?;
    assert!(retry.already_recorded);
    assert_eq!(retry.event_seq, first.event_seq);
    let stored: String = fixture.db().query_row(
        "SELECT receipt_json FROM verified_refutations WHERE claim_id='C'",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(stored, first_receipt);

    fixture.witness = Witness::new("fun h => True.intro h\n", 2);
    let error = fixture.record().unwrap_err();
    assert!(
        error
            .to_string()
            .contains("conflicting refutation replay recipe")
    );
    assert_eq!(fs::read(fixture.archive(&first.wiki_id))?, first_bytes);
    assert_eq!(fixture.witness_events()?, 1);
    assert_eq!(fixture.store.status()?.last_event_seq, seq);
    assert_eq!(fixture.phase()?, RefutationOutboxPhase::Complete);
    Ok(())
}

#[test]
fn conflicting_pending_recipe_is_retained_for_the_original_witness() -> Result<()> {
    let mut fixture = Fixture::new()?;
    fixture.db().execute_batch(
        "CREATE TRIGGER fail_refutation_insert BEFORE INSERT ON verified_refutations
         BEGIN SELECT RAISE(ABORT, 'injected final SQL failure'); END;",
    )?;
    assert!(fixture.record().is_err());
    let pending = fixture
        .store
        .pending_refutation()?
        .context("pending intent")?;
    let original_markdown = fixture.markdown()?;
    fixture.witness = Witness::new("fun h => True.intro h\n", 2);
    let error = fixture.record().unwrap_err();
    assert!(
        error
            .to_string()
            .contains("conflicting refutation replay recipe")
    );
    assert_eq!(fixture.markdown()?, original_markdown);
    assert_eq!(
        fixture.store.pending_refutation()?.unwrap().wiki_id,
        pending.wiki_id
    );
    assert!(!fixture.archive(&pending.wiki_id).exists());
    Ok(())
}

#[test]
fn graveyard_replay_rejects_symlink_hardlink_fifo_and_shared_writable_files() -> Result<()> {
    // Symlink: the DB may commit, but publication must fail without touching the target.
    let mut fixture = Fixture::new()?;
    fs::remove_dir(fixture.graveyard())?;
    assert!(fixture.record().is_err());
    let pending = fixture
        .store
        .pending_refutation()?
        .context("pending intent")?;
    fs::create_dir(fixture.graveyard())?;
    let archive = fixture.archive(&pending.wiki_id);
    let victim = fixture.root.join("victim.md");
    fs::write(&victim, "do not overwrite")?;
    symlink(&victim, &archive)?;
    assert!(fixture.record().is_err());
    assert_eq!(fs::read_to_string(&victim)?, "do not overwrite");
    assert!(fs::symlink_metadata(&archive)?.file_type().is_symlink());

    // Hard link: reject nlink=2 before opening, even if the bytes are identical.
    let mut fixture = Fixture::new()?;
    fs::remove_dir(fixture.graveyard())?;
    assert!(fixture.record().is_err());
    let pending = fixture
        .store
        .pending_refutation()?
        .context("pending intent")?;
    fs::create_dir(fixture.graveyard())?;
    let archive = fixture.archive(&pending.wiki_id);
    let victim = fixture.root.join("hardlink-victim.md");
    fs::write(&victim, "do not overwrite")?;
    fs::hard_link(&victim, &archive)?;
    assert!(fixture.record().is_err());
    assert_eq!(fs::metadata(&archive)?.nlink(), 2);
    assert_eq!(fs::read_to_string(&victim)?, "do not overwrite");

    // FIFO: O_NONBLOCK alone is insufficient; the pre-open inode check rejects it.
    let mut fixture = Fixture::new()?;
    fs::remove_dir(fixture.graveyard())?;
    assert!(fixture.record().is_err());
    let pending = fixture
        .store
        .pending_refutation()?
        .context("pending intent")?;
    fs::create_dir(fixture.graveyard())?;
    let archive = fixture.archive(&pending.wiki_id);
    let name = CString::new(archive.to_str().context("FIFO path UTF-8")?)?;
    // SAFETY: name is a live NUL-terminated path and mode is a C unsigned int.
    ensure!(
        unsafe { libc::mkfifo(name.as_ptr(), 0o600 as libc::mode_t) } == 0,
        "create FIFO fixture"
    );
    assert!(fixture.record().is_err());
    assert!(fs::metadata(&archive)?.file_type().is_fifo());

    // A completed archive that later becomes shared-writable is not replayed.
    let mut fixture = Fixture::new()?;
    let record = fixture.record()?;
    let archive = fixture.archive(&record.wiki_id);
    fs::set_permissions(&archive, fs::Permissions::from_mode(0o666))?;
    let report = fixture.store.reconcile_refutations()?;
    assert_eq!(report.failures.len(), 1);
    assert!(
        report.failures[0]
            .error
            .contains("unsafe existing Graveyard archive")
    );
    assert_eq!(fs::metadata(&archive)?.mode() & 0o022, 0o022);
    fs::set_permissions(&archive, fs::Permissions::from_mode(0o600))?;
    assert!(fixture.store.reconcile_refutation("C")?.already_recorded);
    Ok(())
}

#[test]
fn graveyard_directory_symlink_fails_before_authoritative_commit() -> Result<()> {
    let mut fixture = Fixture::new()?;
    fs::remove_dir(fixture.graveyard())?;
    let outside = fixture.root.join("outside");
    fs::create_dir(&outside)?;
    symlink(&outside, fixture.graveyard())?;
    assert!(fixture.record().is_err());
    assert_eq!(fixture.phase()?, RefutationOutboxPhase::PendingFile);
    assert_eq!(
        fixture.count("SELECT count(*) FROM verified_refutations")?,
        0
    );
    assert_eq!(fixture.witness_events()?, 0);
    assert_eq!(fs::read_dir(&outside)?.count(), 0);
    assert!(fixture.graveyard().is_symlink());
    Ok(())
}

#[test]
fn old_schema_is_additively_migrated_only_on_writable_open() -> Result<()> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("state.sqlite");
    let mut store = StateStore::open(&path)?;
    store.create_claim(
        NewClaim {
            id: "old",
            title: "old",
            informal_statement: "old",
            engine: "lean4",
            formal_target: None,
            module_path: None,
            critical: false,
        },
        Change {
            actor: "fixture",
            git_commit: Some(BASE),
        },
    )?;
    drop(store);
    let db = Connection::open(&path)?;
    db.execute_batch(
        "DROP TABLE refutation_outbox;
         DELETE FROM state_migrations WHERE version=3;",
    )?;
    drop(db);

    let readonly = StateStore::open_readonly(&path)?;
    assert!(readonly.pending_refutation()?.is_none());
    let check = Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let table: i64 = check.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='refutation_outbox'",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(table, 0);
    drop(check);
    drop(readonly);

    let _store = StateStore::open(&path)?;
    let check = Connection::open(&path)?;
    let table: i64 = check.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='refutation_outbox'",
        [],
        |r| r.get(0),
    )?;
    let migration: i64 = check.query_row(
        "SELECT count(*) FROM state_migrations WHERE version=3",
        [],
        |r| r.get(0),
    )?;
    assert_eq!((table, migration), (1, 1));
    Ok(())
}

#[cfg(target_vendor = "apple")]
#[test]
fn macos_extended_acl_cannot_bypass_private_mode_checks() -> Result<()> {
    fn add_mutation_acl(path: &Path) -> Result<()> {
        let status = Command::new("chmod")
            .arg("+a")
            .arg("everyone allow write,delete")
            .arg(path)
            .status()?;
        ensure!(status.success(), "install macOS ACL fixture");
        Ok(())
    }

    // A directory ACL can authorize shared mutation while st_mode remains 0700.
    let mut fixture = Fixture::new()?;
    add_mutation_acl(&fixture.graveyard())?;
    assert_eq!(fs::metadata(fixture.graveyard())?.mode() & 0o022, 0);
    assert!(fixture.record().is_err());
    assert_eq!(fixture.phase()?, RefutationOutboxPhase::PendingFile);
    assert_eq!(
        fixture.count("SELECT count(*) FROM verified_refutations")?,
        0
    );
    assert_eq!(fixture.witness_events()?, 0);

    // The same bypass on a completed archive is rejected on replay.
    let mut fixture = Fixture::new()?;
    let record = fixture.record()?;
    let archive = fixture.archive(&record.wiki_id);
    let expected = fs::read(&archive)?;
    add_mutation_acl(&archive)?;
    assert_eq!(fs::metadata(&archive)?.mode() & 0o022, 0);
    let error = fixture.store.reconcile_refutation("C").unwrap_err();
    assert!(format!("{error:#}").contains("extended ACL grants shared"));
    assert_eq!(fs::read(&archive)?, expected);
    Ok(())
}

#[test]
fn normal_success_keeps_record_and_event_abi() -> Result<()> {
    let mut fixture = Fixture::new()?;
    let record = fixture.record()?;
    assert!(!record.already_recorded);
    assert_eq!(record.witness_sha256, fixture.witness.witness_sha);
    assert!(record.wiki_id.starts_with("GRV-"));
    assert_eq!(record.event_seq, fixture.witness_events_sequence()?);
    let event = fixture
        .store
        .events_after(0, 100)?
        .into_iter()
        .find(|event| event.event_seq == record.event_seq)
        .context("WITNESS_VERIFIED event")?;
    assert_eq!(event.event_type, "WITNESS_VERIFIED");
    assert_eq!(event.entity_type, "claim");
    assert_eq!(event.entity_id, "C");
    assert_eq!(event.git_commit.as_deref(), Some(BASE));
    assert_eq!(event.actor, "broker:witness");
    assert_eq!(event.payload["wiki_id"], record.wiki_id);
    assert_eq!(event.payload["witness_sha256"], record.witness_sha256);
    assert_eq!(event.payload["file_path"], "Spec.lean");
    assert_eq!(event.payload["target_decl"], "target");
    assert_eq!(fixture.phase()?, RefutationOutboxPhase::Complete);
    assert_eq!(fixture.rebuild_count()?, 1);
    let warnings = Wiki::open(fixture.root.join(".mathwiki/.index.sqlite"))?.search(WikiQuery {
        scope: WikiStratum::Graveyard,
        ..WikiQuery::default()
    })?;
    assert_eq!(warnings.hits.len(), 1);
    assert_eq!(warnings.hits[0].summary.id, record.wiki_id);
    Ok(())
}

impl Fixture {
    fn witness_events_sequence(&self) -> Result<i64> {
        Ok(self.db().query_row(
            "SELECT event_seq FROM events WHERE event_type='WITNESS_VERIFIED'",
            [],
            |r| r.get(0),
        )?)
    }
}
