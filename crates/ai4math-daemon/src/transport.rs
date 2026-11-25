//! Local JSON-RPC transport: one UTF-8 JSON object per newline (no batches).
//! Only the writer thread touches a connection's outbound byte stream.

use serde_json::{Map, Value, json};
use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::fs::{self, File, Metadata, OpenOptions, Permissions};
use std::io::{self, BufRead, BufReader, Write};
use std::net::Shutdown;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub const MAX_FRAME_BYTES: usize = 1024 * 1024;
const MAX_CONNECTIONS: usize = 16;
const QUEUE_DEPTH: usize = 4;
const MAX_TOPICS: usize = 32;
const FRAME_READ_DEADLINE: Duration = Duration::from_secs(10);
const READ_CHECK_INTERVAL: Duration = Duration::from_secs(1);
const FRAME_WRITE_DEADLINE: Duration = Duration::from_secs(2);

type Frame = Arc<[u8]>;

#[derive(Debug)]
pub struct RpcError {
    code: i64,
    message: String,
    data: Option<Value>,
}

impl RpcError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    #[cfg(test)]
    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }
}

/// Subscription view for this connection, scoped to one handler invocation.
/// Changes commit atomically after a successful reply is queued (or after a
/// successful client notification). Failed handlers do not change subscriptions.
pub struct Connection {
    topics: RefCell<BTreeSet<String>>,
}

impl Connection {
    pub fn subscribe(&self, topic: &str) -> Result<bool, RpcError> {
        if topic.is_empty() || topic.len() > 128 {
            return Err(RpcError::new(-32602, "Invalid subscription topic"));
        }
        let mut topics = self.topics.borrow_mut();
        if topics.contains(topic) {
            return Ok(false);
        }
        if topics.len() >= MAX_TOPICS {
            return Err(RpcError::new(-32602, "Subscription limit exceeded"));
        }
        Ok(topics.insert(topic.to_owned()))
    }

    pub fn unsubscribe(&self, topic: &str) -> bool {
        self.topics.borrow_mut().remove(topic)
    }
}

struct Peer {
    topics: BTreeSet<String>,
    outgoing: SyncSender<Frame>,
    control: UnixStream,
}

#[derive(Default)]
struct Registry {
    next_id: u64,
    peers: HashMap<u64, Peer>,
}

/// Shared by the server and publishers (e.g. the background ticker).
#[derive(Clone, Default)]
pub struct Hub {
    registry: Arc<Mutex<Registry>>,
}

impl Hub {
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue one JSON-RPC notification for matching subscribers without waiting
    /// on sockets. Returns the number of successful queues; a full queue closes
    /// that connection rather than blocking the ticker or dropping unseen data.
    pub fn publish(&self, topic: &str, method: &str, params: Value) -> usize {
        if method.is_empty() || !params.is_object() {
            return 0;
        }
        let Ok(frame) = encode_frame(&json!({"jsonrpc":"2.0", "method":method, "params":params}))
        else {
            return 0;
        };
        let mut registry = self.registry.lock().unwrap();
        let mut queued = 0;
        registry.peers.retain(|_, peer| {
            if !peer.topics.contains(topic) {
                return true;
            }
            if peer.outgoing.try_send(frame.clone()).is_ok() {
                queued += 1;
                true
            } else {
                let _ = peer.control.shutdown(Shutdown::Both);
                false
            }
        });
        queued
    }

    fn register(&self, outgoing: SyncSender<Frame>, control: UnixStream) -> u64 {
        let mut registry = self.registry.lock().unwrap();
        let id = registry.next_id;
        registry.next_id = registry
            .next_id
            .checked_add(1)
            .expect("connection ID exhausted");
        registry.peers.insert(
            id,
            Peer {
                topics: BTreeSet::new(),
                outgoing,
                control,
            },
        );
        id
    }

    fn unregister(&self, id: u64) {
        self.registry.lock().unwrap().peers.remove(&id);
    }

