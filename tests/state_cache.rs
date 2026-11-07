//! FL4-KV persistent state cache. CPU only. No sockets.

use fastlink::qwen35_4b::{is_attention_layer, is_recurrent_layer, ssm_state_bytes};
use fastlink::state_cache::{PersistentStateCache, StateError};
use fastlink::CHUNK_BYTES;
use std::fs;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;

fn fresh_root() -> std::path::PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!("fastlink-fl4kv-{}-{id}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    root
}

fn ssm(byte: u8) -> Vec<u8> {
    vec![byte; ssm_state_bytes()]
}

#[test]
fn ssm_state_survives_worker_restart_and_old_refs_go_stale() {
    let root = fresh_root();
    let bytes = ssm(7);
    let first = PersistentStateCache::open(&root).unwrap();
    let reference = first.put_ssm(1, 0, &bytes).unwrap();
    assert_eq!(first.get(&reference).unwrap(), bytes);
    drop(first);

    let restarted = PersistentStateCache::open(&root).unwrap();
    assert!(matches!(
        restarted.get(&reference),
        Err(StateError::StaleReference)
    ));
    let current = restarted.lookup_ssm(1, 0).unwrap();
    assert_ne!(current.worker_generation, reference.worker_generation);
    assert_eq!(current.hash, reference.hash);
    assert_eq!(restarted.get(&current).unwrap(), bytes);
    let generation = restarted.note_worker_restart().unwrap();
    assert!(matches!(
        restarted.get(&current),
        Err(StateError::StaleReference)
    ));
    let after = restarted.lookup_ssm(1, 0).unwrap();
    assert_eq!(after.worker_generation, generation);
    assert_eq!(restarted.get(&after).unwrap(), bytes);
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn ssm_update_invalidates_the_previous_version() {
    let root = fresh_root();
    let cache = PersistentStateCache::open(&root).unwrap();
    let first = cache.put_ssm(4, 1, &ssm(1)).unwrap();
    let second = cache.put_ssm(4, 1, &ssm(2)).unwrap();
    assert!(matches!(cache.get(&first), Err(StateError::Invalidated)));
    assert_eq!(cache.get(&second).unwrap(), ssm(2));
    assert_eq!(cache.lookup_ssm(4, 1).unwrap().hash, second.hash);
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn memory_reuse_drops_the_old_bytes() {
    let root = fresh_root();
    let cache = PersistentStateCache::open(&root).unwrap();
    let first = cache.put_ssm(8, 2, &ssm(3)).unwrap();
    cache.invalidate_memory_reuse(&first).unwrap();
    assert!(matches!(cache.get(&first), Err(StateError::Invalidated)));
    assert!(matches!(cache.lookup_ssm(8, 2), Err(StateError::NotFound)));
    let second = cache.put_ssm(8, 2, &ssm(4)).unwrap();
    assert_eq!(cache.get(&second).unwrap(), ssm(4));
    assert!(matches!(cache.get(&first), Err(StateError::Invalidated)));
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn concurrent_sessions_do_not_share_state() {
    let root = fresh_root();
    let cache = Arc::new(PersistentStateCache::open(&root).unwrap());
    let left = Arc::clone(&cache);
    let right = Arc::clone(&cache);
    let layer = 0;
    assert!(is_recurrent_layer(layer));
    let a = thread::spawn(move || left.put_ssm(11, layer, &ssm(11)).unwrap());
    let b = thread::spawn(move || right.put_ssm(12, layer, &ssm(12)).unwrap());
    let ref_a = a.join().unwrap();
    let ref_b = b.join().unwrap();
    assert_eq!(cache.get(&ref_a).unwrap(), ssm(11));
    assert_eq!(cache.get(&ref_b).unwrap(), ssm(12));
    assert_ne!(ref_a.hash, ref_b.hash);
    let mut foreign = ref_a;
    foreign.session = ref_b.session;
    assert!(matches!(cache.get(&foreign), Err(StateError::NotFound)));
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn session_teardown_leaves_the_other_session() {
    let root = fresh_root();
    let cache = PersistentStateCache::open(&root).unwrap();
    let kept = cache.put_ssm(21, 5, &ssm(5)).unwrap();
    let dropped = cache.put_ssm(22, 5, &ssm(6)).unwrap();
    cache.end_session(22).unwrap();
    assert!(matches!(
        cache.get(&dropped),
        Err(StateError::SessionClosed)
    ));
    assert_eq!(cache.get(&kept).unwrap(), ssm(5));
    assert!(matches!(
        cache.end_session(22),
        Err(StateError::SessionClosed)
    ));
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn kv_append_is_contiguous_and_bounded() {
    let root = fresh_root();
    let cache = PersistentStateCache::open(&root).unwrap();
    let layer = 3;
    assert!(is_attention_layer(layer));
    let span = vec![9u8; 320];
    let first = cache.append_kv(3, layer, 0, 0, &span).unwrap();
    let second = cache.append_kv(3, layer, 0, 1, &span).unwrap();
    assert_eq!(cache.get(&first).unwrap(), span);
    assert_eq!(cache.get(&second).unwrap(), span);
    assert!(matches!(
        cache.append_kv(3, layer, 0, 5, &span),
        Err(StateError::NotContiguous)
    ));
    assert!(matches!(
        cache.append_kv(3, layer, 0, 2, &vec![0u8; CHUNK_BYTES + 320]),
        Err(StateError::Bounded)
    ));
    assert!(matches!(
        cache.put_ssm(3, layer, &ssm(1)),
        Err(StateError::WrongLayerKind)
    ));
    assert!(matches!(
        cache.append_kv(3, 0, 0, 0, &span),
        Err(StateError::WrongLayerKind)
    ));
    cache.invalidate_memory_reuse(&first).unwrap();
    assert!(matches!(cache.get(&first), Err(StateError::Invalidated)));
    assert_eq!(cache.get(&second).unwrap(), span);
    assert!(matches!(
        cache.append_kv(3, layer, 0, 2, &span),
        Err(StateError::Invalidated)
    ));
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn partial_files_are_not_promoted_and_corrupt_records_do_not_reload() {
    let root = fresh_root();
    let bytes = ssm(8);
    let cache = PersistentStateCache::open(&root).unwrap();
    let reference = cache.put_ssm(30, 4, &bytes).unwrap();
    let session = root
        .join("sessions")
        .join(format!("{:016x}", reference.session));
    fs::write(
        session.join("tensors").join(".partial-not-a-record"),
        b"stale",
    )
    .unwrap();
    drop(cache);

    let reopened = PersistentStateCache::open(&root).unwrap();
    assert!(!session
        .join("tensors")
        .join(".partial-not-a-record")
        .exists());
    let current = reopened.lookup_ssm(30, 4).unwrap();
    assert_eq!(reopened.get(&current).unwrap(), bytes);
    drop(reopened);

    let mut flipped = None;
    for entry in fs::read_dir(session.join("tensors")).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("bin") {
            let mut data = fs::read(&path).unwrap();
            let last = data.len() - 1;
            data[last] ^= 0xff;
            fs::write(&path, data).unwrap();
            flipped = Some(path);
        }
    }
    assert!(flipped.is_some());
    let checked = PersistentStateCache::open(&root).unwrap();
    assert!(matches!(
        checked.lookup_ssm(30, 4),
        Err(StateError::NotFound)
    ));
    let _ = fs::remove_dir_all(&root);
}
