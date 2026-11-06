//! FASTLINK protocol integration tests over a real loopback TCP connection.
//!
//! Covers the milestone requirements: cold and warm transfer, single-chunk
//! delta, malformed frame, invalid filename, corrupt chunk digest, truncated
//! transfer, and guaranteeing a failed transfer never replaces a previously
//! completed output. All peers bind/connect to `127.0.0.1` only.
//!
//! Negative-path tests assert a *contract* rather than a socket error: when
//! the server aborts it closes the connection, which a peer reads as EOF
//! (`Ok(0)`), not an `io::Error`. So each failing test (a) reads until EOF to
//! confirm the session did not report `STATUS_OK`, and (b) joins the server
//! thread before asserting that no output was committed (or that a previously
//! committed output was preserved).

use fastlink::{
    chunk_count, client_sync, digest, encode_chunk, handshake, server_sync,
    validate_connect_address, validate_listen_address, write_u16, write_u32, write_u64, write_u8,
    CHUNK_BYTES, STATUS_OK, VERSION,
};
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

static ROOT_SEQ: AtomicUsize = AtomicUsize::new(0);

fn fresh_root() -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "fastlink-integration-{}-{}",
        std::process::id(),
        ROOT_SEQ.fetch_add(1, Ordering::Relaxed) + 1
    ));
    fs::create_dir_all(root.join("chunks")).unwrap();
    fs::create_dir_all(root.join("outputs")).unwrap();
    root
}

fn cleanup(root: &Path) {
    let _ = fs::remove_dir_all(root);
}

/// A loopback server running one `server_sync` session on a joinable thread.
/// Call [`LoopbackServer::finish`] to join the thread and obtain the data root,
/// so the test never races the server when inspecting the filesystem.
struct LoopbackServer {
    handle: JoinHandle<()>,
    root: PathBuf,
}

impl LoopbackServer {
    fn start(root: &Path) -> (TcpStream, Self) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let root_for_thread = root.to_path_buf();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            if let Err(e) = server_sync(&mut stream, &root_for_thread) {
                eprintln!("integration server_sync: {e}");
            }
        });
        let stream = TcpStream::connect(addr).unwrap();
        // A finite timeout converts a protocol deadlock into a test error
        // instead of a hang (the real CLI applies the same kind of guard).
        stream
            .set_read_timeout(Some(Duration::from_secs(15)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(15)))
            .unwrap();
        (
            stream,
            LoopbackServer {
                handle,
                root: root.to_path_buf(),
            },
        )
    }

    /// Join the server thread and return the data root. After this returns, the
    /// session's I/O has fully completed, so filesystem assertions are stable.
    fn finish(self) -> PathBuf {
        let _ = self.handle.join();
        self.root
    }
}

/// Read one byte from a session socket. Returns `None` if the peer has closed
/// the connection or returned no byte (i.e. the session did not complete with
/// a status). Returns `Some(b)` only when a status byte was actually delivered.
fn read_status(stream: &mut TcpStream) -> Option<u8> {
    let mut buf = [0u8; 1];
    match stream.read(&mut buf) {
        Ok(0) => None,
        Ok(_n) => Some(buf[0]),
        Err(_) => None,
    }
}

/// Run the full versioned handshake (send FLNK+VERSION and consume the reply).
fn do_handshake(stream: &mut TcpStream) {
    handshake(stream).expect("handshake with loopback server");
}

/// Consume the server's "missing count + indices" reply, returning the count.
fn read_missing(stream: &mut TcpStream) -> u32 {
    let mut b = [0u8; 4];
    stream.read_exact(&mut b).unwrap();
    let count = u32::from_le_bytes(b);
    for _ in 0..count {
        stream.read_exact(&mut b).unwrap();
    }
    count
}

/// Pre-store one verified chunk in the content-addressed cache under `root`.
fn prestore(root: &Path, chunk: &[u8]) {
    let hash = digest(chunk);
    let s = fastlink::hex(&hash);
    let dir = root.join("chunks").join(&s[..2]);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join(&s), chunk).unwrap();
}

/// A loopback server that accepts `n` concurrent SYNC sessions on one
/// listener, mirroring the real CLI's thread-per-connection accept loop.
/// `finish` joins the dispatcher *and* every per-session worker, so the
/// filesystem is fully settled before assertions run.
struct ConcurrentServer {
    handle: JoinHandle<()>,
    root: PathBuf,
}