    fn complete(&self, id: u64, reply: Option<Frame>, topics: Option<&BTreeSet<String>>) -> bool {
        let mut registry = self.registry.lock().unwrap();
        let Some(peer) = registry.peers.get_mut(&id) else {
            return false;
        };
        // The shared registry lock serializes publishing with the response and
        // subscription update. The FIFO writer preserves that order on the wire.
        if let Some(reply) = reply
            && peer.outgoing.try_send(reply).is_err()
        {
            let _ = peer.control.shutdown(Shutdown::Both);
            registry.peers.remove(&id);
            return false;
        }
        if let Some(topics) = topics {
            peer.topics.clone_from(topics);
        }
        true
    }
}

/// Exclusive lease on a private runtime directory. Acquire this before opening
/// or migrating any databases, and keep it through bind/listen and serving.
/// Closing the lease (or exiting the process) releases the nonblocking flock.
pub struct SingletonLease {
    _directory: File,
    metadata: Metadata,
    owner: u32,
}

/// Lock a pre-existing, private (0700), effective-user-owned runtime directory.
/// Cooperating instances cannot race database startup: a second instance gets
/// `AddrInUse` before it can open or migrate the StateStore or Wiki index.
pub fn acquire_singleton(runtime: &Path) -> io::Result<SingletonLease> {
    let absolute = std::path::absolute(runtime)?;
    // SAFETY: geteuid has no arguments or memory requirements.
    let owner = unsafe { libc::geteuid() };
    validate_ancestors(&absolute, owner)?;
    let real_runtime = fs::canonicalize(&absolute)?;
    validate_ancestors(&real_runtime, owner)?;
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
        .open(&real_runtime)?;
    let metadata = directory.metadata()?;
    if !same_inode(&metadata, &fs::symlink_metadata(&real_runtime)?)
        || metadata.uid() != owner
        || metadata.mode() & 0o7777 != 0o700
    {
        return Err(path_error(
            io::ErrorKind::PermissionDenied,
            &real_runtime,
            "runtime directory changed during startup",
        ));
    }
    // SAFETY: flock receives the valid directory FD held by the lease. Closing
    // that FD releases the lock, including on every startup error path.
    loop {
        if unsafe { libc::flock(directory.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            break;
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            if error.kind() == io::ErrorKind::WouldBlock {
                return Err(path_error(
                    io::ErrorKind::AddrInUse,
                    &real_runtime,
                    "runtime directory is locked by another daemon",
                ));
            }
            return Err(path_error(error.kind(), &real_runtime, error));
        }
    }
    Ok(SingletonLease {
        _directory: directory,
        metadata,
        owner,
    })
}

/// Listen using a runtime lease acquired before database startup. Accepts only
/// peers with the same effective UID. The endpoint must not already exist:
/// after a crash, confirm the daemon is stopped before removing a stale socket
/// manually. The lease is held for the server's lifetime.
pub fn serve(
    socket_path: &Path,
    lease: SingletonLease,
    hub: Hub,
    ready: impl FnOnce() -> io::Result<()>,
    handler: impl Fn(&Connection, &str, Map<String, Value>) -> Result<Value, RpcError>
    + Send
    + Sync
    + 'static,
) -> io::Result<()> {
    let socket = bind_with_lease(lease, socket_path)?;
    ready()?;
    let handler = Arc::new(handler);
    let active = Arc::new(AtomicUsize::new(0));
    loop {
        let stream = match socket.listener.accept() {
            Ok((stream, _)) => stream,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        if !peer_is_owner(&stream, socket.owner) {
            continue;
        }
        if active.load(Ordering::Relaxed) >= MAX_CONNECTIONS {
            // Do not spawn threads or queue work for excess connections.
            continue;
        }
        // The accept loop is the only incrementer; workers only release permits.
        active.fetch_add(1, Ordering::Relaxed);
        let permit = Permit(active.clone());
        let hub = hub.clone();
        let handler = handler.clone();
        // If spawn fails, the closure and its connection permit are dropped.
        let _ = thread::Builder::new()
            .name("ai4math-rpc-reader".into())
            .spawn(move || {
                let _permit = permit;
                let _ = handle_connection(stream, hub, &*handler);
            });
    }
}

struct Permit(Arc<AtomicUsize>);

impl Drop for Permit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

struct Session {
    hub: Hub,
    id: u64,
    writer: Option<JoinHandle<()>>,
}

impl Drop for Session {
    fn drop(&mut self) {
        // Closing the sender lets the writer flush any queued replies after a
        // client half-closes its write side. This also runs on handler panic.
        self.hub.unregister(self.id);
        if let Some(writer) = self.writer.take() {
            let _ = writer.join();
        }
    }
}

fn handle_connection(
    stream: UnixStream,
    hub: Hub,
    handler: &impl Fn(&Connection, &str, Map<String, Value>) -> Result<Value, RpcError>,
) -> io::Result<()> {
    stream.set_read_timeout(Some(READ_CHECK_INTERVAL))?;
    let control = stream.try_clone()?;
    let writer_stream = stream.try_clone()?;
    let (sender, receiver) = mpsc::sync_channel(QUEUE_DEPTH);
    let writer = thread::Builder::new()
        .name("ai4math-rpc-writer".into())
        .spawn(move || writer_loop(writer_stream, receiver))?;
    let session = Session {
        id: hub.register(sender, control),
        hub,
        writer: Some(writer),
    };
    let mut reader = BufReader::new(stream);
    let mut frame = Vec::new();
    let mut topics = BTreeSet::new();
    while read_frame(&mut reader, &mut frame)? {
        let connection = Connection {
            topics: RefCell::new(topics.clone()),
        };
        let (reply, success) = dispatch(&frame, &connection, handler);
        if success {
            topics = connection.topics.into_inner();
        }
        if !session
            .hub
            .complete(session.id, reply, success.then_some(&topics))
        {
            break;
        }
    }
    Ok(())
}

fn read_frame(reader: &mut impl BufRead, frame: &mut Vec<u8>) -> io::Result<bool> {
    frame.clear();
    let mut started: Option<Instant> = None;
    loop {
        if started.is_some_and(|time| time.elapsed() >= FRAME_READ_DEADLINE) {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "incomplete RPC frame",
            ));
        }
        let available = match reader.fill_buf() {
            Ok([]) => return Ok(false), // Never dispatch an unterminated final frame.
            Ok(bytes) => bytes,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) =>
            {
                continue;
            }
            Err(error) => return Err(error),
        };
        let start = started.get_or_insert_with(Instant::now);
        if start.elapsed() >= FRAME_READ_DEADLINE {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "incomplete RPC frame",
            ));
        }
        let newline = available.iter().position(|&byte| byte == b'\n');
        let len = newline.unwrap_or(available.len());
        let last = if len == 0 {
            frame.last()
        } else {
            available.get(len - 1)
        };
        // An optional CR before the newline does not count toward the JSON limit.
        let allowed = MAX_FRAME_BYTES + usize::from(last == Some(&b'\r'));
        if frame.len() + len > allowed {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "RPC frame exceeds 1 MiB",
            ));
        }
        frame.extend_from_slice(&available[..len]);
        reader.consume(len + usize::from(newline.is_some()));
        if newline.is_some() {
            if frame.last() == Some(&b'\r') {
                frame.pop();
            }
            return Ok(true);
        }
    }
}

