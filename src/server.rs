//! Bounded-queue, backpressure, resumable-by-manifest, and session-isolated FASTLINK server/client runtime.
//!
//! Preserves exact FASTLINK v0.1 wire framing (`MAGIC`, `VERSION`, `CMD_SYNC`, manifest, `NEED` list,
//! chunk frames, and `status: u8 = 0`) while providing:
//! 1. **Bounded work queue + backpressure (`BoundedQueue`)**: caps in-flight chunk buffers per session
//!    and accepted client connections per server, blocking producers when full instead of allowing
//!    unbounded memory growth.
//! 2. **Resume after disconnect (re-attach by manifest, no full resend)**: verified chunks received
//!    before a connection drop are atomically persisted in the content-addressed store, while
//!    session-scoped temporary staging files are cleaned up on abort so no partial output is ever
//!    committed. Re-sending the manifest causes the server's `NEED` list to request only the
//!    remaining missing chunks.
//! 3. **Per-session isolation & concurrent client safety**: unique per-session atomic staging paths
//!    and per-output commit locks guarantee byte-exact materialization and zero cache corruption
//!    under concurrent client transfers (`N=4+`).

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::codec::{LZ4, RAW, RLE, ZSTD};
use crate::{
    chunk_count, decode_chunk, digest, encode_chunk, expected_chunk_len, hex, mib_per_sec,
    read_u16, read_u32, read_u64, read_u8, recover_atomic_writes, write_u16, write_u32, write_u64,
    write_u8, CountingStream, Digest, TransferMetrics, WireBreakdown, CHUNK_BYTES, MAGIC,
    MAX_CHUNKS, MAX_ENCODED_BYTES, VERSION,
};

pub const CMD_SYNC: u8 = 1;

static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(1);

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

/// Thread-safe bounded FIFO queue with condition-variable backpressure.
///
/// When the queue reaches `capacity`, `push` blocks the producer until a consumer pops an item
/// or the queue is closed, preventing unbounded memory growth and propagating TCP backpressure
/// naturally to the socket reader.
pub struct BoundedQueue<T> {
    capacity: usize,
    state: Mutex<QueueState<T>>,
    not_empty: Condvar,
    not_full: Condvar,
    backpressure_waits: AtomicU64,
    peak_depth: AtomicUsize,
}

struct QueueState<T> {
    items: VecDeque<T>,
    closed: bool,
}

impl<T> BoundedQueue<T> {
    pub fn new(capacity: usize) -> Self {
        let cap = capacity.max(1);
        Self {
            capacity: cap,
            state: Mutex::new(QueueState {
                items: VecDeque::with_capacity(cap),
                closed: false,
            }),
            not_empty: Condvar::new(),
            not_full: Condvar::new(),
            backpressure_waits: AtomicU64::new(0),
            peak_depth: AtomicUsize::new(0),
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn backpressure_waits(&self) -> u64 {
        self.backpressure_waits.load(Ordering::Relaxed)
    }

    pub fn peak_depth(&self) -> usize {
        self.peak_depth.load(Ordering::Relaxed)
    }

    /// Enqueue an item, blocking if the queue is currently at capacity.
    /// Returns `Err` if the queue has been closed.
    pub fn push(&self, item: T) -> Result<(), T> {
        let mut guard = self.state.lock().expect("BoundedQueue mutex poisoned");
        while guard.items.len() >= self.capacity && !guard.closed {
            self.backpressure_waits.fetch_add(1, Ordering::Relaxed);
            guard = self
                .not_full
                .wait(guard)
                .expect("BoundedQueue condvar wait poisoned");
        }
        if guard.closed {
            return Err(item);
        }
        guard.items.push_back(item);
        let depth = guard.items.len();
        self.peak_depth.fetch_max(depth, Ordering::Relaxed);
        self.not_empty.notify_one();
        Ok(())
    }

    /// Pop the next item, blocking until an item is available or the queue is closed and drained.
    pub fn pop(&self) -> Option<T> {
        let mut guard = self.state.lock().expect("BoundedQueue mutex poisoned");
        while guard.items.is_empty() && !guard.closed {
            guard = self
                .not_empty
                .wait(guard)
                .expect("BoundedQueue condvar wait poisoned");
        }
        let item = guard.items.pop_front();
        if item.is_some() {
            self.not_full.notify_one();
        }
        item
    }

    /// Close the queue and wake all waiting producers and consumers.
    pub fn close(&self) {
        let mut guard = self.state.lock().expect("BoundedQueue mutex poisoned");
        guard.closed = true;
        self.not_empty.notify_all();
        self.not_full.notify_all();
    }
}

/// Configuration for the FASTLINK server and session queues.
#[derive(Debug, Clone, Copy)]
pub struct ServerConfig {
    /// Maximum number of decoded/encoded chunks buffered in memory per session before backpressure stalls socket reads.
    pub chunk_queue_capacity: usize,
    /// Maximum number of pending accepted connections in the server worker queue before accept backpressure applies.
    pub connection_queue_capacity: usize,
    /// Number of concurrent session worker threads.
    pub max_concurrent_sessions: usize,
    /// Per-socket read/write timeout.
    pub io_timeout: Duration,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            chunk_queue_capacity: 8,
            connection_queue_capacity: 16,
            max_concurrent_sessions: 8,
            io_timeout: Duration::from_secs(300),
        }
    }
}

