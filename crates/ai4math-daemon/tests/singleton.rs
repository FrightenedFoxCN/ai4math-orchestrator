#![cfg(unix)]

use ai4math_store::{Change, StateStore, Wiki};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OpenFlags, types::ValueRef};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::{
        fs::{MetadataExt, PermissionsExt},
        net::UnixStream,
    },
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

const WAIT: Duration = Duration::from_secs(10);
const SOCKET: &str = ".runtime/ai4mathd.sock";
const STATE: &str = ".runtime/state.sqlite";
const INDEX: &str = ".mathwiki/.index.sqlite";

struct Daemon {
    child: Child,
    log: tempfile::NamedTempFile,
}

impl Daemon {
    fn spawn(root: &Path) -> Result<Self> {
        let log = tempfile::NamedTempFile::new_in(root)?;
        let child = Command::new(env!("CARGO_BIN_EXE_ai4mathd"))
            .arg("--project")
            .arg(root)
            .args(["--tick-ms", "5000"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log.reopen()?)
            .spawn()?;
        Ok(Self { child, log })
    }

    fn logs(&self) -> String {
        fs::read_to_string(self.log.path()).unwrap_or_default()
    }

    fn connect(&mut self, root: &Path) -> Result<UnixStream> {
        let socket = root.join(SOCKET);
        let deadline = Instant::now() + WAIT;
        loop {
            match UnixStream::connect(&socket) {
                Ok(stream) => {
                    stream.set_read_timeout(Some(WAIT))?;
                    stream.set_write_timeout(Some(WAIT))?;
                    return Ok(stream);
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                    ) => {}
                Err(error) => return Err(error.into()),
            }
            ensure!(
                self.child.try_wait()?.is_none(),
                "daemon exited before its socket was ready: {}; {}",
                self.logs(),
                self.child.id()
            );
            ensure!(
                Instant::now() < deadline,
                "daemon socket not ready: {}",
                self.logs()
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn wait_for_exit(&mut self) -> Result<ExitStatus> {
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(status) = self.child.try_wait()? {
                return Ok(status);
            }
            ensure!(
                Instant::now() < deadline,
                "daemon did not exit: {}",
                self.logs()
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn signal(&self, signal: i32) -> Result<()> {
        // SAFETY: the positive PID belongs to the Child spawned by this fixture.
        ensure!(
            unsafe { libc::kill(self.child.id() as libc::pid_t, signal) } == 0,
            "signal daemon {}: {}",
            self.child.id(),
            std::io::Error::last_os_error()
        );
        Ok(())
    }

    fn pause(&self) -> Result<()> {
        self.signal(libc::SIGSTOP)?;
        let deadline = Instant::now() + WAIT;
        loop {
            let mut status = 0;
            // SAFETY: status is a live output value and the PID is our Child.
            let observed = unsafe {
                libc::waitpid(
                    self.child.id() as libc::pid_t,
                    &mut status,
                    libc::WUNTRACED | libc::WNOHANG,
                )
            };
            ensure!(
                observed != -1,
                "observe stopped daemon: {}",
                std::io::Error::last_os_error()
            );
            if observed == self.child.id() as libc::pid_t {
                ensure!(
                    libc::WIFSTOPPED(status),
                    "daemon exited instead of stopping: {}; {}",
                    self.logs(),
                    self.child.id()
                );
                return Ok(());
            }
            ensure!(
                Instant::now() < deadline,
                "daemon did not stop: {}",
                self.logs()
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn resume(&self) -> Result<()> {
        self.signal(libc::SIGCONT)
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        // Always release a fixture pause before reaping it, including on panic.
        let _ = self.signal(libc::SIGCONT);
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn call(stream: &mut UnixStream, id: u64, method: &str, params: Value) -> Result<Value> {
    writeln!(
        stream,
        "{}",
        json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
    )?;
    let mut line = String::new();
    ensure!(
        BufReader::new(stream.try_clone()?).read_line(&mut line)? > 0,
        "EOF during {method}"
    );
    let response: Value = serde_json::from_str(&line)?;
    assert_eq!(response["id"], id, "{method}: {response}");
    Ok(response)
}

fn wait_for_first_tick(daemon: &mut Daemon, root: &Path) -> Result<Value> {
    let mut reader = BufReader::new(daemon.connect(root)?);
    writeln!(
        reader.get_mut(),
        "{}",
        json!({"jsonrpc":"2.0","id":1,"method":"subscribe","params":{"topics":["vital_signs.tick"]}})
    )?;
    let mut line = String::new();
    ensure!(reader.read_line(&mut line)? > 0, "EOF during subscribe");
    let subscribed: Value = serde_json::from_str(&line)?;
    ensure!(
        subscribed.get("result").is_some(),
        "subscribe failed: {subscribed}"
    );
    let deadline = Instant::now() + WAIT;
    loop {
        line.clear();
        ensure!(
            reader.read_line(&mut line)? > 0,
            "daemon closed before its first telemetry tick"
        );
        let frame: Value = serde_json::from_str(&line)?;
        if frame["method"] == "vital_signs.tick" {
            return Ok(frame["params"].clone());
        }
        ensure!(
            Instant::now() < deadline,
            "first telemetry tick not received"
        );
    }
}

fn readonly(path: &Path) -> Result<Connection> {
    let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    db.busy_timeout(WAIT)?;
    Ok(db)
}

fn query_rows(db: &Connection, sql: &str) -> Result<Vec<Vec<Value>>> {
    let mut statement = db.prepare(sql)?;
    let columns = statement.column_count();
    let mut rows = Vec::new();
    let mut iter = statement.query([])?;
    while let Some(row) = iter.next()? {
        let mut values = Vec::with_capacity(columns);
        for column in 0..columns {
            values.push(match row.get_ref(column)? {
                ValueRef::Null => Value::Null,
                ValueRef::Integer(value) => json!(value),
                ValueRef::Real(value) => json!(value),
                ValueRef::Text(value) => {
                    json!(std::str::from_utf8(value).context("non-UTF-8 SQLite text")?)
                }
                ValueRef::Blob(value) => json!(value),
            });
        }
        rows.push(values);
    }
    Ok(rows)
}

const SCHEMA: &str = "SELECT type,name,tbl_name,sql FROM sqlite_master
    WHERE name NOT GLOB 'sqlite_*' ORDER BY type,name";

#[derive(Debug, PartialEq, Eq)]
struct LogicalSnapshot {
    state_schema: Vec<Vec<Value>>,
    state_events: Vec<Vec<Value>>,
    state_migrations: Vec<Vec<Value>>,
    wiki_schema: Vec<Vec<Value>>,
    wiki_entries: Vec<Vec<Value>>,
}

fn logical_snapshot(root: &Path) -> Result<LogicalSnapshot> {
    let state = readonly(&root.join(STATE))?;
    let wiki = readonly(&root.join(INDEX))?;
    Ok(LogicalSnapshot {
        state_schema: query_rows(&state, SCHEMA)?,
        state_events: query_rows(
            &state,
            "SELECT event_seq,occurred_at,event_type,entity_type,entity_id,git_commit,actor,payload_json
             FROM events ORDER BY event_seq",
        )?,
        state_migrations: query_rows(&state, "SELECT version FROM state_migrations ORDER BY version")?,
        wiki_schema: query_rows(&wiki, SCHEMA)?,
        wiki_entries: query_rows(
            &wiki,
            "SELECT id,stratum,title,file_path FROM entries ORDER BY id",
        )?,
    })
}

#[derive(Debug, PartialEq, Eq)]
struct FileSnapshot {
    bytes: Option<Vec<u8>>,
    device: u64,
    inode: u64,
    links: u64,
    mode: u32,
    len: u64,
    modified_ns: i64,
    changed_ns: i64,
}

fn database_paths(root: &Path) -> Vec<PathBuf> {
    [root.join(STATE), root.join(INDEX)]
        .into_iter()
        .flat_map(|path| {
            ["", "-wal", "-shm", "-journal"]
                .into_iter()
                .map(move |suffix| {
                    let mut sidecar = path.clone().into_os_string();
                    sidecar.push(suffix);
                    PathBuf::from(sidecar)
                })
        })
        .collect()
}

fn capture_files(paths: &[PathBuf]) -> Result<BTreeMap<PathBuf, FileSnapshot>> {
    let mut snapshots = BTreeMap::new();
    for path in paths {
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error).with_context(|| format!("inspect {}", path.display())),
        };
        ensure!(
            metadata.is_file(),
            "database sidecar is not a file: {}",
            path.display()
        );
        snapshots.insert(
            path.clone(),
            FileSnapshot {
                bytes: Some(fs::read(path)?),
                device: metadata.dev(),
                inode: metadata.ino(),
                links: metadata.nlink(),
                mode: metadata.mode(),
                len: metadata.len(),
                modified_ns: metadata.mtime_nsec(),
                changed_ns: metadata.ctime_nsec(),
            },
        );
    }
    Ok(snapshots)
}

fn make_database_open_observably_mutating(root: &Path) -> Result<()> {
    // A pre-upgrade marker makes StateStore::open perform migration 2. The
    // missing Wiki index makes Wiki::open execute DDL. A duplicate that opens
    // either database before acquiring the directory lease changes these files.
    let state = Connection::open(root.join(STATE))?;
    state.busy_timeout(WAIT)?;
    assert_eq!(
        state.execute("DELETE FROM state_migrations WHERE version=2", [])?,
        1,
        "fixture must start with migration 2 installed"
    );
    drop(state);

    let wiki = Connection::open(root.join(INDEX))?;
    wiki.busy_timeout(WAIT)?;
    wiki.execute("DROP INDEX idx_entries_status_ref", [])?;
    let remaining: i64 = wiki.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type='index' AND name='idx_entries_status_ref'",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(
        remaining, 0,
        "fixture must have a missing Wiki schema object"
    );
    Ok(())
}

#[test]
fn second_instance_fails_before_opening_or_migrating_databases() -> Result<()> {
    // Keep the canonical endpoint below the macOS Unix socket path limit.
    let temp = tempfile::Builder::new()
        .prefix("a4")
        .rand_bytes(4)
        .tempdir()?;
    let root = temp.path().canonicalize()?;
    let wiki_root = root.join(".mathwiki");
    fs::create_dir_all(wiki_root.join("core"))?;
    fs::set_permissions(&wiki_root, fs::Permissions::from_mode(0o700))?;
    fs::write(
        wiki_root.join("core/COR-SINGLETON.md"),
        "---\nid: COR-SINGLETON\nstratum: core\ntrust_level: L5\nengine: lean4\nstatus: active\ntitle: Singleton fixture\n---\n",
    )?;

    let event_seq = {
        let mut state = StateStore::open(root.join(STATE))?;
        state.append_event(
            Change {
                actor: "singleton-fixture",
                git_commit: None,
            },
            "SINGLETON_FIXTURE",
            "project",
            "singleton",
            json!({"kept":true}),
        )?
    };
    assert_eq!(Wiki::open(root.join(INDEX))?.rebuild(&wiki_root)?, 1);

    let mut first = Daemon::spawn(&root)?;
    let first_tick = wait_for_first_tick(&mut first, &root)?;
    assert_eq!(first_tick["event_seq"], event_seq, "{first_tick}");
    let runtime = root.join(".runtime");
    let socket = root.join(SOCKET);
    assert_eq!(fs::metadata(&runtime)?.mode() & 0o7777, 0o700);
    assert_eq!(fs::symlink_metadata(&socket)?.mode() & 0o7777, 0o600);
    let original_socket = fs::symlink_metadata(&socket)?;

    make_database_open_observably_mutating(&root)?;
    // Freeze the owner after its startup tick so its own telemetry cannot
    // explain any byte, timestamp, event, or schema difference below.
    first.pause()?;
    let paths = database_paths(&root);
    let logical_before = logical_snapshot(&root)?;
    assert_eq!(logical_before.state_events.len(), 1);
    assert!(
        logical_before
            .state_migrations
            .iter()
            .any(|row| row[0] == 1),
        "fixture must retain the installed route projection migration"
    );
    assert!(
        logical_before
            .state_migrations
            .iter()
            .all(|row| row[0] != 2),
        "fixture must have a pending migration"
    );
    ensure!(
        logical_before
            .wiki_schema
            .iter()
            .all(|row| row[1] != "idx_entries_status_ref"),
        "fixture must have a missing Wiki schema object"
    );
    let files_before = capture_files(&paths)?;
    ensure!(files_before.contains_key(&root.join(STATE)));
    ensure!(files_before.contains_key(&root.join(format!("{STATE}-wal"))));
    ensure!(files_before.contains_key(&root.join(format!("{STATE}-shm"))));
    ensure!(files_before.contains_key(&root.join(INDEX)));

    let mut second = Daemon::spawn(&root)?;
    let status = second.wait_for_exit()?;
    ensure!(
        !status.success(),
        "second daemon unexpectedly stayed running"
    );
    let diagnostic = second.logs();
    ensure!(
        diagnostic.contains("runtime directory is locked by another daemon"),
        "second daemon failed for the wrong reason: {diagnostic}"
    );

    let files_after = capture_files(&paths)?;
    ensure!(
        files_before == files_after,
        "rejected second instance changed StateStore/Wiki files or sidecars"
    );
    let logical_after = logical_snapshot(&root)?;
    ensure!(
        logical_before == logical_after,
        "rejected second instance changed events or schema"
    );
    let surviving_socket = fs::symlink_metadata(&socket)?;
    assert_eq!(original_socket.dev(), surviving_socket.dev());
    assert_eq!(original_socket.ino(), surviving_socket.ino());
    assert_eq!(original_socket.mode() & 0o7777, 0o600);

    first.resume()?;
    ensure!(
        first.child.try_wait()?.is_none(),
        "first daemon was displaced"
    );
    let mut stream = first.connect(&root)?;
    let status = call(&mut stream, 2, "status", json!({}))?;
    assert_eq!(
        status["result"]["event_seq"], event_seq,
        "first daemon stopped serving after the rejected instance: {status}"
    );
    Ok(())
}

#[test]
fn rejected_instance_does_not_create_wiki_or_briefings() -> Result<()> {
    let temp = tempfile::Builder::new()
        .prefix("a4")
        .rand_bytes(4)
        .tempdir()?;
    let root = temp.path().canonicalize()?;
    let mut first = Daemon::spawn(&root)?;
    drop(first.connect(&root)?);

    let wiki = root.join(".mathwiki");
    let briefings = root.join(".runtime/briefings");
    assert!(wiki.is_dir());
    assert!(briefings.is_dir());
    // The owner already has both databases open. Remove only the directories a
    // losing instance must not recreate before it observes the flock failure.
    fs::remove_dir_all(&wiki)?;
    fs::remove_dir_all(&briefings)?;
    first.pause()?;

    let mut second = Daemon::spawn(&root)?;
    let status = second.wait_for_exit()?;
    ensure!(
        !status.success(),
        "second daemon unexpectedly stayed running"
    );
    let diagnostic = second.logs();
    ensure!(
        diagnostic.contains("runtime directory is locked by another daemon"),
        "second daemon failed for the wrong reason: {diagnostic}"
    );
    assert!(!wiki.exists(), "rejected instance recreated .mathwiki");
    assert!(
        !briefings.exists(),
        "rejected instance recreated .runtime/briefings"
    );

    first.resume()?;
    ensure!(
        first.child.try_wait()?.is_none(),
        "first daemon was displaced"
    );
    let mut stream = first.connect(&root)?;
    let status = call(&mut stream, 1, "status", json!({}))?;
    assert_eq!(status["result"]["event_seq"], 0, "{status}");
    Ok(())
}