fn writer_loop(mut stream: UnixStream, receiver: Receiver<Frame>) {
    for frame in receiver {
        if write_frame(&mut stream, &frame).is_err() {
            break;
        }
    }
    let _ = stream.shutdown(Shutdown::Both);
}

fn write_frame(stream: &mut UnixStream, mut bytes: &[u8]) -> io::Result<()> {
    let deadline = Instant::now() + FRAME_WRITE_DEADLINE;
    while !bytes.is_empty() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "RPC client not reading",
            ));
        }
        stream.set_write_timeout(Some(remaining))?;
        match stream.write(bytes) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(written) => bytes = &bytes[written..],
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn encode_frame(value: &Value) -> io::Result<Frame> {
    struct LimitedFrame(Vec<u8>);
    impl Write for LimitedFrame {
        fn write(&mut self, data: &[u8]) -> io::Result<usize> {
            if data.len() > MAX_FRAME_BYTES - self.0.len() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "RPC output exceeds 1 MiB",
                ));
            }
            self.0.extend_from_slice(data);
            Ok(data.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut output = LimitedFrame(Vec::new());
    serde_json::to_writer(&mut output, value)?;
    output.0.push(b'\n');
    Ok(output.0.into())
}

fn error_response(id: &Value, error: RpcError) -> Value {
    let mut body = json!({"code":error.code, "message":error.message});
    if let Some(data) = error.data {
        body["data"] = data;
    }
    json!({"jsonrpc":"2.0", "id":id, "error":body})
}