impl ConcurrentServer {
    fn start(root: &Path, n: usize) -> (Vec<TcpStream>, Self) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let root_for_thread = root.to_path_buf();
        let handle = std::thread::spawn(move || {
            let mut workers = Vec::new();
            for _ in 0..n {
                if let Ok((stream, _)) = listener.accept() {
                    let rt = root_for_thread.clone();
                    let w = std::thread::spawn(move || {
                        let mut s = stream;
                        if let Err(e) = server_sync(&mut s, &rt) {
                            eprintln!("concurrent server_sync: {e}");
                        }
                    });
                    workers.push(w);
                }
            }
            for w in workers {
                let _ = w.join();
            }
        });
        let streams = (0..n)
            .map(|_| {
                let s = TcpStream::connect(addr).unwrap();
                s.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
                s.set_write_timeout(Some(Duration::from_secs(15))).unwrap();
                s
            })
            .collect();
        (
            streams,
            ConcurrentServer {
                handle,
                root: root.to_path_buf(),
            },
        )
    }

    fn finish(self) -> PathBuf {
        let _ = self.handle.join();
        self.root
    }
}

#[test]
fn cold_transfer_over_tcp() {
    let root = fresh_root();
    let work = fresh_root();
    let name = "cold.bin";
    let chunk_a = vec![0u8; CHUNK_BYTES];
    let chunk_b = vec![255u8; CHUNK_BYTES];
    let file_bytes: Vec<u8> = [chunk_a, chunk_b].concat();
    let source = work.join(name);
    fs::write(&source, &file_bytes).unwrap();

    let (mut stream, server) = LoopbackServer::start(&root);
    let stats = client_sync(&mut stream, name, &source).unwrap();

    let root = server.finish();
    let output = root.join("outputs").join(name);
    assert_eq!(fs::read(&output).unwrap(), file_bytes);
    assert_eq!(stats.chunks_total, 2);
    assert_eq!(stats.chunks_transferred, 2);
    assert_eq!(stats.chunks_reused, 0);
    assert_eq!(stats.raw_missing_bytes, file_bytes.len() as u64);
    assert!(stats.encoded_payload_bytes > 0);
    cleanup(&root);
    cleanup(&work);
}

#[test]
fn warm_transfer_reuses_cache() {
    let root = fresh_root();
    let work = fresh_root();
    let name = "warm.bin";
    let mut first_chunk = vec![0u8; CHUNK_BYTES];
    first_chunk[10] = 1;
    first_chunk[20] = 2;
    let mut second_chunk = vec![0u8; CHUNK_BYTES];
    second_chunk[5] = 9;
    let full: Vec<u8> = [first_chunk, second_chunk].concat();
    let source = work.join(name);
    fs::write(&source, &full).unwrap();

    // First (cold) transfer populates the cache.
    let (mut s1, server1) = LoopbackServer::start(&root);
    let first = client_sync(&mut s1, name, &source).unwrap();
    assert_eq!(first.chunks_transferred, 2);
    drop(server1.finish());

    // Second (warm) transfer must reuse both chunks and move none.
    let (mut s2, server2) = LoopbackServer::start(&root);
    let second_stats = client_sync(&mut s2, name, &source).unwrap();
    let root = server2.finish();
    assert_eq!(fs::read(root.join("outputs").join(name)).unwrap(), full);
    assert_eq!(second_stats.chunks_reused, 2);
    assert_eq!(second_stats.chunks_transferred, 0);
    assert_eq!(second_stats.raw_missing_bytes, 0);
    assert_eq!(second_stats.encoded_payload_bytes, 0);
    assert_eq!(second_stats.file_bytes, (2 * CHUNK_BYTES) as u64);
    cleanup(&root);
    cleanup(&work);
}