/// Shared server coordination state for concurrent sessions and atomic cache/output commits.
#[derive(Default)]
pub struct ServerCoord {
    output_locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    chunk_locks: Mutex<HashMap<Digest, Arc<Mutex<()>>>>,
    active_sessions: AtomicUsize,
    peak_concurrent_sessions: AtomicUsize,
    total_sessions_completed: AtomicU64,
    total_sessions_aborted: AtomicU64,
    total_backpressure_waits: AtomicU64,
}

impl ServerCoord {
    pub fn new() -> Self {
        Self::default()
    }

    fn output_lock(&self, name: &str) -> Arc<Mutex<()>> {
        let mut map = self.output_locks.lock().expect("output_locks poisoned");
        map.entry(name.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    fn chunk_lock(&self, hash: &Digest) -> Arc<Mutex<()>> {
        let mut map = self.chunk_locks.lock().expect("chunk_locks poisoned");
        map.entry(*hash)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    pub fn active_sessions(&self) -> usize {
        self.active_sessions.load(Ordering::Relaxed)
    }

    pub fn peak_concurrent_sessions(&self) -> usize {
        self.peak_concurrent_sessions.load(Ordering::Relaxed)
    }

    pub fn total_sessions_completed(&self) -> u64 {
        self.total_sessions_completed.load(Ordering::Relaxed)
    }

    pub fn total_sessions_aborted(&self) -> u64 {
        self.total_sessions_aborted.load(Ordering::Relaxed)
    }

    pub fn total_backpressure_waits(&self) -> u64 {
        self.total_backpressure_waits.load(Ordering::Relaxed)
    }
}

pub fn digest_path(root: &Path, hash: &Digest) -> PathBuf {
    let s = hex(hash);
    root.join("chunks").join(&s[..2]).join(s)
}

/// Refuse to read or write through a symlink. Regular files, directories and
/// missing paths are fine.
fn ensure_not_symlink(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(md) if md.file_type().is_symlink() => {
            Err(invalid("refusing to follow a symlink in the data root"))
        }
        _ => Ok(()),
    }
}

/// Read a regular file that is at most `limit` bytes.
///
/// The size is taken from metadata before the read, so a planted oversized
/// cache object cannot be loaded to test its digest.
fn read_bounded_file(path: &Path, limit: usize) -> io::Result<Vec<u8>> {
    ensure_not_symlink(path)?;
    let meta = fs::symlink_metadata(path)?;
    if !meta.file_type().is_file() {
        return Err(invalid("cache entry is not a regular file"));
    }
    let len = meta.len();
    if len > limit as u64 {
        return Err(invalid("cached object exceeds size cap"));
    }
    let mut file = File::open(path)?;
    let mut buf = vec![0u8; len as usize];
    file.read_exact(&mut buf)?;
    Ok(buf)
}

pub fn cached_and_valid(path: &Path, expected: &Digest) -> bool {
    match read_bounded_file(path, CHUNK_BYTES) {
        Ok(bytes) => digest(&bytes) == *expected,
        Err(_) => false,
    }
}

/// Atomically store a verified chunk using a session-isolated temporary file so concurrent
/// sessions writing the same deduplicated chunk never clobber each other's temporary file.
pub fn store_chunk_isolated(
    root: &Path,
    hash: &Digest,
    bytes: &[u8],
    session_id: u64,
    coord: &ServerCoord,
) -> io::Result<()> {
    if bytes.len() > CHUNK_BYTES || digest(bytes) != *hash {
        return Err(invalid(
            "refusing to store a chunk that does not match its digest",
        ));
    }
    let path = digest_path(root, hash);
    if cached_and_valid(&path, hash) {
        return Ok(());
    }
    let lock = coord.chunk_lock(hash);
    let _guard = lock.lock().expect("chunk lock poisoned");
    if cached_and_valid(&path, hash) {
        return Ok(());
    }
    fs::create_dir_all(path.parent().ok_or_else(|| invalid("invalid cache path"))?)?;
    let tmp = path.with_extension(format!("partial.s{session_id}"));
    let write_res = (|| -> io::Result<()> {
        let mut f = File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        fs::rename(&tmp, &path)?;
        Ok(())
    })();
    if write_res.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    write_res
}

pub fn safe_basename(name: &str) -> io::Result<&str> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.len() > 240
        || name.contains('/')
        || name.contains('\\')
        || name.contains('\0')
    {
        return Err(invalid("unsafe or overlong file name"));
    }
    Ok(name)
}

pub fn client_handshake(stream: &mut (impl Read + Write)) -> io::Result<()> {
    stream.write_all(MAGIC)?;
    write_u16(stream, VERSION)?;
    stream.flush()?;
    let mut magic = [0; 4];
    stream.read_exact(&mut magic)?;
    let version = read_u16(stream)?;
    if &magic != MAGIC || version != VERSION {
        return Err(invalid("incompatible FASTLINK protocol"));
    }
    Ok(())
}

pub fn server_handshake(stream: &mut (impl Read + Write)) -> io::Result<()> {
    let mut magic = [0; 4];
    stream.read_exact(&mut magic)?;
    let version = read_u16(stream)?;
    if &magic != MAGIC || version != VERSION {
        return Err(invalid("incompatible FASTLINK protocol"));
    }
    stream.write_all(MAGIC)?;
    write_u16(stream, VERSION)?;
    stream.flush()?;
    Ok(())
}

struct QueuedChunkFrame {
    index: u32,
    codec: u8,
    encoded: Vec<u8>,
}

/// Process a single client SYNC session with bounded-queue backpressure and session isolation.
pub fn process_session_with_config(
    mut stream: TcpStream,
    root: &Path,
    config: ServerConfig,
    coord: &ServerCoord,
) -> io::Result<()> {
    let session_id = NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed);
    let active = coord.active_sessions.fetch_add(1, Ordering::SeqCst) + 1;
    coord
        .peak_concurrent_sessions
        .fetch_max(active, Ordering::SeqCst);