fn dispatch(
    frame: &[u8],
    connection: &Connection,
    handler: &impl Fn(&Connection, &str, Map<String, Value>) -> Result<Value, RpcError>,
) -> (Option<Frame>, bool) {
    let failure = |code, message| {
        (
            encode_frame(&error_response(&Value::Null, RpcError::new(code, message))).ok(),
            false,
        )
    };
    let mut request = match serde_json::from_slice::<Value>(frame) {
        Ok(Value::Object(request)) => request,
        Ok(_) => return failure(-32600, "Invalid Request"),
        Err(_) => return failure(-32700, "Parse error"),
    };
    if request.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return failure(-32600, "Invalid Request");
    }
    let Some(Value::String(method)) = request.remove("method") else {
        return failure(-32600, "Invalid Request");
    };
    let id = request.remove("id");
    if id
        .as_ref()
        .is_some_and(|id| !matches!(id, Value::Null | Value::String(_) | Value::Number(_)))
    {
        return failure(-32600, "Invalid Request");
    }
    if id
        .as_ref()
        .and_then(Value::as_str)
        .is_some_and(|id| id.len() > 1024)
    {
        return failure(-32600, "Request ID is too long");
    }
    let result = match request.remove("params") {
        None => handler(connection, &method, Map::new()),
        Some(Value::Object(params)) => handler(connection, &method, params),
        Some(_) => Err(RpcError::new(-32602, "Expected named object params")),
    };
    let success = result.is_ok();
    let Some(id) = id else {
        return (None, success);
    };
    let response = match result {
        Ok(result) => json!({"jsonrpc":"2.0", "id":&id, "result":result}),
        Err(error) => error_response(&id, error),
    };
    if let Ok(reply) = encode_frame(&response) {
        return (Some(reply), success);
    }
    let oversized = RpcError::new(-32603, "Response exceeds frame limit");
    let reply = encode_frame(&error_response(&id, oversized)).ok();
    if reply.is_none() {
        return failure(-32600, "Request ID exceeds response frame limit");
    }
    (reply, false)
}

struct SocketPath {
    path: PathBuf,
    device: u64,
    inode: u64,
}

impl SocketPath {
    fn matches(&self, metadata: &Metadata) -> bool {
        metadata.file_type().is_socket()
            && metadata.dev() == self.device
            && metadata.ino() == self.inode
            && metadata.nlink() == 1
    }
}

impl Drop for SocketPath {
    fn drop(&mut self) {
        if fs::symlink_metadata(&self.path).is_ok_and(|metadata| self.matches(&metadata)) {
            let _ = fs::remove_file(&self.path);
        }
    }
}

pub(crate) struct BoundSocket {
    // Unlink while our inode is still live, then close the socket and unlock.
    _path: SocketPath,
    listener: UnixListener,
    _lease: SingletonLease,
    owner: u32,
}

fn path_error(kind: io::ErrorKind, path: &Path, message: impl std::fmt::Display) -> io::Error {
    io::Error::new(kind, format!("{}: {message}", path.display()))
}

fn same_inode(left: &Metadata, right: &Metadata) -> bool {
    left.dev() == right.dev() && left.ino() == right.ino()
}

fn validate_ancestors(parent: &Path, uid: u32) -> io::Result<()> {
    for dir in parent.ancestors() {
        let meta =
            fs::symlink_metadata(dir).map_err(|error| path_error(error.kind(), dir, error))?;
        // Darwin's /tmp and /var are system-owned aliases of /private/tmp and
        // /private/var. Only allow them as ancestors, not as the runtime dir.
        if cfg!(target_os = "macos")
            && dir != parent
            && (dir == Path::new("/var") || dir == Path::new("/tmp"))
            && meta.file_type().is_symlink()
            && meta.uid() == 0
        {
            continue;
        }
        if !meta.is_dir() || (meta.uid() != uid && meta.uid() != 0) {
            return Err(path_error(
                io::ErrorKind::PermissionDenied,
                dir,
                "untrusted socket path component",
            ));
        }
        // In a trusted sticky directory (/tmp), other users cannot replace a
        // child owned by the effective user or root. Every child is checked too.
        if meta.mode() & 0o022 != 0 && meta.mode() & 0o1000 == 0 {
            return Err(path_error(
                io::ErrorKind::PermissionDenied,
                dir,
                "socket path is writable by other users",
            ));
        }
        if dir == parent && (meta.uid() != uid || meta.mode() & 0o7777 != 0o700) {
            return Err(path_error(
                io::ErrorKind::PermissionDenied,
                dir,
                "socket directory must be owned by the effective user and have mode 0700",
            ));
        }
    }
    Ok(())
}