#[test]
fn single_chunk_delta_only_transfers_missing() {
    let root = fresh_root();
    let work = fresh_root();
    let name = "delta.bin";
    let chunk_a = vec![0u8; CHUNK_BYTES];
    let chunk_b = vec![7u8; CHUNK_BYTES];
    let file_bytes: Vec<u8> = [chunk_a.clone(), chunk_b].concat();
    let source = work.join(name);
    fs::write(&source, &file_bytes).unwrap();

    // Only chunk 0 is already cached; chunk 1 must be the sole transfer.
    prestore(&root, &chunk_a);
    let (mut stream, server) = LoopbackServer::start(&root);
    let stats = client_sync(&mut stream, name, &source).unwrap();
    let root = server.finish();
    assert_eq!(
        fs::read(root.join("outputs").join(name)).unwrap(),
        file_bytes
    );
    assert_eq!(stats.chunks_total, 2);
    assert_eq!(stats.chunks_reused, 1);
    assert_eq!(stats.chunks_transferred, 1);
    assert_eq!(stats.raw_missing_bytes, CHUNK_BYTES as u64);
    cleanup(&root);
    cleanup(&work);
}

#[test]
fn corrupt_chunk_digest_is_rejected_and_no_output() {
    let root = fresh_root();
    let name = "corrupt.bin";
    let sent = vec![0u8; CHUNK_BYTES]; // actual content
    let claimed_hash = digest(&[1u8; CHUNK_BYTES]); // different content

    // Hand-craft a manifest that claims `claimed_hash` for chunk 0.
    let mut manifest = Vec::new();
    write_u16(&mut manifest, name.len() as u16).unwrap();
    manifest.extend_from_slice(name.as_bytes());
    write_u64(&mut manifest, CHUNK_BYTES as u64).unwrap();
    write_u32(&mut manifest, CHUNK_BYTES as u32).unwrap();
    write_u32(&mut manifest, 1).unwrap(); // 1 chunk
    manifest.extend_from_slice(&claimed_hash);

    let (mut stream, server) = LoopbackServer::start(&root);
    do_handshake(&mut stream);
    write_u8(&mut stream, 1).unwrap(); // CMD_SYNC
    stream.write_all(&manifest).unwrap();
    let missing = read_missing(&mut stream);
    assert_eq!(missing, 1, "server should request the one missing chunk");

    // Send the chunk with correct framing but content that does not match.
    let (codec, encoded) = encode_chunk(&sent);
    write_u32(&mut stream, 0).unwrap();
    write_u8(&mut stream, codec).unwrap();
    write_u32(&mut stream, encoded.len() as u32).unwrap();
    stream.write_all(&encoded).unwrap();

    // The session must not complete with STATUS_OK.
    let status = read_status(&mut stream);
    assert!(
        status.is_none_or(|s| s != STATUS_OK),
        "server must not report success for a corrupt chunk (got {:?})",
        status
    );
    let root = server.finish();
    assert!(
        !root.join("outputs").join(name).exists(),
        "no output may be committed for a corrupt chunk"
    );
    cleanup(&root);
}

#[test]
fn truncated_transfer_leaves_no_output() {
    let root = fresh_root();
    let work = fresh_root();
    let name = "trunc.bin";
    let chunk_a = vec![0u8; CHUNK_BYTES];
    let chunk_b = vec![9u8; CHUNK_BYTES];
    let file_bytes: Vec<u8> = [chunk_a.clone(), chunk_b.clone()].concat();
    let source = work.join(name);
    fs::write(&source, &file_bytes).unwrap();

    // Pre-store chunk 0 so only chunk 1 is "missing"; then drop the
    // connection before sending chunk 1.
    prestore(&root, &chunk_a);
    let (mut stream, server) = LoopbackServer::start(&root);
    do_handshake(&mut stream);
    write_u8(&mut stream, 1).unwrap(); // CMD_SYNC
    let size = file_bytes.len() as u64;
    let count = chunk_count(size).unwrap();
    write_u16(&mut stream, name.len() as u16).unwrap();
    stream.write_all(name.as_bytes()).unwrap();
    write_u64(&mut stream, size).unwrap();
    write_u32(&mut stream, CHUNK_BYTES as u32).unwrap();
    write_u32(&mut stream, count).unwrap();
    stream.write_all(&digest(&chunk_a)).unwrap();
    stream.write_all(&digest(&chunk_b)).unwrap();
    let missing = read_missing(&mut stream);
    assert_eq!(missing, 1, "server should request only chunk 1");

    // Do NOT send chunk 1; just close. The server must abort and commit nothing.
    drop(stream);
    let root = server.finish();
    assert!(
        !root.join("outputs").join(name).exists(),
        "no partial output may be committed for a truncated transfer"
    );
    cleanup(&root);
    cleanup(&work);
}

