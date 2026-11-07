//! FASTLINK protocol integration tests over a real loopback TCP connection.
//!
//! Covers cold and warm transfer, single-chunk delta, malformed frames,
//! invalid filenames, corrupt chunk digests, truncated transfers, and the
//! guarantee that a failed transfer never replaces a previously completed
//! output. All peers bind and connect to `127.0.0.1` only.
//!
//! Negative-path tests speak v0.1 to `spawn_server`. When the server aborts it
//! closes the connection, which a peer reads as EOF, not `STATUS_OK`. Each
//! failing test reads until EOF and waits until the session worker has left
//! `active_sessions` before inspecting the filesystem.

use fastlink::{
    chunk_count, client_handshake, digest, encode_chunk, safe_basename, send_file_with_metrics,
    spawn_server, validate_connect_address, validate_listen_address, write_u16, write_u32,
    write_u64, write_u8, RunningServer, ServerConfig, CHUNK_BYTES, CMD_SYNC, VERSION,
};
use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const STATUS_OK: u8 = 0;

static ROOT_SEQ: AtomicUsize = AtomicUsize::new(0);

fn fresh_root() -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "fastlink-integration-{}-{}",
        std::process::id(),
        ROOT_SEQ.fetch_add(1, Ordering::Relaxed) + 1
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("chunks")).unwrap();
    fs::create_dir_all(root.join("outputs")).unwrap();
    root
}

fn cleanup(root: &Path) {
    let _ = fs::remove_dir_all(root);
}

fn test_config() -> ServerConfig {
    ServerConfig {
        io_timeout: Duration::from_secs(15),
        ..ServerConfig::default()
    }
}

/// A loopback `spawn_server`. `finish` waits until the session worker is idle,
/// then shuts the server down so filesystem assertions are stable.
struct LoopbackServer {
    running: RunningServer,
    root: PathBuf,
}

impl LoopbackServer {
    fn start(root: &Path) -> (String, Self) {
        Self::start_with(root, test_config())
    }

    fn start_with(root: &Path, config: ServerConfig) -> (String, Self) {
        let running = spawn_server("127.0.0.1:0", root, config).expect("loopback bind");
        let addr = running.addr_string();
        (
            addr,
            LoopbackServer {
                running,
                root: root.to_path_buf(),
            },
        )
    }

    fn connect(addr: &str) -> TcpStream {
        let stream = TcpStream::connect(addr).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(15)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(15)))
            .unwrap();
        stream
    }

    fn finish(self) -> PathBuf {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            let done = self.running.coord().total_sessions_completed()
                + self.running.coord().total_sessions_aborted();
            if done >= 1 && self.running.coord().active_sessions() == 0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let root = self.root.clone();
        self.running.shutdown();
        root
    }
}

/// Read one byte from a session socket. Returns `None` if the peer has closed
/// the connection or returned no byte.
fn read_status(stream: &mut TcpStream) -> Option<u8> {
    let mut buf = [0u8; 1];
    match stream.read(&mut buf) {
        Ok(0) => None,
        Ok(_) => Some(buf[0]),
        Err(_) => None,
    }
}

fn do_handshake(stream: &mut TcpStream) {
    client_handshake(stream).expect("handshake with loopback server");
}

fn read_missing(stream: &mut TcpStream) -> u32 {
    let mut b = [0u8; 4];
    stream.read_exact(&mut b).unwrap();
    let count = u32::from_le_bytes(b);
    for _ in 0..count {
        stream.read_exact(&mut b).unwrap();
    }
    count
}