pub(crate) fn bind_with_lease(lease: SingletonLease, path: &Path) -> io::Result<BoundSocket> {
    if path.as_os_str().as_bytes().ends_with(b"/")
        || path.as_os_str().as_bytes().ends_with(b"/.")
        || path.components().any(|part| part == Component::ParentDir)
    {
        return Err(path_error(
            io::ErrorKind::InvalidInput,
            path,
            "expected a socket filename without '..' or a trailing slash",
        ));
    }
    let name = path
        .file_name()
        .ok_or_else(|| path_error(io::ErrorKind::InvalidInput, path, "missing socket filename"))?;
    let absolute = std::path::absolute(path)?;
    let parent = absolute
        .parent()
        .expect("absolute socket filename has a parent");
    let owner = lease.owner;
    validate_ancestors(parent, owner)?;
    let real_parent = fs::canonicalize(parent)?;
    validate_ancestors(&real_parent, owner)?;
    let endpoint = real_parent.join(name);
    let addr = SocketAddress::new(&endpoint)?;
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
        .open(&real_parent)?;
    let directory_metadata = directory.metadata()?;
    if !same_inode(&directory_metadata, &fs::symlink_metadata(&real_parent)?)
        || !same_inode(&directory_metadata, &lease.metadata)
        || directory_metadata.uid() != owner
        || directory_metadata.mode() & 0o7777 != 0o700
    {
        return Err(path_error(
            io::ErrorKind::PermissionDenied,
            &real_parent,
            "socket directory changed after the runtime lease was acquired",
        ));
    }
    require_absent_endpoint(&endpoint, owner)?;
    let fd = new_socket()?;
    // SAFETY: addr stores an initialized sockaddr_un of the supplied length; fd is valid.
    if unsafe { libc::bind(fd.as_raw_fd(), addr.as_ptr(), addr.len) } == -1 {
        let error = io::Error::last_os_error();
        return Err(path_error(error.kind(), &endpoint, error));
    }
    let meta = fs::symlink_metadata(&endpoint)?;
    if !meta.file_type().is_socket() || meta.uid() != owner || meta.nlink() != 1 {
        return Err(path_error(
            io::ErrorKind::PermissionDenied,
            &endpoint,
            "bound socket was replaced",
        ));
    }
    let cleanup = SocketPath {
        path: endpoint.clone(),
        device: meta.dev(),
        inode: meta.ino(),
    };
    // The private directory blocks other users even before chmod; the socket
    // never starts listening until its own mode is 0600, whatever the umask.
    fs::set_permissions(&endpoint, Permissions::from_mode(0o600))?;
    // SAFETY: fd is a valid bound Unix stream socket.
    if unsafe { libc::listen(fd.as_raw_fd(), MAX_CONNECTIONS as libc::c_int) } == -1 {
        let error = io::Error::last_os_error();
        return Err(path_error(error.kind(), &endpoint, error));
    }
    Ok(BoundSocket {
        listener: fd.into(),
        _path: cleanup,
        _lease: lease,
        owner,
    })
}

struct SocketAddress {
    raw: libc::sockaddr_un,
    len: libc::socklen_t,
}