#[test]
fn malformed_manifest_frame_is_rejected() {
    let root = fresh_root();
    let name = "bad.bin";
    // A manifest whose declared chunk size is inconsistent with the constant.
    let mut manifest = Vec::new();
    write_u16(&mut manifest, name.len() as u16).unwrap();
    manifest.extend_from_slice(name.as_bytes());
    write_u64(&mut manifest, CHUNK_BYTES as u64).unwrap();
    write_u32(&mut manifest, 4096).unwrap(); // wrong chunk size
    write_u32(&mut manifest, 1).unwrap();
    let (mut stream, server) = LoopbackServer::start(&root);
    write_u8(&mut stream, 1).unwrap(); // CMD_SYNC
    stream.write_all(&manifest).unwrap();
    // The server must reject the manifest (EOF), and commit nothing.
    let status = read_status(&mut stream);
    assert!(
        status.is_none_or(|s| s != STATUS_OK),
        "server must not accept a malformed manifest"
    );
    let root = server.finish();
    assert!(
        !root.join("outputs").join(name).exists(),
        "no output may be committed for a malformed manifest"
    );
    cleanup(&root);
}

#[test]
fn invalid_filename_is_rejected_before_any_output() {
    let root = fresh_root();
    let work = fresh_root();
    let source = work.join("real.bin");
    fs::write(&source, vec![0u8; 16]).unwrap();
    let (mut stream, server) = LoopbackServer::start(&root);
    let res = client_sync(&mut stream, "../../evil", &source);
    assert!(
        res.is_err(),
        "traversal filename must be rejected client-side"
    );
    drop(stream);
    let root = server.finish();
    let outputs = root.join("outputs");
    if outputs.exists() {
        let entries: Vec<_> = fs::read_dir(&outputs)
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert!(
            entries.is_empty(),
            "no output may be created for a rejected name"
        );
    }
    cleanup(&root);
    cleanup(&work);
}

#[test]
fn handshake_magic_mismatch_is_rejected() {
    let root = fresh_root();
    let (mut stream, server) = LoopbackServer::start(&root);
    stream.write_all(b"XXXX").unwrap(); // bad magic
    write_u16(&mut stream, VERSION).unwrap();
    stream.flush().unwrap();
    let status = read_status(&mut stream);
    assert!(
        status.is_none_or(|s| s != STATUS_OK),
        "server must reject a bad handshake magic"
    );
    drop(stream);
    let root = server.finish();
    let outputs = root.join("outputs");
    assert!(
        !outputs.exists() || fs::read_dir(&outputs).unwrap().next().is_none(),
        "no output may exist after a rejected handshake"
    );
    cleanup(&root);
}

#[test]
fn loopback_only_guards() {
    assert!(validate_listen_address("127.0.0.1:0").is_ok());
    assert!(validate_listen_address("127.0.0.1:18777").is_ok());
    assert!(validate_listen_address("0.0.0.0:18777").is_err());
    assert!(validate_listen_address("192.168.1.5:18777").is_err());
    assert!(validate_listen_address("not-an-address").is_err());
    assert!(validate_connect_address("127.0.0.1:18777").is_ok());
    assert!(validate_connect_address("0.0.0.0:18777").is_err());
    assert!(validate_connect_address("example.com:443").is_err());
    assert!(validate_connect_address("not-an-address").is_err());
}

