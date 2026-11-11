use fastlink::{
    digest, send_file_with_policy, spawn_server, verify_cache_integrity, AttemptFault,
    FaultInjectionPlan, LinkEstimate, RetryPolicy, ServerConfig, CHUNK_BYTES,
    POLICY_MAX_RECONNECTS, POLICY_MAX_RETRIES_PER_RPC, POLICY_RECONNECT_BACKOFF_MS,
    POLICY_TIMEOUT_FLOOR_MS, POLICY_TIMEOUT_MULTIPLIER,
};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

fn temp_test_dir(prefix: &str) -> PathBuf {
    static NEXT_ID: AtomicU64 = AtomicU64::new(1);
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let dir =
        std::env::temp_dir().join(format!("fastlink-a5-{prefix}-{}-{id}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn retry_policy_constants_and_timeout_formula_match_fl3_c3_table() {
    let policy = RetryPolicy::default();
    assert_eq!(policy.max_retries_per_rpc, 2);
    assert_eq!(POLICY_MAX_RETRIES_PER_RPC, 2);
    assert_eq!(policy.timeout_floor_ms, 50.0);
    assert_eq!(POLICY_TIMEOUT_FLOOR_MS, 50.0);
    assert_eq!(policy.timeout_multiplier, 3.0);
    assert_eq!(POLICY_TIMEOUT_MULTIPLIER, 3.0);
    assert_eq!(policy.reconnect_backoff_ms, [100, 400, 1600]);
    assert_eq!(POLICY_RECONNECT_BACKOFF_MS, [100, 400, 1600]);
    assert_eq!(policy.max_reconnects, 3);
    assert_eq!(POLICY_MAX_RECONNECTS, 3);

    assert_eq!(policy.backoff_ms_for(0), 100);
    assert_eq!(policy.backoff_ms_for(1), 400);
    assert_eq!(policy.backoff_ms_for(2), 1600);
    assert_eq!(policy.backoff_ms_for(9), 1600);

    let nominal = LinkEstimate::nominal();

    // 1. decode_boundary_8KiB (8,192 B = 0.0078125 MiB): 50 ms floor across f in {1.0, 0.5, 0.1, 0.01}
    for &factor in &[1.0, 0.5, 0.1, 0.01] {
        let t = policy.attempt_timeout_ms(&nominal.degraded(factor), 8 * 1024);
        assert_eq!(t.round() as u64, 50);
    }

    // 2. weight_chunk_64KiB (65,536 B = 0.0625 MiB): 50 ms at f=1.0, 0.5, 0.1; 200 ms at f=0.01
    assert_eq!(
        policy
            .attempt_timeout_ms(&nominal.degraded(1.0), 65_536)
            .round() as u64,
        50
    );
    assert_eq!(
        policy
            .attempt_timeout_ms(&nominal.degraded(0.5), 65_536)
            .round() as u64,
        50
    );
    assert_eq!(
        policy
            .attempt_timeout_ms(&nominal.degraded(0.1), 65_536)
            .round() as u64,
        50
    );
    assert_eq!(
        policy
            .attempt_timeout_ms(&nominal.degraded(0.01), 65_536)
            .round() as u64,
        200
    );

    // 3. ubatch_boundary_4MiB (4,194,304 B = 4.0 MiB): 130 ms, 255 ms, 1262 ms, 12587 ms
    let ubatch_bytes = 4 * 1024 * 1024;
    assert_eq!(
        policy
            .attempt_timeout_ms(&nominal.degraded(1.0), ubatch_bytes)
            .round() as u64,
        130
    );
    assert_eq!(
        policy
            .attempt_timeout_ms(&nominal.degraded(0.5), ubatch_bytes)
            .round() as u64,
        255
    );
    assert_eq!(
        policy
            .attempt_timeout_ms(&nominal.degraded(0.1), ubatch_bytes)
            .round() as u64,
        1262
    );
    assert_eq!(
        policy
            .attempt_timeout_ms(&nominal.degraded(0.01), ubatch_bytes)
            .round() as u64,
        12587
    );

    // Worst-case 4 MiB ubatch budget at f=0.01: 3 reconnects * (1 + 2 retries) * 12586.66 ms = 113.3 s..151.0 s
    let worst_s = policy.worst_case_budget_ms(&nominal.degraded(0.01), ubatch_bytes) / 1000.0;
    assert!(worst_s > 110.0 && worst_s < 152.0);
}

#[test]
fn simulated_link_degradation_forced_timeouts_recover_on_retry_and_resume_by_manifest() {
    let dir = temp_test_dir("degraded-retry");
    let root = dir.join("cache");
    let src_path = dir.join("degraded-weights.bin");

    // 8 distinct 64 KiB chunks (512 KiB)
    let total_chunks = 8usize;
    let mut payload = Vec::with_capacity(CHUNK_BYTES * total_chunks);
    for c in 0..total_chunks {
        let chunk: Vec<u8> = (0..CHUNK_BYTES)
            .map(|i| ((i.wrapping_mul(23 + c * 5) ^ (c * 109)) % 251) as u8)
            .collect();
        payload.extend_from_slice(&chunk);
    }
    fs::write(&src_path, &payload).unwrap();

    let server = spawn_server("127.0.0.1:0", &root, ServerConfig::default()).unwrap();
    let addr = server.addr_string();

    // Fault plan:
    // - Attempt 0: sends 2 chunks, then times out as link degrades to 10% (f=0.1)
    // - Attempt 1 (retry 1): sends 3 more chunks, then times out as link degrades to 1% (f=0.01)
    // - Attempt 2 (retry 2): link timeout dynamically expands to 1% link estimate; transfers remaining 3 chunks and succeeds!
    let plan = FaultInjectionPlan {
        per_attempt_faults: vec![
            AttemptFault::TimeoutAfterChunks {
                chunks_before_timeout: 2,
                degrade_factor: 10,
            },
            AttemptFault::TimeoutAfterChunks {
                chunks_before_timeout: 3,
                degrade_factor: 1,
            },
            AttemptFault::None,
        ],
        sleep_on_backoff: false,
    };

    let report = send_file_with_policy(
        &src_path,
        &addr,
        RetryPolicy::default(),
        LinkEstimate::nominal(),
        Some(&plan),
    )
    .unwrap();

    assert!(!report.terminal_abort);
    assert_eq!(report.total_attempts, 3);
    assert_eq!(report.rpc_retries_used, 2);
    assert_eq!(report.reconnects_used, 0);
    assert_eq!(report.attempt_timeouts_ms.len(), 3);
    // Timeout must strictly increase as the link estimate degrades from 100% -> 10% -> 1%
    assert!(report.attempt_timeouts_ms[1] > report.attempt_timeouts_ms[0]);
    assert!(report.attempt_timeouts_ms[2] > report.attempt_timeouts_ms[1]);

    // The injected disconnect occurs after writing 2+3 chunks, but the
    // receiver may have durably cached fewer bytes before socket teardown.
    // Validate the final complete transfer, not a scheduler-dependent count.
    let metrics = report.final_metrics.unwrap();
    assert_eq!(metrics.chunks_total, 8);
    assert!(metrics.chunks_reused <= 5);
    assert_eq!(metrics.chunks_reused + metrics.chunks_transferred, 8);

    let materialized = fs::read(root.join("outputs").join("degraded-weights.bin")).unwrap();
    assert_eq!(digest(&materialized), digest(&payload));
    assert_eq!(materialized, payload);
    assert_eq!(verify_cache_integrity(&root).unwrap(), 8);

    server.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn terminal_failure_after_exhausting_retries_and_reconnects_commits_zero_output_bytes() {
    let dir = temp_test_dir("terminal-abort");
    let root = dir.join("cache");
    let src_path = dir.join("terminal-fail.bin");

    // 6 distinct 64 KiB chunks
    let total_chunks = 6usize;
    let mut payload = Vec::with_capacity(CHUNK_BYTES * total_chunks);
    for c in 0..total_chunks {
        let chunk: Vec<u8> = (0..CHUNK_BYTES)
            .map(|i| ((i.wrapping_mul(41 + c * 9) + c * 67) % 253) as u8)
            .collect();
        payload.extend_from_slice(&chunk);
    }
    fs::write(&src_path, &payload).unwrap();

    let server = spawn_server("127.0.0.1:0", &root, ServerConfig::default()).unwrap();
    let addr = server.addr_string();

    // Disconnect before the first chunk on each attempt. Allowing one chunk
    // per attempt is nondeterministic: enough cached chunks may accumulate to
    // complete the file before the injected fault, defeating the terminal test.
    // Four disconnects: initial connect + 3 reconnects -> terminal abort.
    let plan = FaultInjectionPlan {
        per_attempt_faults: vec![
            AttemptFault::DisconnectAfterChunks(0),
            AttemptFault::DisconnectAfterChunks(0),
            AttemptFault::DisconnectAfterChunks(0),
            AttemptFault::DisconnectAfterChunks(0),
        ],
        sleep_on_backoff: false,
    };

    let err = send_file_with_policy(
        &src_path,
        &addr,
        RetryPolicy::default(),
        LinkEstimate::nominal(),
        Some(&plan),
    )
    .unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::ConnectionAborted);

    // Wait briefly for server sessions to finish cleaning up
    for _ in 0..50 {
        if server.coord().total_sessions_aborted() >= 4 {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }

    // Verify ZERO output files or partial files were committed in `outputs/`
    let final_output = root.join("outputs").join("terminal-fail.bin");
    assert!(
        !final_output.exists(),
        "terminal abort must NEVER commit partial output"
    );
    let output_entries: Vec<_> = fs::read_dir(root.join("outputs"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .collect();
    assert!(
        output_entries.is_empty(),
        "outputs directory must be completely empty on terminal abort, got: {output_entries:?}"
    );

    // At most four chunks could have been received, but TCP shutdown does not
    // guarantee delivery/persistence of all four on a busy Windows runner.
    // Every chunk that *was* retained must pass cache-integrity validation.
    let verified = verify_cache_integrity(&root).unwrap();
    assert!(
        verified <= 4,
        "a failed transfer cannot cache more than four chunks"
    );

    server.shutdown();
    let _ = fs::remove_dir_all(&dir);
}
