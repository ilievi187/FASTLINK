//! Integration tests for ChunkCache and cache invalidation logic.

use fastlink::cache::ChunkCache;
use fastlink::{digest, Digest, CHUNK_BYTES};
use std::fs;
use std::path::PathBuf;

fn temp_cache_dir(name: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("fastlink_cache_test_{name}_{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    dir
}

fn sample_chunk(fill: u8, len: usize) -> (Digest, Vec<u8>) {
    let bytes = vec![fill; len];
    let d = digest(&bytes);
    (d, bytes)
}

#[test]
fn store_and_retrieve_valid_chunk() {
    let dir = temp_cache_dir("store_retrieve");
    let cache = ChunkCache::new(&dir).expect("init cache");
    let (hash, chunk) = sample_chunk(0x42, CHUNK_BYTES);

    assert!(!cache.is_valid(&hash));
    cache.store(&hash, &chunk).expect("store chunk");
    assert!(cache.is_valid(&hash));

    let retrieved = cache.get(&hash).expect("get chunk").expect("chunk present");
    assert_eq!(retrieved, chunk);

    // Storing identical chunk again is a safe no-op
    cache.store(&hash, &chunk).expect("idempotent store");
    assert!(cache.is_valid(&hash));

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn automatic_invalidation_on_content_corruption() {
    let dir = temp_cache_dir("corrupt_invalidation");
    let cache = ChunkCache::new(&dir).expect("init cache");
    let (hash, chunk) = sample_chunk(0xAA, CHUNK_BYTES);

    cache.store(&hash, &chunk).expect("store chunk");
    assert!(cache.is_valid(&hash));

    // Tamper with one byte directly on disk
    let file_path = cache.digest_path(&hash);
    let mut on_disk = fs::read(&file_path).expect("read file");
    on_disk[42] ^= 0xFF; // flip bits
    fs::write(&file_path, &on_disk).expect("write corrupted file");

    // is_valid must detect the digest failure
    assert!(!cache.is_valid(&hash));

    // get() must detect corruption, delete the file, and return None
    let result = cache.get(&hash).expect("get chunk");
    assert!(result.is_none(), "corrupt chunk must not be returned");
    assert!(!file_path.exists(), "corrupt chunk file must be removed");

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn automatic_invalidation_on_truncated_file() {
    let dir = temp_cache_dir("truncated_invalidation");
    let cache = ChunkCache::new(&dir).expect("init cache");
    let (hash, chunk) = sample_chunk(0xBB, CHUNK_BYTES);

    cache.store(&hash, &chunk).expect("store chunk");
    let file_path = cache.digest_path(&hash);

    // Truncate file on disk to 16 bytes
    fs::write(&file_path, &chunk[..16]).expect("truncate file");

    assert!(!cache.is_valid(&hash));
    let result = cache.get(&hash).expect("get chunk");
    assert!(result.is_none());
    assert!(!file_path.exists(), "truncated file must be purged");

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn explicit_invalidation() {
    let dir = temp_cache_dir("explicit_invalidation");
    let cache = ChunkCache::new(&dir).expect("init cache");
    let (hash, chunk) = sample_chunk(0xCC, CHUNK_BYTES);

    cache.store(&hash, &chunk).expect("store chunk");
    assert!(cache.is_valid(&hash));

    // Explicit invalidation deletes file and returns true
    assert!(cache.invalidate(&hash).expect("invalidate"));
    assert!(!cache.is_valid(&hash));

    // Second invalidation returns false (already gone)
    assert!(!cache.invalidate(&hash).expect("invalidate again"));

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn purge_invalid_scans_and_repairs_cache() {
    let dir = temp_cache_dir("purge_invalid");
    let cache = ChunkCache::new(&dir).expect("init cache");

    let mut valid_hashes = Vec::new();
    for i in 0..5u8 {
        let (hash, chunk) = sample_chunk(i + 1, CHUNK_BYTES);
        cache.store(&hash, &chunk).expect("store");
        valid_hashes.push(hash);
    }
    assert_eq!(cache.count().unwrap(), 5);

    // Corrupt chunk 1 and chunk 3 on disk
    let corrupt_path_1 = cache.digest_path(&valid_hashes[1]);
    fs::write(&corrupt_path_1, b"corrupted bytes").expect("write corrupt 1");

    let corrupt_path_3 = cache.digest_path(&valid_hashes[3]);
    let mut bytes_3 = fs::read(&corrupt_path_3).unwrap();
    bytes_3[0] ^= 0x01;
    fs::write(&corrupt_path_3, &bytes_3).expect("write corrupt 3");

    // Add an invalid non-digest file (e.g. leftover temp file)
    let bad_dir = cache.root().join("chunks").join("01");
    fs::create_dir_all(&bad_dir).unwrap();
    let bad_file = bad_dir.join("stale.partial");
    fs::write(&bad_file, b"stale partial").unwrap();

    // Purge invalid
    let purged = cache.purge_invalid().expect("purge");
    assert_eq!(purged, 3, "must purge 2 corrupt chunks and 1 stale file");

    // The remaining 3 chunks must be valid
    assert!(cache.is_valid(&valid_hashes[0]));
    assert!(!cache.is_valid(&valid_hashes[1]));
    assert!(cache.is_valid(&valid_hashes[2]));
    assert!(!cache.is_valid(&valid_hashes[3]));
    assert!(cache.is_valid(&valid_hashes[4]));
    assert_eq!(cache.count().unwrap(), 3);

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn delta_sync_with_cache_invalidation_and_repair() {
    let dir = temp_cache_dir("delta_sync_repair");
    let cache = ChunkCache::new(&dir).expect("init cache");

    // File with 4 chunks
    let chunks: Vec<(Digest, Vec<u8>)> = (0..4u8)
        .map(|i| sample_chunk(i + 10, CHUNK_BYTES))
        .collect();

    // Initial sync: store all 4 chunks
    for (hash, data) in &chunks {
        cache.store(hash, data).expect("store initial chunk");
    }
    assert_eq!(cache.count().unwrap(), 4);

    // Simulate disk corruption on chunk index 2
    let corrupt_hash = &chunks[2].0;
    let corrupt_path = cache.digest_path(corrupt_hash);
    fs::write(&corrupt_path, b"corrupted payload").unwrap();

    // Delta sync pass: identify missing or invalid chunks
    let mut missing_indices = Vec::new();
    for (idx, (hash, _)) in chunks.iter().enumerate() {
        if !cache.is_valid(hash) {
            missing_indices.push(idx);
        }
    }

    // Only chunk index 2 must be requested
    assert_eq!(missing_indices, vec![2]);

    // Client sends missing chunk; cache stores and repairs it
    for idx in missing_indices {
        let (hash, ref data) = chunks[idx];
        cache.store(&hash, data).expect("repair chunk");
    }

    // Now all 4 chunks are fully valid
    for (hash, _) in &chunks {
        assert!(cache.is_valid(hash));
    }
    assert_eq!(cache.count().unwrap(), 4);

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn store_rejects_hash_mismatch_and_oversize() {
    let dir = temp_cache_dir("reject_invalid");
    let cache = ChunkCache::new(&dir).expect("init cache");

    let chunk = vec![0xEE; CHUNK_BYTES];
    let fake_hash = [0xFF; 32];

    // Wrong digest: rejected
    let err = cache.store(&fake_hash, &chunk);
    assert!(err.is_err());
    assert_eq!(err.unwrap_err().kind(), std::io::ErrorKind::InvalidData);

    // Oversized chunk: rejected
    let big_chunk = vec![0xAA; CHUNK_BYTES + 1];
    let big_hash = digest(&big_chunk);
    let err = cache.store(&big_hash, &big_chunk);
    assert!(err.is_err());
    assert_eq!(err.unwrap_err().kind(), std::io::ErrorKind::InvalidData);

    let _ = fs::remove_dir_all(&dir);
}
