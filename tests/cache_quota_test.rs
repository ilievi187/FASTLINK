//! Quota enforcement and eviction tests for [`ChunkCache`].

use fastlink::cache::ChunkCache;
use fastlink::digest;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

fn temp_cache_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "fastlink_cache_quota_{name}_{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    dir
}

fn store_sized_chunk(cache: &ChunkCache, seed: u8, len: usize) -> fastlink::Digest {
    let bytes = vec![seed; len];
    let hash = digest(&bytes);
    cache.store(&hash, &bytes).expect("store should succeed");
    hash
}

fn set_mtime_secs(path: &Path, secs: u64) {
    let file = fs::File::options()
        .write(true)
        .open(path)
        .expect("open chunk for mtime");
    file.set_modified(UNIX_EPOCH + Duration::from_secs(secs))
        .expect("set chunk mtime");
}

#[test]
fn store_rejects_chunk_exceeding_quota_before_writing_files() {
    let dir = temp_cache_dir("reject_oversize");
    let cache = ChunkCache::new(&dir).expect("cache").with_max_bytes(512);
    let bytes = vec![0x42u8; 1024];
    let hash = digest(&bytes);
    let final_path = cache.digest_path(&hash);
    let partial_path = final_path.with_extension("partial");

    let err = cache
        .store(&hash, &bytes)
        .expect_err("chunk larger than max_bytes quota must be rejected");
    assert_eq!(err.kind(), ErrorKind::InvalidData);
    assert!(
        !final_path.exists(),
        "rejected chunk must not create final cache file"
    );
    assert!(
        !partial_path.exists(),
        "rejected chunk must not create .partial file"
    );
    assert!(
        !final_path.parent().expect("shard dir").exists(),
        "rejected chunk must fail before creating shard directory"
    );
    assert_eq!(cache.total_bytes().expect("total"), 0);
    assert!(cache.total_bytes().expect("total") <= 512);
    assert_eq!(cache.count().expect("count"), 0);

    // Non-empty chunk must also be rejected when max_bytes is zero.
    let zero_dir = temp_cache_dir("reject_nonzero_on_zero_quota");
    let zero_cache = ChunkCache::new(&zero_dir).expect("cache").with_max_bytes(0);
    let one_byte = [0xAAu8];
    let one_hash = digest(&one_byte);
    let err_zero = zero_cache
        .store(&one_hash, &one_byte)
        .expect_err("non-empty chunk must fail when max_bytes is zero");
    assert_eq!(err_zero.kind(), ErrorKind::InvalidData);
    assert!(!zero_cache.digest_path(&one_hash).exists());
    assert!(!zero_cache
        .digest_path(&one_hash)
        .with_extension("partial")
        .exists());
    assert_eq!(zero_cache.total_bytes().expect("total"), 0);

    let _ = fs::remove_dir_all(&dir);
    let _ = fs::remove_dir_all(&zero_dir);
}

#[test]
fn shrunk_quota_evicts_lone_oversize_chunk() {
    let dir = temp_cache_dir("shrunk_lone_oversize");
    let unquotaed = ChunkCache::new(dir.clone()).expect("cache");
    let hash = store_sized_chunk(&unquotaed, 9, 1024);
    assert_eq!(unquotaed.total_bytes().expect("total"), 1024);

    // Shrinking quota below the single existing chunk must allow manual eviction
    // to remove even the sole/newest chunk so total_bytes <= max.
    let shrunk = ChunkCache::new(dir.clone())
        .expect("cache")
        .with_max_bytes(512);
    assert_eq!(shrunk.max_bytes(), Some(512));
    let evicted = shrunk.evict_to_quota().expect("evict");
    assert_eq!(evicted, 1);
    assert_eq!(shrunk.total_bytes().expect("total"), 0);
    assert!(shrunk.total_bytes().expect("total") <= 512);
    assert_eq!(shrunk.count().expect("count"), 0);
    assert!(shrunk.get(&hash).expect("get").is_none());

    // Shrinking to zero over a single non-empty chunk must also evict it.
    let zero_dir = temp_cache_dir("shrunk_zero");
    let zero_unquotaed = ChunkCache::new(zero_dir.clone()).expect("cache");
    let h0 = store_sized_chunk(&zero_unquotaed, 7, 256);
    let zero_shrunk = ChunkCache::new(zero_dir.clone())
        .expect("cache")
        .with_max_bytes(0);
    assert_eq!(zero_shrunk.evict_to_quota().expect("evict"), 1);
    assert_eq!(zero_shrunk.total_bytes().expect("total"), 0);
    assert!(zero_shrunk.get(&h0).expect("get").is_none());

    let _ = fs::remove_dir_all(&dir);
    let _ = fs::remove_dir_all(&zero_dir);
}