#[test]
fn failed_transfer_never_replaces_prior_output() {
    let root = fresh_root();
    let work = fresh_root();
    // A name that the old `with_extension("fastlink-partial")` code path made
    // collide with the in-progress file. A failed transfer must now leave the
    // previously committed output intact.
    let name = "x.fastlink-partial";
    let chunk0 = vec![0u8; CHUNK_BYTES];
    let chunk1 = vec![7u8; CHUNK_BYTES];
    let good: Vec<u8> = [chunk0.clone(), chunk1].concat();
    let source = work.join(name);
    fs::write(&source, &good).unwrap();

    // 1) Commit a good copy via a successful cold transfer.
    let (mut s1, server1) = LoopbackServer::start(&root);
    client_sync(&mut s1, name, &source).unwrap();
    drop(server1.finish());
    let output = root.join("outputs").join(name);
    assert_eq!(fs::read(&output).unwrap(), good);

    // 2) Attempt a failing transfer of the same name: chunk 0 is already
    // cached, but the one chunk the server asks for will not match the
    // manifest's claimed digest, so the session aborts before materialization.
    let bad_chunk1 = vec![9u8; CHUNK_BYTES];
    let bad: Vec<u8> = [chunk0.clone(), bad_chunk1.clone()].concat();
    let _bad_source = work.join("bad.bin");
    fs::write(&_bad_source, &bad).unwrap();
    prestore(&root, &chunk0); // so only index 1 is "missing"

    let (mut stream, server) = LoopbackServer::start(&root);
    do_handshake(&mut stream);
    let size = bad.len() as u64;
    let count = chunk_count(size).unwrap();
    write_u8(&mut stream, 1).unwrap(); // CMD_SYNC
    write_u16(&mut stream, name.len() as u16).unwrap();
    stream.write_all(name.as_bytes()).unwrap();
    write_u64(&mut stream, size).unwrap();
    write_u32(&mut stream, CHUNK_BYTES as u32).unwrap();
    write_u32(&mut stream, count).unwrap();
    stream.write_all(&digest(&chunk0)).unwrap();
    stream.write_all(&digest(&bad_chunk1)).unwrap(); // claim for index 1
    let missing = read_missing(&mut stream);
    assert_eq!(missing, 1, "server should request only index 1");

    // Send a chunk whose content does not match the claimed hash -> reject.
    let (codec, encoded) = encode_chunk(&[3u8; CHUNK_BYTES]);
    write_u32(&mut stream, 1).unwrap();
    write_u8(&mut stream, codec).unwrap();
    write_u32(&mut stream, encoded.len() as u32).unwrap();
    stream.write_all(&encoded).unwrap();

    let status = read_status(&mut stream);
    assert!(
        status.is_none_or(|s| s != STATUS_OK),
        "server must abort a corrupt transfer (got {:?})",
        status
    );
    drop(stream);
    let root = server.finish();

    // The previously committed output must be untouched.
    assert_eq!(
        fs::read(root.join("outputs").join(name)).unwrap(),
        good,
        "a failed transfer must not replace a previously completed output"
    );
    cleanup(&root);
    cleanup(&work);
}

#[test]
fn zero_byte_file_transfers_and_reconstructs() {
    let root = fresh_root();
    let work = fresh_root();
    let name = "empty.bin";
    let source = work.join(name);
    fs::write(&source, b"").unwrap(); // 0 bytes

    let (mut stream, server) = LoopbackServer::start(&root);
    let stats = client_sync(&mut stream, name, &source).unwrap();
    let root = server.finish();

    let output = root.join("outputs").join(name);
    let received = fs::read(&output).unwrap();
    assert!(
        received.is_empty(),
        "zero-byte file must reconstruct as 0 bytes"
    );
    assert_eq!(stats.chunks_total, 0);
    assert_eq!(stats.chunks_transferred, 0);
    assert_eq!(stats.raw_missing_bytes, 0);
    // Byte-exact comparison is the integrity check (stronger than any hash).
    assert_eq!(received, b"");
    cleanup(&root);
    cleanup(&work);
}

#[test]
fn concurrent_transfers_all_byte_identical() {
    let root = fresh_root();
    let work = fresh_root();
    // Each client sends a *distinct* 2-chunk file; all share the same data
    // root, exercising the thread-per-connection accept loop and the shared
    // content-addressed cache under concurrent writes.
    let clients = 5usize;
    let names: Vec<String> = (0..clients).map(|i| format!("c{}.bin", i)).collect();
    let expected: Vec<Vec<u8>> = (0..clients)
        .map(|i| {
            let mut a = vec![0u8; CHUNK_BYTES];
            let mut b = vec![255u8; CHUNK_BYTES];
            a[i % 256] = (i * 3 + 1) as u8;
            b[i % 256] = (i * 5 + 2) as u8;
            let src = work.join(&names[i]);
            fs::write(&src, [a.clone(), b.clone()].concat()).unwrap();
            [a, b].concat()
        })
        .collect();

    let (streams, server) = ConcurrentServer::start(&root, clients);
    let mut handles = Vec::new();
    for (mut stream, name) in streams.into_iter().zip(names.iter().cloned()) {
        let src = work.join(&name);
        handles.push(std::thread::spawn(move || {
            client_sync(&mut stream, &name, &src).expect("concurrent client_sync must succeed");
        }));
    }
    for h in handles {
        h.join().expect("client thread must not panic");
    }
    let root = server.finish();
    for (name, expected_bytes) in names.iter().zip(expected.iter()) {
        let received = fs::read(root.join("outputs").join(name)).unwrap();
        assert_eq!(
            received, *expected_bytes,
            "concurrent output {name} mismatch"
        );
    }
    cleanup(&root);
    cleanup(&work);
}