impl SocketAddress {
    fn new(path: &Path) -> io::Result<Self> {
        // SAFETY: the C sockaddr_un consists of integer fields and byte storage.
        let mut raw: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        let bytes = path.as_os_str().as_bytes();
        if bytes.len() >= raw.sun_path.len() || bytes.contains(&0) {
            return Err(path_error(
                io::ErrorKind::InvalidInput,
                path,
                "Unix socket path is too long or contains NUL",
            ));
        }
        raw.sun_family = libc::AF_UNIX as libc::sa_family_t;
        for (slot, byte) in raw.sun_path.iter_mut().zip(bytes) {
            *slot = *byte as libc::c_char;
        }
        let len = (std::mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1)
            as libc::socklen_t;
        #[cfg(any(
            target_os = "macos",
            target_os = "ios",
            target_os = "freebsd",
            target_os = "netbsd",
            target_os = "openbsd",
            target_os = "dragonfly"
        ))]
        {
            raw.sun_len = len as u8;
        }
        Ok(Self { raw, len })
    }

    fn as_ptr(&self) -> *const libc::sockaddr {
        (&raw const self.raw).cast()
    }
}

fn new_socket() -> io::Result<OwnedFd> {
    // SAFETY: socket takes no pointers, and a nonnegative result is a new owned FD.
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fd is newly allocated and transferred exactly once.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    // SAFETY: fcntl receives a valid FD and the integer flag it expects.
    if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(fd)
}