#[test]
fn shrunk_quota_evicts_oldest_first_and_last_if_still_over_bound() {
    let dir = temp_cache_dir("shrunk_multi");
    let cache = ChunkCache::new(dir.clone()).expect("cache");
    let h1 = store_sized_chunk(&cache, 1, 1024);
    let h2 = store_sized_chunk(&cache, 2, 1024);
    let h3 = store_sized_chunk(&cache, 3, 1024);
    set_mtime_secs(&cache.digest_path(&h1), 1_000);
    set_mtime_secs(&cache.digest_path(&h2), 2_000);
    set_mtime_secs(&cache.digest_path(&h3), 3_000);

    // Shrink first to 1024: oldest two (h1, h2) evicted, newest h3 retained.
    let mid = ChunkCache::new(dir.clone())
        .expect("cache")
        .with_max_bytes(1024);
    assert_eq!(mid.evict_to_quota().expect("evict mid"), 2);
    assert_eq!(mid.total_bytes().expect("total"), 1024);
    assert!(mid.get(&h1).expect("get").is_none());
    assert!(mid.get(&h2).expect("get").is_none());
    assert!(mid.get(&h3).expect("get").is_some());

    // Shrink below h3's own size: even the final chunk must be evicted.
    let tight = ChunkCache::new(dir.clone())
        .expect("cache")
        .with_max_bytes(512);
    assert_eq!(tight.evict_to_quota().expect("evict tight"), 1);
    assert_eq!(tight.total_bytes().expect("total"), 0);
    assert!(tight.get(&h3).expect("get").is_none());

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn no_quota_means_no_eviction() {
    let dir = temp_cache_dir("no_quota");
    let cache = ChunkCache::new(dir.clone()).expect("cache");
    assert_eq!(cache.max_bytes(), None);
    store_sized_chunk(&cache, 1, 1024);
    store_sized_chunk(&cache, 2, 1024);
    store_sized_chunk(&cache, 3, 1024);
    assert_eq!(cache.count().expect("count"), 3);
    assert_eq!(cache.total_bytes().expect("total"), 3072);
    assert_eq!(cache.evict_to_quota().expect("evict"), 0);
    assert_eq!(cache.count().expect("count"), 3);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn quota_evicts_oldest_first_and_retains_newest_when_it_fits() {
    let dir = temp_cache_dir("store_evicts_oldest");
    let cache = ChunkCache::new(dir.clone())
        .expect("cache")
        .with_max_bytes(2048);
    let h1 = store_sized_chunk(&cache, 1, 1024);
    set_mtime_secs(&cache.digest_path(&h1), 1_000);
    let h2 = store_sized_chunk(&cache, 2, 1024);
    set_mtime_secs(&cache.digest_path(&h2), 2_000);

    // Storing a third chunk within quota triggers oldest-first eviction so total
    // remains <= 2048 and the newest chunk (h3) stays cached.
    let h3 = store_sized_chunk(&cache, 3, 1024);
    assert_eq!(cache.total_bytes().expect("total"), 2048);
    assert!(cache.total_bytes().expect("total") <= 2048);
    assert_eq!(cache.count().expect("count"), 2);
    assert!(cache.get(&h1).expect("get h1").is_none());
    assert!(cache.get(&h2).expect("get h2").is_some());
    assert!(cache.get(&h3).expect("get h3").is_some());

    // Even when all files have the exact same filesystem mtime tick, storing a
    // newer chunk that fits must keep the just-stored chunk.
    let tie_dir = temp_cache_dir("store_evicts_tie");
    let tie = ChunkCache::new(tie_dir.clone())
        .expect("cache")
        .with_max_bytes(1024);
    let ta = store_sized_chunk(&tie, 11, 1024);
    set_mtime_secs(&tie.digest_path(&ta), 5_000);
    let tb = store_sized_chunk(&tie, 12, 1024);
    assert!(tie.total_bytes().expect("total") <= 1024);
    assert!(tie.get(&tb).expect("get newest").is_some());
    assert!(tie.get(&ta).expect("get old").is_none());

    let _ = fs::remove_dir_all(&dir);
    let _ = fs::remove_dir_all(&tie_dir);
}

#[test]
fn duplicate_store_within_quota_preserves_chunks_and_bounds() {
    let dir = temp_cache_dir("duplicate_within_quota");
    let cache = ChunkCache::new(dir.clone())
        .expect("cache")
        .with_max_bytes(2048);
    let bytes1 = vec![0x11u8; 1024];
    let h1 = digest(&bytes1);
    let h2 = store_sized_chunk(&cache, 0x22, 1024);
    cache.store(&h1, &bytes1).expect("initial store");
    // Re-storing duplicate existing chunk must remain valid and within quota.
    cache.store(&h1, &bytes1).expect("duplicate store");
    assert_eq!(cache.count().expect("count"), 2);
    assert_eq!(cache.total_bytes().expect("total"), 2048);
    assert!(cache.get(&h1).expect("get h1").is_some());
    assert!(cache.get(&h2).expect("get h2").is_some());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn zero_length_chunk_allowed_within_zero_quota() {
    let dir = temp_cache_dir("zero_len");
    let cache = ChunkCache::new(dir.clone())
        .expect("cache")
        .with_max_bytes(0);
    let empty: [u8; 0] = [];
    let hash = digest(&empty);
    cache
        .store(&hash, &empty)
        .expect("empty chunk fits in zero quota");
    assert_eq!(cache.total_bytes().expect("total"), 0);
    assert_eq!(cache.count().expect("count"), 1);
    assert_eq!(cache.evict_to_quota().expect("evict"), 0);
    assert_eq!(cache.get(&hash).expect("get"), Some(Vec::new()));
    let _ = fs::remove_dir_all(&dir);
}