fn prestore(root: &Path, chunk: &[u8]) {
    let hash = digest(chunk);
    let s = fastlink::hex(&hash);
    let dir = root.join("chunks").join(&s[..2]);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join(&s), chunk).unwrap();
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

    let (addr, server) = LoopbackServer::start(&root);
    let (_, stats) = send_file_with_metrics(&source, &addr).unwrap();

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

    let (addr, server1) = LoopbackServer::start(&root);
    let (_, first) = send_file_with_metrics(&source, &addr).unwrap();
    assert_eq!(first.chunks_transferred, 2);
    drop(server1.finish());

    let (addr, server2) = LoopbackServer::start(&root);
    let (_, second_stats) = send_file_with_metrics(&source, &addr).unwrap();
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

    prestore(&root, &chunk_a);
    let (addr, server) = LoopbackServer::start(&root);
    let (_, stats) = send_file_with_metrics(&source, &addr).unwrap();
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
    let sent = vec![0u8; CHUNK_BYTES];
    let claimed_hash = digest(&[1u8; CHUNK_BYTES]);

    let (addr, server) = LoopbackServer::start(&root);
    let mut stream = LoopbackServer::connect(&addr);
    do_handshake(&mut stream);
    write_u8(&mut stream, CMD_SYNC).unwrap();
    write_u16(&mut stream, name.len() as u16).unwrap();
    stream.write_all(name.as_bytes()).unwrap();
    write_u64(&mut stream, CHUNK_BYTES as u64).unwrap();
    write_u32(&mut stream, CHUNK_BYTES as u32).unwrap();
    write_u32(&mut stream, 1).unwrap();
    stream.write_all(&claimed_hash).unwrap();
    let missing = read_missing(&mut stream);
    assert_eq!(missing, 1, "server should request the one missing chunk");

    let (codec, encoded) = encode_chunk(&sent);
    write_u32(&mut stream, 0).unwrap();
    write_u8(&mut stream, codec).unwrap();
    write_u32(&mut stream, encoded.len() as u32).unwrap();
    stream.write_all(&encoded).unwrap();

    let status = read_status(&mut stream);
    assert!(
        status.is_none_or(|s| s != STATUS_OK),
        "server must not report success for a corrupt chunk (got {status:?})"
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

    prestore(&root, &chunk_a);
    let (addr, server) = LoopbackServer::start(&root);
    let mut stream = LoopbackServer::connect(&addr);
    do_handshake(&mut stream);
    write_u8(&mut stream, CMD_SYNC).unwrap();
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
    let (addr, server) = LoopbackServer::start(&root);
    let mut stream = LoopbackServer::connect(&addr);
    do_handshake(&mut stream);
    write_u8(&mut stream, CMD_SYNC).unwrap();
    write_u16(&mut stream, name.len() as u16).unwrap();
    stream.write_all(name.as_bytes()).unwrap();
    write_u64(&mut stream, CHUNK_BYTES as u64).unwrap();
    write_u32(&mut stream, 4096).unwrap();
    write_u32(&mut stream, 1).unwrap();
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
    assert!(safe_basename("../../evil").is_err());
    assert!(safe_basename("..").is_err());
    assert!(safe_basename("").is_err());

    let root = fresh_root();
    let work = fresh_root();
    let err = send_file_with_metrics(&work.join(".."), "127.0.0.1:9").unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);

    let name = "../../evil";
    let (addr, server) = LoopbackServer::start(&root);
    let mut stream = LoopbackServer::connect(&addr);
    do_handshake(&mut stream);
    write_u8(&mut stream, CMD_SYNC).unwrap();
    write_u16(&mut stream, name.len() as u16).unwrap();
    stream.write_all(name.as_bytes()).unwrap();
    let status = read_status(&mut stream);
    assert!(
        status.is_none_or(|s| s != STATUS_OK),
        "server must reject a traversal filename"
    );
    let root = server.finish();
    let outputs = root.join("outputs");
    let entries: Vec<_> = fs::read_dir(&outputs)
        .unwrap()
        .filter_map(|e| e.ok())
        .collect();
    assert!(
        entries.is_empty(),
        "no output may be created for a rejected name"
    );
    cleanup(&root);
    cleanup(&work);
}

#[test]
fn handshake_magic_mismatch_is_rejected() {
    let root = fresh_root();
    let (addr, server) = LoopbackServer::start(&root);
    let mut stream = LoopbackServer::connect(&addr);
    stream.write_all(b"XXXX").unwrap();
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
    let root = fresh_root();
    assert!(spawn_server("0.0.0.0:9", &root, test_config()).is_err());
    cleanup(&root);
}

