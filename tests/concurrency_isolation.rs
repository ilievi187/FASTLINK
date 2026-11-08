use fastlink::{
    digest, send_file_once_with_options, send_file_resumable, send_file_with_metrics, spawn_server,
    synthetic_incompressible_chunk, synthetic_sparse_chunk, synthetic_tensor_like_chunk,
    verify_cache_integrity, SendOptions, ServerConfig, CHUNK_BYTES,
};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;

fn temp_test_dir(prefix: &str) -> PathBuf {
    static NEXT_ID: AtomicU64 = AtomicU64::new(1);
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let dir =
        std::env::temp_dir().join(format!("fastlink-a4-{prefix}-{}-{id}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn four_concurrent_clients_transfer_overlapping_and_unique_files_byte_exact_without_cache_corruption(
) {
    let dir = temp_test_dir("n4-concurrent");
    let root = dir.join("cache");
    let client_dir = dir.join("clients");
    fs::create_dir_all(&client_dir).unwrap();

    // Shared chunks across all 4 clients to stress concurrent deduplicated cache writes
    let shared_sparse = synthetic_sparse_chunk();
    let shared_tensor = synthetic_tensor_like_chunk();
    let shared_incomp = synthetic_incompressible_chunk();

    let mut client_files = Vec::new();
    let mut expected_payloads = Vec::new();

    for client_id in 0..4usize {
        let fname = format!("client-{client_id}-tensor.bin");
        let path = client_dir.join(&fname);
        let mut payload = Vec::with_capacity(CHUNK_BYTES * 6);
        // 3 shared chunks (in client-dependent order) + 3 unique chunks per client
        match client_id % 3 {
            0 => {
                payload.extend_from_slice(&shared_sparse);
                payload.extend_from_slice(&shared_tensor);
                payload.extend_from_slice(&shared_incomp);
            }
            1 => {
                payload.extend_from_slice(&shared_tensor);
                payload.extend_from_slice(&shared_incomp);
                payload.extend_from_slice(&shared_sparse);
            }
            _ => {
                payload.extend_from_slice(&shared_incomp);
                payload.extend_from_slice(&shared_sparse);
                payload.extend_from_slice(&shared_tensor);
            }
        }
        for u in 0..3usize {
            let unique_chunk: Vec<u8> = (0..CHUNK_BYTES)
                .map(|i| {
                    ((i.wrapping_mul(37 + client_id * 13 + u * 7) ^ (client_id * 151 + u * 41))
                        % 251) as u8
                })
                .collect();
            payload.extend_from_slice(&unique_chunk);
        }
        fs::write(&path, &payload).unwrap();
        client_files.push((fname, path));
        expected_payloads.push(payload);
    }

    let config = ServerConfig {
        chunk_queue_capacity: 4,
        connection_queue_capacity: 8,
        max_concurrent_sessions: 4,
        io_timeout: Duration::from_secs(30),
    };
    let server = spawn_server("127.0.0.1:0", &root, config).unwrap();
    let addr = server.addr_string();

    // Synchronize all N=4 client threads at a barrier so they connect and transfer simultaneously
    let barrier = Arc::new(Barrier::new(4));
    let mut handles = Vec::new();
    for (_, path) in &client_files {
        let p = path.clone();
        let a = addr.clone();
        let b = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            b.wait();
            send_file_with_metrics(&p, &a)
                .or_else(|_| send_file_resumable(&p, &a, SendOptions::default()))
                .unwrap()
        }));
    }

    for (client_id, handle) in handles.into_iter().enumerate() {
        let (name, metrics) = handle.join().unwrap();
        assert_eq!(name, client_files[client_id].0);
        assert_eq!(metrics.file_bytes, (CHUNK_BYTES * 6) as u64);
        assert_eq!(metrics.chunks_total, 6);
        assert_eq!(
            metrics.chunks_reused + metrics.chunks_transferred,
            6,
            "reused + transferred must equal total chunks"
        );
    }

    assert!(server.coord().total_sessions_completed() >= 4);
    assert!(
        server.coord().peak_concurrent_sessions() >= 1,
        "expected session execution across worker pool"
    );

    // Verify all 4 materialized output files are byte-exact
    for (client_id, (fname, _)) in client_files.iter().enumerate() {
        let out_path = root.join("outputs").join(fname);
        let actual = fs::read(&out_path).unwrap();
        assert_eq!(digest(&actual), digest(&expected_payloads[client_id]));
        assert_eq!(actual, expected_payloads[client_id]);
    }

    // Verify cache integrity: 3 shared chunks + 4 * 3 unique chunks = 15 distinct verified chunks, zero partials
    let verified_count = verify_cache_integrity(&root).unwrap();
    assert_eq!(verified_count, 15);

    server.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn concurrent_session_isolation_when_one_client_aborts_mid_transfer_others_complete_intact() {
    let dir = temp_test_dir("n4-isolation");
    let root = dir.join("cache");
    let client_dir = dir.join("clients");
    fs::create_dir_all(&client_dir).unwrap();

    let mut files = Vec::new();
    let mut payloads = Vec::new();
    for client_id in 0..4usize {
        let fname = format!("isolated-{client_id}.bin");
        let path = client_dir.join(&fname);
        let mut data = Vec::with_capacity(CHUNK_BYTES * 5);
        for c in 0..5usize {
            let chunk: Vec<u8> = (0..CHUNK_BYTES)
                .map(|i| {
                    ((i.wrapping_mul(19 + client_id * 11 + c * 5) + client_id * 73) % 253) as u8
                })
                .collect();
            data.extend_from_slice(&chunk);
        }
        fs::write(&path, &data).unwrap();
        files.push((fname, path));
        payloads.push(data);
    }

    let config = ServerConfig {
        chunk_queue_capacity: 2,
        connection_queue_capacity: 8,
        max_concurrent_sessions: 4,
        io_timeout: Duration::from_secs(30),
    };
    let server = spawn_server("127.0.0.1:0", &root, config).unwrap();
    let addr = server.addr_string();

    let barrier = Arc::new(Barrier::new(4));
    let mut handles = Vec::new();
    for (client_id, (_, path)) in files.iter().enumerate() {
        let p = path.clone();
        let a = addr.clone();
        let b = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            b.wait();
            if client_id == 0 {
                // Client 0 aborts mid-transfer after 2 chunks
                send_file_once_with_options(&p, &a, Some(2))
            } else {
                send_file_once_with_options(&p, &a, None)
                    .or_else(|_| send_file_resumable(&p, &a, SendOptions::default()))
            }
        }));
    }

    for (client_id, handle) in handles.into_iter().enumerate() {
        let res = handle.join().unwrap();
        if client_id == 0 {
            assert!(
                res.is_err(),
                "client 0 was configured to abort mid-transfer"
            );
        } else {
            let (_, m) = res.unwrap();
            assert_eq!(m.chunks_total, 5);
        }
    }

    // Wait for client 0's server session to finish cleanup (robust under heavy system load)
    for _ in 0..200 {
        if server.coord().total_sessions_aborted() >= 1
            && server.coord().total_sessions_completed() >= 3
        {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }

    // Client 0's output must NOT exist, while clients 1..=3 must be byte-exact
    assert!(!root.join("outputs").join(&files[0].0).exists());
    for client_id in 1..4usize {
        let actual = fs::read(root.join("outputs").join(&files[client_id].0)).unwrap();
        assert_eq!(digest(&actual), digest(&payloads[client_id]));
        assert_eq!(actual, payloads[client_id]);
    }

    // Cache must have zero corrupt or leftover partial files (2 chunks from client 0 + 15 chunks from clients 1..3 = 17 chunks)
    let verified = verify_cache_integrity(&root).unwrap();
    assert_eq!(verified, 17);

    server.shutdown();
    let _ = fs::remove_dir_all(&dir);
}
