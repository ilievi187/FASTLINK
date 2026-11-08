//! FL4-IMPL state sync over the loopback server. CPU only. No GPU.

use fastlink::qwen35_4b::{kv_q4_0_bytes_per_token_per_head, ssm_state_bytes, WorkerHome};
use fastlink::state_cache::{StateRef, TensorId, TensorKind};
use fastlink::{
    bump_state_layer, check_state_ref, end_state_session, fetch_kv, fetch_ssm,
    invalidate_state_layer, restart_state_worker, spawn_server, sync_kv, sync_ssm, RefStatus,
    ServerConfig, StateSyncReport,
};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;

fn temp_dir(prefix: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "fastlink-fl4impl-{prefix}-{}-{id}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn ssm(byte: u8) -> Vec<u8> {
    vec![byte; ssm_state_bytes()]
}

fn ssm_ref(session: u64, layer: u32, report: &StateSyncReport) -> StateRef {
    StateRef {
        session,
        id: TensorId {
            kind: TensorKind::Ssm,
            layer,
            head: 0,
            seq_start: 0,
            seq_end: 0,
        },
        version: report.version,
        hash: report.hash,
        worker_generation: report.worker_generation,
    }
}

fn server(dir: &std::path::Path) -> fastlink::RunningServer {
    let config = ServerConfig {
        chunk_queue_capacity: 4,
        connection_queue_capacity: 8,
        max_concurrent_sessions: 4,
        io_timeout: Duration::from_secs(30),
    };
    spawn_server("127.0.0.1:0", dir, config).unwrap()
}