#[test]
fn failed_transfer_never_replaces_prior_output() {
    let root = fresh_root();
    let work = fresh_root();
    let name = "x.fastlink-partial";
    let chunk0 = vec![0u8; CHUNK_BYTES];
    let chunk1 = vec![7u8; CHUNK_BYTES];
    let good: Vec<u8> = [chunk0.clone(), chunk1].concat();
    let source = work.join(name);
    fs::write(&source, &good).unwrap();

    let (addr, server1) = LoopbackServer::start(&root);
    send_file_with_metrics(&source, &addr).unwrap();
    drop(server1.finish());
    let output = root.join("outputs").join(name);
    assert_eq!(fs::read(&output).unwrap(), good);

    let bad_chunk1 = vec![9u8; CHUNK_BYTES];
    prestore(&root, &chunk0);

    let (addr, server) = LoopbackServer::start(&root);
    let mut stream = LoopbackServer::connect(&addr);
    do_handshake(&mut stream);
    let size = (2 * CHUNK_BYTES) as u64;
    let count = chunk_count(size).unwrap();
    write_u8(&mut stream, CMD_SYNC).unwrap();
    write_u16(&mut stream, name.len() as u16).unwrap();
    stream.write_all(name.as_bytes()).unwrap();
    write_u64(&mut stream, size).unwrap();
    write_u32(&mut stream, CHUNK_BYTES as u32).unwrap();
    write_u32(&mut stream, count).unwrap();
    stream.write_all(&digest(&chunk0)).unwrap();
    stream.write_all(&digest(&bad_chunk1)).unwrap();
    let missing = read_missing(&mut stream);
    assert_eq!(missing, 1, "server should request only index 1");

    let (codec, encoded) = encode_chunk(&[3u8; CHUNK_BYTES]);
    write_u32(&mut stream, 1).unwrap();
    write_u8(&mut stream, codec).unwrap();
    write_u32(&mut stream, encoded.len() as u32).unwrap();
    stream.write_all(&encoded).unwrap();

    let status = read_status(&mut stream);
    assert!(
        status.is_none_or(|s| s != STATUS_OK),
        "server must abort a corrupt transfer (got {status:?})"
    );
    drop(stream);
    let root = server.finish();

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
    fs::write(&source, b"").unwrap();

    let (addr, server) = LoopbackServer::start(&root);
    let (_, stats) = send_file_with_metrics(&source, &addr).unwrap();
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
    assert_eq!(received, b"");
    cleanup(&root);
    cleanup(&work);
}