    struct SessionGuard<'a> {
        coord: &'a ServerCoord,
        completed: bool,
    }
    impl Drop for SessionGuard<'_> {
        fn drop(&mut self) {
            self.coord.active_sessions.fetch_sub(1, Ordering::SeqCst);
            if self.completed {
                self.coord
                    .total_sessions_completed
                    .fetch_add(1, Ordering::Relaxed);
            } else {
                self.coord
                    .total_sessions_aborted
                    .fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    let mut session_guard = SessionGuard {
        coord,
        completed: false,
    };

    // Drop WP-D crash leftovers before this session can rename anything.
    // WP-A session temps are a different name pattern and are handled by
    // `clean_stale_partials` at server start.
    recover_atomic_writes(root)?;

    stream.set_read_timeout(Some(config.io_timeout))?;
    stream.set_write_timeout(Some(config.io_timeout))?;
    server_handshake(&mut stream)?;

    let command = read_u8(&mut stream)?;
    if command != CMD_SYNC {
        return Err(invalid("unknown command"));
    }
    let name_len = read_u16(&mut stream)? as usize;
    if name_len == 0 || name_len > 240 {
        return Err(invalid("invalid filename length"));
    }
    let mut name_data = vec![0u8; name_len];
    stream.read_exact(&mut name_data)?;
    let name = String::from_utf8(name_data).map_err(|_| invalid("filename is not UTF-8"))?;
    safe_basename(&name)?;
    let size = read_u64(&mut stream)?;
    let chunk_bytes = read_u32(&mut stream)?;
    let count = read_u32(&mut stream)?;
    if chunk_bytes as usize != CHUNK_BYTES || count != chunk_count(size)? {
        return Err(invalid("invalid chunk size or count"));
    }
    let mut hashes = Vec::<Digest>::with_capacity(count as usize);
    for _ in 0..count {
        let mut hash = [0u8; 32];
        stream.read_exact(&mut hash)?;
        hashes.push(hash);
    }

    // Consult the content-addressed chunk cache to build the NEED list.
    // On a resumed session after a mid-transfer disconnect, any chunks already verified and
    // stored during the interrupted attempt are skipped here automatically.
    let mut missing = Vec::new();
    let mut requested_hashes = HashSet::<Digest>::new();
    for (idx, hash) in hashes.iter().enumerate() {
        if !cached_and_valid(&digest_path(root, hash), hash) && requested_hashes.insert(*hash) {
            missing.push(idx as u32);
        }
    }
    write_u32(&mut stream, missing.len() as u32)?;
    for idx in &missing {
        write_u32(&mut stream, *idx)?;
    }
    stream.flush()?;

    // Receive missing chunks through a bounded in-memory queue (`chunk_queue_capacity`).
    // The reader thread reads framed chunks from the socket and pushes them to `chunk_queue`
    // (blocking when full -> TCP receive window backpressure), while the consumer verifies,
    // decodes, and persists each chunk to the content-addressed store.
    if !missing.is_empty() {
        let queue = Arc::new(BoundedQueue::<QueuedChunkFrame>::new(
            config.chunk_queue_capacity,
        ));
        let hashes_arc = Arc::new(hashes.clone());
        let root_buf = root.to_path_buf();
        let queue_consumer = Arc::clone(&queue);

        let consumer_result: io::Result<()> = thread::scope(|s| {
            let consumer_handle = s.spawn(move || -> io::Result<()> {
                while let Some(frame) = queue_consumer.pop() {
                    let chunk = decode_chunk(
                        frame.codec,
                        &frame.encoded,
                        expected_chunk_len(size, frame.index)?,
                    )?;
                    let expected_hash = &hashes_arc[frame.index as usize];
                    if digest(&chunk) != *expected_hash {
                        queue_consumer.close();
                        return Err(invalid("chunk hash mismatch"));
                    }
                    if let Err(e) =
                        store_chunk_isolated(&root_buf, expected_hash, &chunk, session_id, coord)
                    {
                        queue_consumer.close();
                        return Err(e);
                    }
                }
                Ok(())
            });

            let mut read_err: Option<io::Error> = None;
            for expected_index in &missing {
                let frame_res = (|| -> io::Result<QueuedChunkFrame> {
                    let index = read_u32(&mut stream)?;
                    if index != *expected_index {
                        return Err(invalid("chunk order mismatch"));
                    }
                    let codec = read_u8(&mut stream)?;
                    // Reject before reading a declared length, so an unknown
                    // codec cannot force a large allocation.
                    if !matches!(codec, RAW | RLE | LZ4 | ZSTD) {
                        return Err(invalid("unknown codec"));
                    }
                    let compressed_size = read_u32(&mut stream)? as usize;
                    let expected = expected_chunk_len(size, index)?;
                    let size_ok = match codec {
                        RAW => compressed_size == expected,
                        RLE => {
                            compressed_size.is_multiple_of(2)
                                && compressed_size
                                    <= expected.saturating_mul(2).min(MAX_ENCODED_BYTES)
                        }
                        LZ4 | ZSTD => compressed_size <= MAX_ENCODED_BYTES,
                        _ => false,
                    };
                    if !size_ok || compressed_size > MAX_ENCODED_BYTES {
                        return Err(invalid("chunk payload too large"));
                    }
                    let mut encoded = vec![0u8; compressed_size];
                    stream.read_exact(&mut encoded)?;
                    Ok(QueuedChunkFrame {
                        index,
                        codec,
                        encoded,
                    })
                })();

                match frame_res {
                    Ok(frame) => {
                        if queue.push(frame).is_err() {
                            read_err = Some(invalid("chunk consumer aborted early"));
                            break;
                        }
                    }
                    Err(e) => {
                        read_err = Some(e);
                        break;
                    }
                }
            }
            queue.close();
            let consumer_outcome = consumer_handle
                .join()
                .map_err(|_| io::Error::other("chunk consumer thread panicked"))?;
            if let Some(e) = read_err {
                return Err(e);
            }
            consumer_outcome
        });

        coord
            .total_backpressure_waits
            .fetch_add(queue.backpressure_waits(), Ordering::Relaxed);
        consumer_result?;
    }

    // Materialize the final output atomically using a per-session partial file and a per-output
    // lock so concurrent sessions never corrupt or overwrite in-flight partial files, and an
    // interrupted session never leaves a partial file committed at `outputs/<name>`.
    let output_dir = root.join("outputs");
    fs::create_dir_all(&output_dir)?;
    let out_lock = coord.output_lock(&name);
    let _out_guard = out_lock.lock().expect("output lock poisoned");

    let output = output_dir.join(&name);
    let partial = output_dir.join(format!(".{name}.fastlink-partial.s{session_id}"));
    let result = (|| -> io::Result<()> {
        let mut destination = File::create(&partial)?;
        for (idx, hash) in hashes.iter().enumerate() {
            let bytes = read_bounded_file(&digest_path(root, hash), CHUNK_BYTES)?;
            if bytes.len() != expected_chunk_len(size, idx as u32)? || digest(&bytes) != *hash {
                return Err(invalid("cache integrity failure"));
            }
            destination.write_all(&bytes)?;
        }
        destination.sync_all()?;
        drop(destination);
        fs::rename(&partial, &output)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&partial);
    }
    result?;

    write_u8(&mut stream, 0)?;
    stream.flush()?;
    session_guard.completed = true;
    println!("materialized {} ({} bytes)", output.display(), size);
    Ok(())
}

/// Process a single client SYNC session with default server configuration.
pub fn process_session(stream: TcpStream, root: &Path) -> io::Result<()> {
    let coord = ServerCoord::new();
    process_session_with_config(stream, root, ServerConfig::default(), &coord)
}

/// Remove leftover session temp files on server startup.
///
/// Output temps are dotfiles (`.{name}.fastlink-partial.sN`). A committed
/// output whose basename contains `fastlink-partial` is left in place.
pub fn clean_stale_partials(root: &Path) -> io::Result<usize> {
    let mut removed = 0usize;
    let outputs = root.join("outputs");
    if outputs.exists() {
        for entry in fs::read_dir(&outputs)? {
            let entry = entry?;
            let fname = entry.file_name();
            let s = fname.to_string_lossy();
            if s.starts_with('.')
                && (s.contains(".fastlink-partial") || s.ends_with(".partial"))
                && fs::remove_file(entry.path()).is_ok()
            {
                removed += 1;
            }
        }
    }
    let chunks = root.join("chunks");
    if chunks.exists() {
        for sub in fs::read_dir(&chunks)? {
            let sub = sub?;
            if sub.path().is_dir() {
                for entry in fs::read_dir(sub.path())? {
                    let entry = entry?;
                    let fname = entry.file_name();
                    let s = fname.to_string_lossy();
                    if s.contains(".partial") && fs::remove_file(entry.path()).is_ok() {
                        removed += 1;
                    }
                }
            }
        }
    }
    Ok(removed)
}

/// Audit every stored chunk in `<root>/chunks/` against its content-addressed filename digest
/// and verify that no temporary `.partial*` or `.fastlink-partial*` files remain in the cache or outputs.
/// Returns the number of verified cached chunks.
pub fn verify_cache_integrity(root: &Path) -> io::Result<usize> {
    let mut verified_chunks = 0usize;
    let chunks_dir = root.join("chunks");
    if chunks_dir.exists() {
        for sub in fs::read_dir(&chunks_dir)? {
            let sub = sub?;
            if sub.path().is_dir() {
                for entry in fs::read_dir(sub.path())? {
                    let entry = entry?;
                    let fname = entry.file_name().to_string_lossy().to_string();
                    if fname.contains(".partial") {
                        return Err(invalid(format!("leftover partial chunk file: {fname}")));
                    }
                    let bytes = read_bounded_file(&entry.path(), CHUNK_BYTES).map_err(|_| {
                        invalid(format!("oversized or unreadable cached chunk: {fname}"))
                    })?;
                    let actual_hex = hex(&digest(&bytes));
                    if actual_hex != fname {
                        return Err(invalid(format!(
                            "cache corruption: expected {fname}, got {actual_hex}"
                        )));
                    }
                    verified_chunks += 1;
                }
            }
        }
    }
    let outputs_dir = root.join("outputs");
    if outputs_dir.exists() {
        for entry in fs::read_dir(&outputs_dir)? {
            let entry = entry?;
            let fname = entry.file_name().to_string_lossy().to_string();
            if fname.starts_with('.')
                && (fname.contains(".fastlink-partial") || fname.ends_with(".partial"))
            {
                return Err(invalid(format!("leftover partial output file: {fname}")));
            }
        }
    }
    Ok(verified_chunks)
}

/// Handle to a running background FASTLINK server with bounded connection queue and worker pool.
pub struct RunningServer {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    conn_queue: Arc<BoundedQueue<TcpStream>>,
    coord: Arc<ServerCoord>,
    accept_thread: Option<JoinHandle<()>>,
    worker_threads: Vec<JoinHandle<()>>,
}

impl RunningServer {
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn addr_string(&self) -> String {
        self.addr.to_string()
    }

    pub fn coord(&self) -> &Arc<ServerCoord> {
        &self.coord
    }

    pub fn shutdown(mut self) {
        self.stop_inner();
    }

    fn stop_inner(&mut self) {
        if !self.stop.swap(true, Ordering::SeqCst) {
            self.conn_queue.close();
            let _ = TcpStream::connect(self.addr);
        }
        if let Some(handle) = self.accept_thread.take() {
            let _ = handle.join();
        }
        for worker in self.worker_threads.drain(..) {
            let _ = worker.join();
        }
    }
}

impl Drop for RunningServer {
    fn drop(&mut self) {
        self.stop_inner();
    }
}

/// Defence-in-depth guard: the pre-alpha server binds loopback only.
pub fn validate_listen_address(address: &str) -> io::Result<SocketAddr> {
    let socket: SocketAddr = address
        .parse()
        .map_err(|_| invalid("invalid listen address"))?;
    if !socket.ip().is_loopback() {
        return Err(invalid(
            "pre-alpha server must bind to loopback; use SSH forwarding for remote hosts",
        ));
    }
    Ok(socket)
}

/// Defence-in-depth guard: the client may only connect to a loopback peer.
pub fn validate_connect_address(server: &str) -> io::Result<SocketAddr> {
    let socket: SocketAddr = server
        .parse()
        .map_err(|_| invalid("invalid server address"))?;
    if !socket.ip().is_loopback() {
        return Err(invalid(
            "pre-alpha client may only connect to loopback; use an SSH tunnel for remote hosts",
        ));
    }
    Ok(socket)
}

/// Spawn a multi-client FASTLINK server bound to `address` (`127.0.0.1:0` supported).
pub fn spawn_server(address: &str, root: &Path, config: ServerConfig) -> io::Result<RunningServer> {
    let socket = validate_listen_address(address)?;
    fs::create_dir_all(root.join("chunks"))?;
    fs::create_dir_all(root.join("outputs"))?;
    let _ = clean_stale_partials(root)?;
    let _ = recover_atomic_writes(root)?;

    let listener = TcpListener::bind(socket)?;
    let local_addr = listener.local_addr()?;
    let stop = Arc::new(AtomicBool::new(false));
    let conn_queue = Arc::new(BoundedQueue::<TcpStream>::new(
        config.connection_queue_capacity,
    ));
    let coord = Arc::new(ServerCoord::new());

    let worker_count = config.max_concurrent_sessions.max(1);
    let mut worker_threads = Vec::with_capacity(worker_count);
    for _ in 0..worker_count {
        let q = Arc::clone(&conn_queue);
        let root_buf = root.to_path_buf();
        let coord_clone = Arc::clone(&coord);
        worker_threads.push(thread::spawn(move || {
            while let Some(stream) = q.pop() {
                if let Err(e) = process_session_with_config(stream, &root_buf, config, &coord_clone)
                {
                    eprintln!("session error: {e}");
                }
            }
        }));
    }

    let stop_accept = Arc::clone(&stop);
    let q_accept = Arc::clone(&conn_queue);
    let accept_thread = thread::spawn(move || {
        for connection in listener.incoming() {
            if stop_accept.load(Ordering::SeqCst) {
                break;
            }
            match connection {
                Ok(stream) => {
                    if stop_accept.load(Ordering::SeqCst) {
                        break;
                    }
                    if q_accept.push(stream).is_err() {
                        break;
                    }
                }
                Err(err) => {
                    if stop_accept.load(Ordering::SeqCst) {
                        break;
                    }
                    eprintln!("accept error: {err}");
                }
            }
        }
        q_accept.close();
    });

    Ok(RunningServer {
        addr: local_addr,
        stop,
        conn_queue,
        coord,
        accept_thread: Some(accept_thread),
        worker_threads,
    })
}

/// Run the blocking CLI server loop with bounded queues and concurrent session workers.
pub fn serve(address: &str, root: &Path) -> io::Result<()> {
    let config = ServerConfig::default();
    let socket = validate_listen_address(address)?;
    fs::create_dir_all(root.join("chunks"))?;
    fs::create_dir_all(root.join("outputs"))?;
    let _ = clean_stale_partials(root)?;
    let _ = recover_atomic_writes(root)?;

    let listener = TcpListener::bind(socket)?;
    println!("FASTLINK pre-alpha listening at {socket}");
    println!("Cache: {}", root.display());
    println!("UNAUTHENTICATED MVP: DO NOT expose port publicly; use an SSH tunnel");

    let conn_queue = Arc::new(BoundedQueue::<TcpStream>::new(
        config.connection_queue_capacity,
    ));
    let coord = Arc::new(ServerCoord::new());
    let worker_count = config.max_concurrent_sessions.max(1);
    for _ in 0..worker_count {
        let q = Arc::clone(&conn_queue);
        let root_buf = root.to_path_buf();
        let coord_clone = Arc::clone(&coord);
        thread::spawn(move || {
            while let Some(stream) = q.pop() {
                if let Err(e) = process_session_with_config(stream, &root_buf, config, &coord_clone)
                {
                    eprintln!("session error: {e}");
                }
            }
        });
    }

    for connection in listener.incoming() {
        match connection {
            Ok(stream) => {
                if conn_queue.push(stream).is_err() {
                    break;
                }
            }
            Err(err) => eprintln!("accept error: {err}"),
        }
    }
    Ok(())
}

/// Options for `send_file_resumable`, supporting deterministic fault injection (`abort_after_chunks`)
/// and automatic resume-by-manifest on transient connection failure.
#[derive(Debug, Clone, Copy)]
pub struct SendOptions {
    /// Maximum automatic resume-by-manifest attempts after a connection drop (0 = single attempt).
    pub max_resume_retries: u32,
    /// Optional fault injection: intentionally drop the TCP connection after sending `K` chunks
    /// on the first attempt (`attempt == 0`) to test mid-transfer kill/disconnect behavior.
    pub abort_after_chunks: Option<u32>,
}

impl Default for SendOptions {
    fn default() -> Self {
        Self {
            max_resume_retries: 3,
            abort_after_chunks: None,
        }
    }
}

/// Perform a single SYNC attempt over one TCP connection with default 300s socket timeout.
/// If `abort_after_chunks` is `Some(k)` and `missing.len() > k`, sends `k` chunks, flushes, and
/// drops the connection returning `Err(ErrorKind::ConnectionAborted)`.
pub fn send_file_once_with_options(
    file_path: &Path,
    server: &str,
    abort_after_chunks: Option<u32>,
) -> io::Result<(String, TransferMetrics)> {
    send_file_once_with_timeout(
        file_path,
        server,
        abort_after_chunks,
        Duration::from_secs(300),
    )
}

/// Perform a single SYNC attempt over one TCP connection using `io_timeout` for socket reads and writes.
pub fn send_file_once_with_timeout(
    file_path: &Path,
    server: &str,
    abort_after_chunks: Option<u32>,
    io_timeout: Duration,
) -> io::Result<(String, TransferMetrics)> {
    validate_connect_address(server)?;
    let started = Instant::now();
    let filename = file_path
        .file_name()
        .and_then(|x| x.to_str())
        .ok_or_else(|| invalid("invalid input file name"))?;
    safe_basename(filename)?;
    let mut file = File::open(file_path)?;
    let size = file.metadata()?.len();
    let count = chunk_count(size)?;
    let name = filename.as_bytes();
    let mut hashes = Vec::<Digest>::with_capacity(count as usize);
    for index in 0..count {
        let mut chunk = vec![0u8; expected_chunk_len(size, index)?];
        file.read_exact(&mut chunk)?;
        hashes.push(digest(&chunk));
    }

    let tcp = TcpStream::connect(server)?;
    let clamped_timeout = io_timeout.max(Duration::from_millis(50));
    tcp.set_read_timeout(Some(clamped_timeout))?;
    tcp.set_write_timeout(Some(clamped_timeout))?;
    let mut stream = CountingStream::new(tcp);

    let t_handshake = Instant::now();
    client_handshake(&mut stream)?;
    let handshake_us = t_handshake.elapsed().as_micros() as u64;

    let t_manifest = Instant::now();
    write_u8(&mut stream, CMD_SYNC)?;
    write_u16(&mut stream, name.len() as u16)?;
    stream.write_all(name)?;
    write_u64(&mut stream, size)?;
    write_u32(&mut stream, CHUNK_BYTES as u32)?;
    write_u32(&mut stream, count)?;
    for hash in &hashes {
        stream.write_all(hash)?;
    }
    stream.flush()?;
    let missing_len = read_u32(&mut stream)?;
    if missing_len > count || missing_len > MAX_CHUNKS {
        return Err(invalid("server requested invalid missing count"));
    }
    let mut missing = Vec::with_capacity(missing_len as usize);
    let mut last = None;
    for _ in 0..missing_len {
        let idx = read_u32(&mut stream)?;
        if idx >= count || last.is_some_and(|x| idx <= x) {
            return Err(invalid("invalid or unordered missing index"));
        }
        missing.push(idx);
        last = Some(idx);
    }
    let manifest_us = t_manifest.elapsed().as_micros() as u64;

    let t_transfer = Instant::now();
    let mut wire_payload_bytes = 0u64;
    let mut raw_bytes_selected = 0u64;
    for (sent_so_far, idx) in missing.iter().enumerate() {
        if abort_after_chunks.is_some_and(|limit| sent_so_far as u32 >= limit) {
            stream.flush()?;
            let _ = stream.inner().shutdown(std::net::Shutdown::Both);
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                format!("injected mid-transfer disconnect after {sent_so_far} chunks"),
            ));
        }
        let len = expected_chunk_len(size, *idx)?;
        let mut chunk = vec![0u8; len];
        file.seek(SeekFrom::Start(*idx as u64 * CHUNK_BYTES as u64))?;
        file.read_exact(&mut chunk)?;
        if digest(&chunk) != hashes[*idx as usize] {
            return Err(invalid("input file changed while syncing"));
        }
        let (codec, encoded) = encode_chunk(&chunk);
        write_u32(&mut stream, *idx)?;
        write_u8(&mut stream, codec)?;
        write_u32(&mut stream, encoded.len() as u32)?;
        stream.write_all(&encoded)?;
        wire_payload_bytes += encoded.len() as u64;
        raw_bytes_selected += chunk.len() as u64;
    }
    stream.flush()?;
    let status = read_u8(&mut stream)?;
    if status != 0 {
        return Err(invalid("server failed to materialize output"));
    }
    let transfer_us = t_transfer.elapsed().as_micros() as u64;
    let elapsed = started.elapsed();

    let breakdown = WireBreakdown::compute(name.len(), count, missing_len, wire_payload_bytes);
    let wire_bytes_tx = stream.bytes_tx();
    let wire_bytes_rx = stream.bytes_rx();
    let wire_bytes_total = stream.bytes_total();
    debug_assert_eq!(wire_bytes_tx, breakdown.expected_wire_bytes_tx);
    debug_assert_eq!(wire_bytes_rx, breakdown.expected_wire_bytes_rx);
    debug_assert_eq!(wire_bytes_total, breakdown.expected_wire_bytes_total);

    let metrics = TransferMetrics {
        file_bytes: size,
        chunks_total: count,
        chunks_reused: count - missing_len,
        chunks_transferred: missing_len,
        raw_missing_bytes: raw_bytes_selected,
        encoded_payload_bytes: wire_payload_bytes,
        handshake_wire_bytes: breakdown.handshake_wire_bytes,
        manifest_wire_bytes: breakdown.manifest_wire_bytes,
        need_wire_bytes: breakdown.need_wire_bytes,
        chunk_framing_wire_bytes: breakdown.chunk_framing_wire_bytes,
        status_wire_bytes: breakdown.status_wire_bytes,
        headers_and_framing_wire_bytes: breakdown.headers_and_framing_wire_bytes,
        wire_bytes_tx,
        wire_bytes_rx,
        wire_bytes_total,
        handshake_us,
        manifest_us,
        transfer_us,
        elapsed_us: elapsed.as_micros() as u64,
        elapsed_ms: elapsed.as_millis() as u64,
        wire_mib_per_s: mib_per_sec(wire_bytes_total, elapsed),
        logical_mib_per_s: mib_per_sec(size, elapsed),
    };
    Ok((filename.to_string(), metrics))
}