#[test]
fn warm_ssm_resyncs_zero_payload_bytes_and_matches() {
    let dir = temp_dir("warm");
    let running = server(&dir);
    let addr = running.addr_string();
    let bytes = ssm(9);
    let cold = sync_ssm(&addr, 1, 0, &bytes).unwrap();
    assert_eq!(cold.payload_bytes, bytes.len() as u64);
    assert!(!cold.reused);
    assert_eq!(fetch_ssm(&addr, 1, 0).unwrap(), bytes);

    let mut warm_payload = 0u64;
    for _ in 0..3 {
        let warm = sync_ssm(&addr, 1, 0, &bytes).unwrap();
        assert!(warm.reused);
        assert_eq!(warm.hash, cold.hash);
        assert_eq!(warm.version, cold.version);
        warm_payload += warm.payload_bytes;
    }
    assert_eq!(warm_payload, 0);
    assert_eq!(fetch_ssm(&addr, 1, 0).unwrap(), bytes);
    running.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn update_reuse_and_version_bump_invalidate_only_the_old_ref() {
    let dir = temp_dir("inv");
    let running = server(&dir);
    let addr = running.addr_string();
    let original = ssm(1);
    let first = sync_ssm(&addr, 4, 0, &original).unwrap();
    let first_ref = ssm_ref(4, 0, &first);
    assert_eq!(
        check_state_ref(&addr, &first_ref).unwrap(),
        RefStatus::Valid
    );

    let changed = ssm(2);
    let second = sync_ssm(&addr, 4, 0, &changed).unwrap();
    assert_eq!(second.payload_bytes, changed.len() as u64);
    assert!(!second.reused);
    assert_ne!(second.hash, first.hash);
    assert!(second.version > first.version);
    assert_eq!(
        check_state_ref(&addr, &first_ref).unwrap(),
        RefStatus::Invalidated
    );
    assert_eq!(fetch_ssm(&addr, 4, 0).unwrap(), changed);

    invalidate_state_layer(&addr, 4, 0).unwrap();
    assert_eq!(
        check_state_ref(&addr, &ssm_ref(4, 0, &second)).unwrap(),
        RefStatus::Invalidated
    );
    let reused_slot = ssm(3);
    let third = sync_ssm(&addr, 4, 0, &reused_slot).unwrap();
    assert_eq!(third.payload_bytes, reused_slot.len() as u64);
    assert_eq!(fetch_ssm(&addr, 4, 0).unwrap(), reused_slot);
    assert_eq!(
        check_state_ref(&addr, &ssm_ref(4, 0, &second)).unwrap(),
        RefStatus::Invalidated
    );

    let before_bump = ssm_ref(4, 0, &third);
    bump_state_layer(&addr, 4, 0).unwrap();
    assert_eq!(
        check_state_ref(&addr, &before_bump).unwrap(),
        RefStatus::Invalidated
    );
    let revalidated = sync_ssm(&addr, 4, 0, &reused_slot).unwrap();
    assert_eq!(revalidated.payload_bytes, 0);
    assert!(revalidated.reused);
    assert_eq!(revalidated.hash, third.hash);
    assert!(revalidated.version > third.version);
    assert_eq!(fetch_ssm(&addr, 4, 0).unwrap(), reused_slot);
    running.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn restart_resends_only_the_invalidated_layer() {
    let dir = temp_dir("restart");
    let running = server(&dir);
    let addr = running.addr_string();
    let layer0 = ssm(4);
    let layer1 = ssm(5);
    let other = ssm(6);
    let a0 = sync_ssm(&addr, 11, 0, &layer0).unwrap();
    let a1 = sync_ssm(&addr, 11, 1, &layer1).unwrap();
    let b0 = sync_ssm(&addr, 12, 0, &other).unwrap();
    let old = ssm_ref(11, 0, &a0);

    let generation = restart_state_worker(&addr).unwrap();
    assert!(generation > a0.worker_generation);
    assert_eq!(check_state_ref(&addr, &old).unwrap(), RefStatus::Stale);

    let again0 = sync_ssm(&addr, 11, 0, &layer0).unwrap();
    let again1 = sync_ssm(&addr, 11, 1, &layer1).unwrap();
    let again_b = sync_ssm(&addr, 12, 0, &other).unwrap();
    assert_eq!(again0.payload_bytes, 0);
    assert_eq!(again1.payload_bytes, 0);
    assert_eq!(again_b.payload_bytes, 0);
    assert_eq!(again0.hash, a0.hash);
    assert_eq!(again1.hash, a1.hash);
    assert_eq!(again_b.hash, b0.hash);

    invalidate_state_layer(&addr, 11, 0).unwrap();
    let replacement = ssm(7);
    let resent = sync_ssm(&addr, 11, 0, &replacement).unwrap();
    assert_eq!(resent.payload_bytes, replacement.len() as u64);
    let still0 = sync_ssm(&addr, 11, 1, &layer1).unwrap();
    let still_b = sync_ssm(&addr, 12, 0, &other).unwrap();
    assert_eq!(still0.payload_bytes, 0);
    assert_eq!(still_b.payload_bytes, 0);
    assert_eq!(fetch_ssm(&addr, 11, 0).unwrap(), replacement);
    assert_eq!(fetch_ssm(&addr, 11, 1).unwrap(), layer1);
    assert_eq!(fetch_ssm(&addr, 12, 0).unwrap(), other);
    running.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn four_concurrent_sessions_do_not_corrupt_each_other() {
    let dir = temp_dir("n4");
    let running = server(&dir);
    let addr = running.addr_string();
    let barrier = Arc::new(Barrier::new(4));
    let mut handles = Vec::new();
    for session in 0..4u64 {
        let addr = addr.clone();
        let barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            barrier.wait();
            let bytes = ssm(20 + session as u8);
            let report = sync_ssm(&addr, 20 + session, 0, &bytes).unwrap();
            (bytes, report)
        }));
    }
    let mut stored = Vec::new();
    for handle in handles {
        stored.push(handle.join().unwrap());
    }
    assert_eq!(stored.len(), 4);
    invalidate_state_layer(&addr, 20, 0).unwrap();
    assert!(fetch_ssm(&addr, 20, 0).is_err());
    for session in 1..4u64 {
        assert_eq!(
            fetch_ssm(&addr, 20 + session, 0).unwrap(),
            stored[session as usize].0
        );
    }
    end_state_session(&addr, 21).unwrap();
    assert!(fetch_ssm(&addr, 21, 0).is_err());
    assert_eq!(fetch_ssm(&addr, 22, 0).unwrap(), stored[2].0);
    assert_eq!(fetch_ssm(&addr, 23, 0).unwrap(), stored[3].0);
    running.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn kv_appends_only_the_new_span_and_records_the_placement_home() {
    let dir = temp_dir("kv");
    let running = server(&dir);
    let addr = running.addr_string();
    let stride = kv_q4_0_bytes_per_token_per_head();
    let first = vec![1u8; stride];
    let second = vec![2u8; stride];
    let local = sync_kv(&addr, 7, 3, 0, 0, &first).unwrap();
    assert_eq!(local.payload_bytes, first.len() as u64);
    assert_eq!(local.kv_home, Some(WorkerHome::Local7900Xt));
    let warm = sync_kv(&addr, 7, 3, 0, 0, &first).unwrap();
    assert_eq!(warm.payload_bytes, 0);
    assert!(warm.reused);
    let tail = sync_kv(&addr, 7, 3, 0, 1, &second).unwrap();
    assert_eq!(tail.payload_bytes, second.len() as u64);
    assert!(!tail.reused);
    assert_eq!(fetch_kv(&addr, 7, 3, 0, 0, 1).unwrap(), first);
    assert_eq!(fetch_kv(&addr, 7, 3, 0, 1, 2).unwrap(), second);
    let remote = sync_kv(&addr, 7, 11, 0, 0, &first).unwrap();
    assert_eq!(remote.kv_home, Some(WorkerHome::Remote6600Xt));
    assert!(sync_kv(&addr, 7, 3, 0, 5, &first).is_err());
    running.shutdown();
    let _ = fs::remove_dir_all(&dir);
}