fn require_absent_endpoint(path: &Path, uid: u32) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(path_error(error.kind(), path, error)),
        Ok(meta) if meta.file_type().is_socket() && meta.uid() == uid && meta.nlink() == 1 => {
            // A failed connect does not prove a stale inode: an active listener
            // with a full backlog may also refuse connections. Never unlink it.
            Err(path_error(
                io::ErrorKind::AddrInUse,
                path,
                "socket already exists; confirm the daemon is stopped before removing a stale socket manually",
            ))
        }
        Ok(_) => Err(path_error(
            io::ErrorKind::AlreadyExists,
            path,
            "unsafe socket endpoint (not an owned single-link socket); leaving it untouched",
        )),
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn peer_is_owner(stream: &UnixStream, uid: u32) -> bool {
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let expected = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let mut len = expected;
    // SAFETY: cred and len are live initialized output buffers of the stated size.
    let status = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut cred).cast(),
            &mut len,
        )
    };
    status == 0 && len == expected && cred.uid == uid
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn peer_is_owner(stream: &UnixStream, uid: u32) -> bool {
    let mut peer_uid = 0;
    let mut peer_gid = 0;
    // SAFETY: stream is a connected UDS and both output pointers are valid.
    let status = unsafe { libc::getpeereid(stream.as_raw_fd(), &mut peer_uid, &mut peer_gid) };
    status == 0 && peer_uid == uid
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Read};
    use std::os::unix::fs::{DirBuilderExt, symlink};

    fn private_dir() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("a4m-")
            .permissions(Permissions::from_mode(0o700))
            .tempdir_in("/tmp")
            .unwrap()
    }

    fn bind(path: &Path) -> io::Result<BoundSocket> {
        let parent = path.parent().expect("fixture socket has a parent");
        bind_with_lease(acquire_singleton(parent)?, path)
    }

    fn request(stream: &mut UnixStream, id: u64, method: &str) {
        let frame = encode_frame(&json!({"jsonrpc":"2.0", "id":id, "method":method})).unwrap();
        stream.write_all(&frame).unwrap();
    }

    fn receive(reader: &mut BufReader<UnixStream>) -> Value {
        let mut line = String::new();
        assert_ne!(reader.read_line(&mut line).unwrap(), 0);
        serde_json::from_str(&line).unwrap()
    }

    #[test]
    fn singleton_lease_is_exclusive_before_socket_bind() {
        let dir = private_dir();
        let lease = acquire_singleton(dir.path()).unwrap();
        assert_eq!(
            acquire_singleton(dir.path()).err().unwrap().kind(),
            io::ErrorKind::AddrInUse
        );
        assert!(!dir.path().join("rpc.sock").exists());
        drop(lease);
        let rebound = acquire_singleton(dir.path()).unwrap();
        drop(rebound);
    }

    #[test]
    fn socket_mode_lock_and_active_endpoint() {
        let dir = private_dir();
        let path = dir.path().join("rpc.sock");
        let existing = UnixListener::bind(&path).unwrap();
        assert_eq!(bind(&path).err().unwrap().kind(), io::ErrorKind::AddrInUse);
        assert!(path.exists());
        drop(existing);
        let stale = bind(&path).err().unwrap();
        assert_eq!(stale.kind(), io::ErrorKind::AddrInUse);
        assert!(stale.to_string().contains("confirm the daemon is stopped"));
        assert!(fs::symlink_metadata(&path).unwrap().file_type().is_socket());
        fs::remove_file(&path).unwrap();

        let owned = bind(&path).unwrap();
        assert_eq!(fs::symlink_metadata(&path).unwrap().mode() & 0o7777, 0o600);
        assert_eq!(
            acquire_singleton(dir.path()).err().unwrap().kind(),
            io::ErrorKind::AddrInUse
        );
        assert_eq!(bind(&path).err().unwrap().kind(), io::ErrorKind::AddrInUse);
        let _peer = UnixStream::connect(&path).unwrap();
        let (peer, _) = owned.listener.accept().unwrap();
        assert!(peer_is_owner(&peer, owned.owner));
        assert!(!peer_is_owner(&peer, owned.owner.wrapping_add(1)));
        drop(owned);
        assert!(!path.exists());
        // Normal teardown releases both the endpoint and the directory lease.
        let rebound = bind(&path).unwrap();
        drop(rebound);
        assert!(!path.exists());
    }

    #[test]
    fn unsafe_paths_are_never_removed_or_followed() {
        let dir = private_dir();
        let path = dir.path().join("rpc.sock");
        fs::write(&path, b"keep").unwrap();
        assert_eq!(
            bind(&path).err().unwrap().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(fs::read(&path).unwrap(), b"keep");
        let link = dir.path().join("endpoint-link");
        symlink(&path, &link).unwrap();
        assert!(bind(&link).is_err());
        assert!(fs::symlink_metadata(link).unwrap().file_type().is_symlink());
        let real_parent = dir.path().join("real");
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&real_parent)
            .unwrap();
        let link_parent = dir.path().join("parent-link");
        symlink(&real_parent, &link_parent).unwrap();
        assert!(bind(&link_parent.join("socket")).is_err());
        let unsafe_parent = dir.path().join("shared");
        fs::create_dir(&unsafe_parent).unwrap();
        fs::set_permissions(&unsafe_parent, Permissions::from_mode(0o755)).unwrap();
        assert!(bind(&unsafe_parent.join("socket")).is_err());
        assert!(!unsafe_parent.join("socket").exists());
        assert!(bind(&real_parent.join("wrong/../socket")).is_err());
    }

    #[test]
    fn cleanup_does_not_unlink_replacement() {
        let dir = private_dir();
        let path = dir.path().join("socket");
        let owned = bind(&path).unwrap();
        fs::remove_file(&path).unwrap();
        fs::write(&path, b"different inode").unwrap();
        drop(owned);
        assert_eq!(fs::read(&path).unwrap(), b"different inode");
    }

    #[test]
    fn ready_failure_cleans_own_socket_and_releases_lease() {
        let dir = private_dir();
        let path = dir.path().join("rpc.sock");
        let error = serve(
            &path,
            acquire_singleton(dir.path()).unwrap(),
            Hub::new(),
            || Err(io::Error::other("fixture startup failed")),
            |_, _, _| Ok(Value::Null),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(error.to_string(), "fixture startup failed");
        assert!(!path.exists());
        // A failed serve must not leave the directory locked.
        let rebound = bind(&path).unwrap();
        assert_eq!(fs::symlink_metadata(&path).unwrap().mode() & 0o7777, 0o600);
        drop(rebound);
        assert!(!path.exists());
    }

    #[test]
    fn frame_limits_keep_boundaries_and_reject_partial_input() {
        let mut input = vec![b'a'; MAX_FRAME_BYTES];
        input.extend_from_slice(b"\r\nhello\nunterminated");
        let mut reader = BufReader::with_capacity(7, Cursor::new(input));
        let mut frame = Vec::new();
        assert!(read_frame(&mut reader, &mut frame).unwrap());
        assert_eq!(frame.len(), MAX_FRAME_BYTES);
        assert!(read_frame(&mut reader, &mut frame).unwrap());
        assert_eq!(frame, b"hello");
        assert!(!read_frame(&mut reader, &mut frame).unwrap());
        let mut reader = Cursor::new(vec![b'a'; MAX_FRAME_BYTES + 1]);
        assert_eq!(
            read_frame(&mut reader, &mut frame).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(
            encode_frame(&Value::String("x".repeat(MAX_FRAME_BYTES)))
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn rpc_framing_notifications_and_subscription_order() {
        let hub = Hub::new();
        let publisher = hub.clone();
        let connection_hub = hub.clone();
        let (server, mut client) = UnixStream::pair().unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut reader = BufReader::new(client.try_clone().unwrap());
        let worker = thread::spawn(move || {
            handle_connection(
                server,
                connection_hub,
                &|connection, method, params| match method {
                    "subscribe" => {
                        connection.subscribe("tick")?;
                        // The topic is not visible before the subscription is acknowledged.
                        assert_eq!(
                            publisher.publish("tick", "tick", json!({"too_early":true})),
                            0
                        );
                        Ok(json!(true))
                    }
                    "unsubscribe" => Ok(json!(connection.unsubscribe("tick"))),
                    "try_invalid" => {
                        connection.subscribe("tick")?;
                        Err(RpcError::new(-32602, "bad topics").with_data(json!({"bad":true})))
                    }
                    "echo" => Ok(Value::Object(params)),
                    _ => Err(RpcError::new(-32601, "Method not found")),
                },
            )
            .unwrap();
        });
        client.write_all(b"not JSON\n[]\n{\"jsonrpc\":\"2.0\",\"method\":\"echo\",\"id\":false}\n{\"jsonrpc\":\"2.0\",\"method\":\"echo\",\"params\":[1],\"id\":null}\n").unwrap();
        assert_eq!(receive(&mut reader)["error"]["code"], -32700);
        assert_eq!(receive(&mut reader)["error"]["code"], -32600);
        assert_eq!(receive(&mut reader)["error"]["code"], -32600);
        let response = receive(&mut reader);
        assert_eq!(response["error"]["code"], -32602);
        assert_eq!(response["id"], Value::Null);
        request(&mut client, 1, "subscribe");
        let response = receive(&mut reader);
        assert_eq!(response["id"], 1);
        assert_eq!(response["result"], true);
        assert_eq!(hub.publish("tick", "vital_signs.tick", json!({"v":1})), 1);
        assert_eq!(
            receive(&mut reader),
            json!({"jsonrpc":"2.0","method":"vital_signs.tick","params":{"v":1}})
        );
        request(&mut client, 2, "unsubscribe");
        assert_eq!(receive(&mut reader)["result"], true);
        assert_eq!(hub.publish("tick", "vital_signs.tick", json!({})), 0);
        request(&mut client, 3, "try_invalid");
        assert_eq!(receive(&mut reader)["error"]["data"]["bad"], true);
        assert_eq!(hub.publish("tick", "tick", json!({})), 0);
        client
            .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"unknown\"}\n")
            .unwrap();
        request(&mut client, 4, "echo");
        client.shutdown(Shutdown::Write).unwrap();
        assert_eq!(
            receive(&mut reader),
            json!({"jsonrpc":"2.0","id":4,"result":{}})
        );
        worker.join().unwrap();
        assert_eq!(reader.read(&mut [0]).unwrap(), 0);
    }

    #[test]
    fn publisher_disconnects_only_the_backpressured_subscriber() {
        let hub = Hub::new();
        let (slow_server, mut slow_client) = UnixStream::pair().unwrap();
        let (fast_server, _fast_client) = UnixStream::pair().unwrap();
        let (slow_tx, _slow_rx) = mpsc::sync_channel(QUEUE_DEPTH);
        let (fast_tx, fast_rx) = mpsc::sync_channel(QUEUE_DEPTH);
        let slow = hub.register(slow_tx, slow_server);
        let fast = hub.register(fast_tx, fast_server);
        let topics = BTreeSet::from(["tick".to_owned()]);
        assert!(hub.complete(slow, None, Some(&topics)));
        assert!(hub.complete(fast, None, Some(&topics)));
        for _ in 0..QUEUE_DEPTH {
            assert_eq!(hub.publish("tick", "tick", json!({})), 2);
            assert!(fast_rx.try_recv().is_ok());
        }
        assert_eq!(hub.publish("tick", "tick", json!({})), 1);
        assert!(!hub.complete(slow, None, None));
        assert!(hub.complete(fast, None, None));
        assert_eq!(slow_client.read(&mut [0]).unwrap(), 0);
    }
}
