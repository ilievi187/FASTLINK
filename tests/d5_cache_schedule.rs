//! FL3-D5: loopback transfer plus the in-memory budget cache, and the
//! public placement scheduler. Loopback only. No GPU.

use fastlink::cache_budget::MemoryBudgetCache;
use fastlink::scheduler::{
    select_placement, KvMode, LINK_REMOTE, LOCAL_DEDICATED, REMOTE_DEVICE, WORKLOAD_27B,
};
use fastlink::{digest, send_file_with_metrics, spawn_server, ServerConfig, CHUNK_BYTES};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

static ROOT_SEQ: AtomicUsize = AtomicUsize::new(0);

fn fresh_root() -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "fastlink-d5-{}-{}",
        std::process::id(),
        ROOT_SEQ.fetch_add(1, Ordering::Relaxed) + 1
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("chunks")).unwrap();
    fs::create_dir_all(root.join("outputs")).unwrap();
    root
}

#[test]
fn memory_budget_eviction_does_not_change_a_committed_output() {
    let root = fresh_root();
    let work = fresh_root();
    let name = "d5.bin";
    let first = vec![1u8; CHUNK_BYTES];
    let second = vec![2u8; CHUNK_BYTES];
    let file_bytes = [first.clone(), second.clone()].concat();
    let source = work.join(name);
    fs::write(&source, &file_bytes).unwrap();

    let running = spawn_server("127.0.0.1:0", &root, ServerConfig::default()).unwrap();
    let (_, stats) = send_file_with_metrics(&source, &running.addr_string()).unwrap();
    assert_eq!(stats.chunks_transferred, 2);
    let output = root.join("outputs").join(name);
    let committed = fs::read(&output).unwrap();
    assert_eq!(committed, file_bytes);
    running.shutdown();

    let mut cache = MemoryBudgetCache::new(CHUNK_BYTES);
    let first_hash = digest(&first);
    let second_hash = digest(&second);
    assert!(cache.insert(first_hash, first).unwrap().is_empty());
    let evicted = cache.insert(second_hash, second).unwrap();
    assert_eq!(evicted.len(), 1);
    assert_eq!(evicted[0].digest, first_hash);
    assert_eq!(cache.get(&second_hash).unwrap().len(), CHUNK_BYTES);
    assert!(cache.get(&first_hash).is_none());
    assert_eq!(fs::read(&output).unwrap(), committed);

    let _ = fs::remove_dir_all(&root);
    let _ = fs::remove_dir_all(&work);
}

#[test]
fn public_scheduler_picks_the_reference_split() {
    let best = select_placement(
        WORKLOAD_27B,
        131_072,
        KvMode::Q4,
        LOCAL_DEDICATED,
        REMOTE_DEVICE,
        LINK_REMOTE,
    )
    .unwrap();
    assert_eq!(best.k_local, 53);
    assert_eq!(best.layers_remote, 11);
    assert!(best.feasible);
    let repeat = select_placement(
        WORKLOAD_27B,
        131_072,
        KvMode::Q4,
        LOCAL_DEDICATED,
        REMOTE_DEVICE,
        LINK_REMOTE,
    )
    .unwrap();
    assert_eq!(repeat.k_local, best.k_local);
    assert_eq!(
        repeat.decode_step_ms.to_bits(),
        best.decode_step_ms.to_bits()
    );
}
