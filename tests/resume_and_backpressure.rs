use fastlink::{
    cached_and_valid, clean_stale_partials, digest, digest_path, send_file_once_with_options,
    send_file_resumable, spawn_server, BoundedQueue, SendOptions, ServerConfig, CHUNK_BYTES,
};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

fn temp_test_dir(prefix: &str) -> PathBuf {
    static NEXT_ID: AtomicU64 = AtomicU64::new(1);
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let dir =
        std::env::temp_dir().join(format!("fastlink-a3-{prefix}-{}-{id}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn bounded_queue_enforces_capacity_and_records_backpressure() {
    let q = Arc::new(BoundedQueue::<u32>::new(2));
    assert_eq!(q.capacity(), 2);

    q.push(10).unwrap();
    q.push(20).unwrap();
    assert_eq!(q.peak_depth(), 2);

    let q_prod = Arc::clone(&q);
    let producer = thread::spawn(move || {
        for v in [30u32, 40, 50] {
            q_prod.push(v).unwrap();
        }
    });

    // Give the producer a moment to block on the full capacity-2 queue
    thread::sleep(Duration::from_millis(20));
    assert!(
        q.backpressure_waits() >= 1,
        "producer should have blocked on bounded queue backpressure"
    );
    assert_eq!(q.peak_depth(), 2);

    let mut received = Vec::new();
    for _ in 0..5 {
        received.push(q.pop().unwrap());
    }
    producer.join().unwrap();
    assert_eq!(received, vec![10, 20, 30, 40, 50]);
    assert_eq!(q.peak_depth(), 2);
}

#[test]
fn mid_transfer_kill_leaves_no_partial_output_and_resumes_by_manifest_without_full_resend() {
    let dir = temp_test_dir("kill-resume");
    let root = dir.join("cache");
    let src_path = dir.join("resumable-weights.bin");

    // 8 distinct 64 KiB chunks (512 KiB total)
    let total_chunks = 8usize;
    let mut payload = Vec::with_capacity(CHUNK_BYTES * total_chunks);
    let mut chunk_hashes = Vec::with_capacity(total_chunks);
    for c in 0..total_chunks {
        let chunk: Vec<u8> = (0..CHUNK_BYTES)
            .map(|i| ((i.wrapping_mul(17 + c * 3) ^ (c * 97)) % 251) as u8)
            .collect();
        chunk_hashes.push(digest(&chunk));
        payload.extend_from_slice(&chunk);
    }
    fs::write(&src_path, &payload).unwrap();

    // Small bounded queue capacity (2 chunks) to exercise server-side backpressure during transfer
    let config = ServerConfig {
        chunk_queue_capacity: 2,
        connection_queue_capacity: 4,
        max_concurrent_sessions: 2,
        io_timeout: Duration::from_secs(30),
    };
    let server = spawn_server("127.0.0.1:0", &root, config).unwrap();
    let addr = server.addr_string();

    // 1. Mid-transfer kill after sending K=3 of 8 chunks
    let err = send_file_once_with_options(&src_path, &addr, Some(3)).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::ConnectionAborted);

    // Wait briefly for the server session to observe EOF and abort cleanly
    for _ in 0..50 {
        if server.coord().total_sessions_aborted() >= 1 {
            break;
        }
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(server.coord().total_sessions_aborted(), 1);
    assert_eq!(server.coord().total_sessions_completed(), 0);

    // Verify NO output file or leftover partial output file is committed
    let final_output = root.join("outputs").join("resumable-weights.bin");
    assert!(
        !final_output.exists(),
        "killed transfer must never commit partial output"
    );
    let output_entries: Vec<_> = fs::read_dir(root.join("outputs"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .collect();
    assert!(
        output_entries.is_empty(),
        "outputs directory must not contain committed or partial files after abort, got: {output_entries:?}"
    );

    // Verify the first 3 chunks were persisted in the content-addressed store
    for (i, h) in chunk_hashes.iter().enumerate().take(3) {
        assert!(
            cached_and_valid(&digest_path(&root, h), h),
            "chunk {i} should be cached before mid-transfer disconnect"
        );
    }

    // 2. Resume by manifest: re-attach and send only the remaining 5 missing chunks (no full resend)
    let (_, resumed) = send_file_once_with_options(&src_path, &addr, None).unwrap();
    assert_eq!(resumed.chunks_total, 8);
    assert_eq!(
        resumed.chunks_reused, 3,
        "resumed session must reuse the 3 previously cached chunks"
    );
    assert_eq!(
        resumed.chunks_transferred, 5,
        "resumed session must transfer only the 5 remaining chunks"
    );

    // Verify byte-exact final output
    let materialized = fs::read(&final_output).unwrap();
    assert_eq!(digest(&materialized), digest(&payload));
    assert_eq!(materialized, payload);
    assert_eq!(server.coord().total_sessions_completed(), 1);

    server.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn send_file_resumable_automatically_recovers_from_mid_transfer_disconnect() {
    let dir = temp_test_dir("auto-resume");
    let root = dir.join("cache");
    let src_path = dir.join("auto-resume.bin");

    // 6 distinct 64 KiB chunks
    let total_chunks = 6usize;
    let mut payload = Vec::with_capacity(CHUNK_BYTES * total_chunks);
    for c in 0..total_chunks {
        let chunk: Vec<u8> = (0..CHUNK_BYTES)
            .map(|i| ((i.wrapping_mul(29 + c * 7) + c * 53) % 253) as u8)
            .collect();
        payload.extend_from_slice(&chunk);
    }
    fs::write(&src_path, &payload).unwrap();

    // Also plant a stale partial file to verify startup cleanup removes it
    fs::create_dir_all(root.join("outputs")).unwrap();
    fs::write(
        root.join("outputs")
            .join(".auto-resume.bin.fastlink-partial.s999"),
        b"corrupt-partial",
    )
    .unwrap();
    assert_eq!(clean_stale_partials(&root).unwrap(), 1);

    let config = ServerConfig {
        chunk_queue_capacity: 1, // force maximum bounded-queue backpressure (1 chunk in flight)
        connection_queue_capacity: 4,
        max_concurrent_sessions: 2,
        io_timeout: Duration::from_secs(30),
    };
    let server = spawn_server("127.0.0.1:0", &root, config).unwrap();
    let addr = server.addr_string();

    // First attempt aborts after 2 chunks; `send_file_resumable` automatically reconnects and finishes the remaining 4 chunks
    let (_, metrics) = send_file_resumable(
        &src_path,
        &addr,
        SendOptions {
            max_resume_retries: 2,
            abort_after_chunks: Some(2),
        },
    )
    .unwrap();

    assert_eq!(metrics.chunks_total, 6);
    assert_eq!(metrics.chunks_reused, 2);
    assert_eq!(metrics.chunks_transferred, 4);

    let materialized = fs::read(root.join("outputs").join("auto-resume.bin")).unwrap();
    assert_eq!(digest(&materialized), digest(&payload));
    assert_eq!(materialized, payload);

    server.shutdown();
    let _ = fs::remove_dir_all(&dir);
}