#[test]
fn concurrent_transfers_all_byte_identical() {
    let root = fresh_root();
    let work = fresh_root();
    let clients = 5usize;
    let names: Vec<String> = (0..clients).map(|i| format!("c{i}.bin")).collect();
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

    let (addr, server) = LoopbackServer::start(&root);
    let mut handles = Vec::new();
    for name in &names {
        let src = work.join(name);
        let addr = addr.clone();
        handles.push(std::thread::spawn(move || {
            send_file_with_metrics(&src, &addr).expect("concurrent send must succeed");
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
    let chunk = vec![42u8; CHUNK_BYTES];
    let file_bytes: Vec<u8> = [chunk.clone(), chunk].concat();
    let source = work.join(name);
    fs::write(&source, &file_bytes).unwrap();

    let (addr, server) = LoopbackServer::start(&root);
    let (_, stats) = send_file_with_metrics(&source, &addr).unwrap();
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
    let chunk_files: Vec<PathBuf> = fs::read_dir(root.join("chunks"))
        .unwrap()
        .filter_map(|e| e.ok())
        .flat_map(|e| {
            fs::read_dir(e.path()).ok().map(|sub| {
                sub.filter_map(|e2| e2.ok())
                    .map(|e2| e2.path())
                    .filter(|p| {
                        p.file_name()
                            .and_then(|n| n.to_str())
                            .is_some_and(|n| !n.contains(".partial"))
                    })
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
    prestore(&root, &chunk0);

    let (addr, server) = LoopbackServer::start(&root);
    let mut stream = LoopbackServer::connect(&addr);
    do_handshake(&mut stream);
    write_u8(&mut stream, CMD_SYNC).unwrap();
    write_u16(&mut stream, name.len() as u16).unwrap();
    stream.write_all(name.as_bytes()).unwrap();
    write_u64(&mut stream, (2 * CHUNK_BYTES) as u64).unwrap();
    write_u32(&mut stream, CHUNK_BYTES as u32).unwrap();
    write_u32(&mut stream, 2).unwrap();
    stream.write_all(&digest(&chunk0)).unwrap();
    stream.write_all(&digest(&chunk1)).unwrap();
    let missing = read_missing(&mut stream);
    assert_eq!(missing, 1, "server should request chunk 1");

    let (codec, encoded) = encode_chunk(&chunk1);
    write_u32(&mut stream, 1).unwrap();
    write_u8(&mut stream, codec).unwrap();
    write_u32(&mut stream, encoded.len() as u32).unwrap();
    stream.write_all(&encoded[..encoded.len() / 2]).unwrap();
    drop(stream);

    let root = server.finish();
    assert!(
        !root.join("outputs").join(name).exists(),
        "a mid-chunk disconnect must not commit any output"
    );
    cleanup(&root);
}

#[test]
fn stale_in_progress_is_discarded_not_committed() {
    let root = fresh_root();
    let work = fresh_root();
    let name = "recovered.bin";
    let bytes = vec![4u8; 32];
    let source = work.join(name);
    fs::write(&source, &bytes).unwrap();
    let stale = root
        .join("outputs")
        .join(format!(".fastlink-inprogress-{name}"));
    fs::write(&stale, b"TORN").unwrap();
    let partial_dir = root.join("chunks").join("ab");
    fs::create_dir_all(&partial_dir).unwrap();
    fs::write(partial_dir.join(".partial-1-001122334455"), b"TORN-CHUNK").unwrap();

    let (addr, server) = LoopbackServer::start(&root);
    send_file_with_metrics(&source, &addr).unwrap();
    let root = server.finish();

    assert_eq!(fs::read(root.join("outputs").join(name)).unwrap(), bytes);
    assert!(
        !root
            .join("outputs")
            .join(format!(".fastlink-inprogress-{name}"))
            .exists(),
        "crash recovery must not leave or promote the in-progress file"
    );
    let leftover_partial = root
        .join("chunks")
        .join("ab")
        .join(".partial-1-001122334455");
    assert!(
        !leftover_partial.exists(),
        "crash recovery must remove the partial chunk without promoting it"
    );
    cleanup(&root);
    cleanup(&work);
}

#[test]
fn unknown_codec_is_rejected_before_payload_length() {
    let root = fresh_root();
    let name = "unk.bin";
    let config = ServerConfig {
        io_timeout: Duration::from_secs(8),
        ..test_config()
    };
    let (addr, server) = LoopbackServer::start_with(&root, config);
    let mut stream = LoopbackServer::connect(&addr);
    do_handshake(&mut stream);
    write_u8(&mut stream, CMD_SYNC).unwrap();
    write_u16(&mut stream, name.len() as u16).unwrap();
    stream.write_all(name.as_bytes()).unwrap();
    write_u64(&mut stream, 4).unwrap();
    write_u32(&mut stream, CHUNK_BYTES as u32).unwrap();
    write_u32(&mut stream, 1).unwrap();
    stream.write_all(&digest(&[1, 2, 3, 4])).unwrap();
    let missing = read_missing(&mut stream);
    assert_eq!(missing, 1);
    write_u32(&mut stream, 0).unwrap();
    write_u8(&mut stream, 9).unwrap();
    stream.flush().unwrap();
    let started = Instant::now();
    let status = read_status(&mut stream);
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "unknown codec must be rejected before the server reads a payload length"
    );
    assert!(status.is_none_or(|s| s != STATUS_OK));
    let root = server.finish();
    assert!(!root.join("outputs").join(name).exists());
    cleanup(&root);
}