/// Send a file to `server`, automatically resuming by manifest on transient connection drops
/// without re-sending already verified chunks.
pub fn send_file_resumable(
    file_path: &Path,
    server: &str,
    options: SendOptions,
) -> io::Result<(String, TransferMetrics)> {
    let mut attempt = 0u32;
    loop {
        let abort_limit = if attempt == 0 {
            options.abort_after_chunks
        } else {
            None
        };
        match send_file_once_with_options(file_path, server, abort_limit) {
            Ok(res) => return Ok(res),
            Err(err) => {
                let retryable = matches!(
                    err.kind(),
                    io::ErrorKind::ConnectionAborted
                        | io::ErrorKind::ConnectionReset
                        | io::ErrorKind::BrokenPipe
                        | io::ErrorKind::UnexpectedEof
                        | io::ErrorKind::TimedOut
                );
                if !retryable || attempt >= options.max_resume_retries {
                    return Err(err);
                }
                attempt += 1;
                thread::sleep(Duration::from_millis(15));
            }
        }
    }
}

/// Send a file once and return `(filename, TransferMetrics)`.
pub fn send_file_with_metrics(
    file_path: &Path,
    server: &str,
) -> io::Result<(String, TransferMetrics)> {
    send_file_once_with_options(file_path, server, None)
}

#[cfg(test)]
mod reliability_tests {
    use super::{cached_and_valid, digest_path, store_chunk_isolated, ServerCoord};
    use crate::digest;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_root() -> std::path::PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("fastlink-srv-{}-{}", std::process::id(), id));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("chunks")).unwrap();
        fs::create_dir_all(root.join("outputs")).unwrap();
        root
    }

    #[test]
    fn store_chunk_isolated_rejects_misaddressed_bytes() {
        let root = temp_root();
        let bytes = vec![1u8; 8];
        let coord = ServerCoord::new();
        let err = store_chunk_isolated(&root, &digest(b"other"), &bytes, 1, &coord).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn oversized_cache_object_is_not_a_hit() {
        let root = temp_root();
        let chunk = vec![1u8; 32];
        let hash = digest(&chunk);
        let path = digest_path(&root, &hash);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, vec![1u8; crate::CHUNK_BYTES + 8]).unwrap();
        assert!(!cached_and_valid(&path, &hash));
        fs::remove_dir_all(&root).ok();
    }
}