#[test]
fn duplicate_chunks_transferred_once() {
    let root = fresh_root();
    let work = fresh_root();
    let name = "dup.bin";
    // Two identical 64 KiB chunks: the file has 2 chunks but only ONE unique
    // content hash, so the server must request it once and store it once.
    let chunk = vec![42u8; CHUNK_BYTES];
    let file_bytes: Vec<u8> = [chunk.clone(), chunk].concat();
    let source = work.join(name);
    fs::write(&source, &file_bytes).unwrap();

    let (mut stream, server) = LoopbackServer::start(&root);
    let stats = client_sync(&mut stream, name, &source).unwrap();
    let root = server.finish();

    assert_eq!(
        fs::read(root.join("outputs").join(name)).unwrap(),
        file_bytes
    );
    assert_eq!(stats.chunks_total, 2, "file has two 64 KiB chunks");
    assert_eq!(
        stats.chunks_transferred, 1,
        "a duplicated hash must be transferred only once"
    );
    assert_eq!(stats.chunks_reused, 1);
    // Exactly one cached chunk must exist (content-addressed dedup).
    let chunk_files: Vec<PathBuf> = fs::read_dir(root.join("chunks"))
        .unwrap()
        .filter_map(|e| e.ok())
        .flat_map(|e| {
            fs::read_dir(e.path()).ok().map(|sub| {
                sub.filter_map(|e2| e2.ok())
                    .map(|e2| e2.path())
                    .collect::<Vec<_>>()
            })
        })
        .flatten()
        .collect();
    assert_eq!(
        chunk_files.len(),
        1,
        "exactly one unique chunk should be cached"
    );
    cleanup(&root);
    cleanup(&work);
}

#[test]
fn mid_chunk_disconnect_leaves_no_output() {
    let root = fresh_root();
    let name = "mid.bin";
    let chunk0 = vec![0u8; CHUNK_BYTES];
    let chunk1 = vec![7u8; CHUNK_BYTES];
    // 2-chunk file; chunk 0 already cached so the server requests chunk 1.
    prestore(&root, &chunk0);

    let (mut stream, server) = LoopbackServer::start(&root);
    do_handshake(&mut stream);
    write_u8(&mut stream, 1).unwrap(); // CMD_SYNC
    write_u16(&mut stream, name.len() as u16).unwrap();
    stream.write_all(name.as_bytes()).unwrap();
    write_u64(&mut stream, (2 * CHUNK_BYTES) as u64).unwrap();
    write_u32(&mut stream, CHUNK_BYTES as u32).unwrap();
    write_u32(&mut stream, 2).unwrap(); // 2 chunks
    stream.write_all(&digest(&chunk0)).unwrap();
    stream.write_all(&digest(&chunk1)).unwrap();
    let missing = read_missing(&mut stream);
    assert_eq!(missing, 1, "server should request chunk 1");

    // Begin chunk 1's framing, then drop mid-payload (send only half of it).
    let (codec, encoded) = encode_chunk(&chunk1);
    write_u32(&mut stream, 1).unwrap();
    write_u8(&mut stream, codec).unwrap();
    write_u32(&mut stream, encoded.len() as u32).unwrap();
    stream.write_all(&encoded[..encoded.len() / 2]).unwrap();
    drop(stream); // abrupt close mid-chunk

    let root = server.finish();
    assert!(
        !root.join("outputs").join(name).exists(),
        "a mid-chunk disconnect must not commit any output"
    );
    cleanup(&root);
}
