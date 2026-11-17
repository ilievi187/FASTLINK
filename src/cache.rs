//! Content-addressed chunk cache with validation, atomic writes, and invalidation.
//!
//! Chunks are stored in a two-level directory hierarchy under `chunks/xx/xx...`
//! where `xx` is the first two hex characters of the 32-byte BLAKE3 digest.
//!
//! Cache integrity:
//! - Writes are atomic via a temporary `.partial` file and sync before rename.
//! - Reads verify that file content matches the expected digest.
//! - Corrupted chunks are automatically detected and invalidated.
//! - Explicit invalidation removes specific chunks or purges invalid entries.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::{digest, hex, Digest, CHUNK_BYTES};

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

/// File modification time in nanoseconds since the epoch, for eviction ordering.
fn mtime_nanos(meta: &fs::Metadata) -> u128 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// Extract a 64-character hex digest filename, ignoring `.partial` and non-digest files.
fn chunk_digest_name(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    if name.len() == 64 && name.bytes().all(|b| b.is_ascii_hexdigit()) {
        Some(name.to_string())
    } else {
        None
    }
}

/// A content-addressed on-disk cache for verified 64 KiB tensor chunks.
#[derive(Debug, Clone)]
pub struct ChunkCache {
    root: PathBuf,
    max_bytes: Option<u64>,
}

impl ChunkCache {
    /// Initialize a cache at the given root directory, ensuring `chunks/` exists.
    pub fn new(root: impl Into<PathBuf>) -> io::Result<Self> {
        let root = root.into();
        fs::create_dir_all(root.join("chunks"))?;
        Ok(Self {
            root,
            max_bytes: None,
        })
    }

    /// Set an optional maximum storage quota in bytes.
    pub fn with_max_bytes(mut self, max: u64) -> Self {
        self.max_bytes = Some(max);
        self
    }

    /// Maximum storage quota in bytes, if one is configured.
    pub fn max_bytes(&self) -> Option<u64> {
        self.max_bytes
    }

    /// Collect valid chunk digest files (`chunks/<xx>/<64-hex>`) with mtime and byte length.
    fn collect_chunk_entries(&self) -> io::Result<Vec<(PathBuf, u128, String, u64)>> {
        let chunks_dir = self.root.join("chunks");
        if !chunks_dir.exists() {
            return Ok(Vec::new());
        }
        let mut entries = Vec::new();
        for entry in fs::read_dir(&chunks_dir)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            for sub in fs::read_dir(entry.path())? {
                let sub = sub?;
                let path = sub.path();
                if !sub.file_type()?.is_file() {
                    continue;
                }
                let Some(name) = chunk_digest_name(&path) else {
                    continue;
                };
                match sub.metadata() {
                    Ok(meta) => entries.push((path, mtime_nanos(&meta), name, meta.len())),
                    Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                    Err(e) => return Err(e),
                }
            }
        }
        Ok(entries)
    }

    fn evict_to_quota_internal(&self, keep_last: Option<&Path>) -> io::Result<usize> {
        let Some(max) = self.max_bytes else {
            return Ok(0);
        };
        let mut chunks = self.collect_chunk_entries()?;
        let mut total: u64 = chunks.iter().map(|(_, _, _, len)| *len).sum();
        if total <= max {
            return Ok(0);
        }
        // Evict oldest chunks first (ties broken by digest hex). During `store()`,
        // keep the just-written chunk last so identical filesystem mtime ticks do
        // not evict it prematurely; manual eviction can evict even the last chunk.
        chunks.sort_by(|a, b| {
            let a_keep = keep_last.is_some_and(|p| a.0 == p);
            let b_keep = keep_last.is_some_and(|p| b.0 == p);
            a_keep
                .cmp(&b_keep)
                .then_with(|| a.1.cmp(&b.1))
                .then_with(|| a.2.cmp(&b.2))
        });
        let mut evicted = 0;
        for (path, _, _, size) in chunks {
            if total <= max {
                break;
            }
            match fs::remove_file(&path) {
                Ok(()) => {
                    evicted += 1;
                    total = total.saturating_sub(size);
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    total = total.saturating_sub(size);
                }
                Err(e) => return Err(e),
            }
        }
        Ok(evicted)
    }

    /// Enforce the configured storage quota by evicting the oldest chunk files.
    ///
    /// Chunks are ordered by modification time (oldest first, ties broken by
    /// digest hex) and removed until stored valid chunk bytes are at or below
    /// `max_bytes`, including the newest/final chunk if needed when a quota is
    /// smaller than that chunk. Non-digest files such as `.partial` temporary
    /// files are ignored here (`purge_invalid` owns their lifecycle).
    pub fn evict_to_quota(&self) -> io::Result<usize> {
        self.evict_to_quota_internal(None)
    }

    /// Root directory of the cache.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Compute the hierarchical file path for a given chunk digest.
    pub fn digest_path(&self, hash: &Digest) -> PathBuf {
        let s = hex(hash);
        self.root.join("chunks").join(&s[..2]).join(s)
    }

    /// Check if a chunk exists in the cache and has a valid digest matching `expected`.
    pub fn is_valid(&self, expected: &Digest) -> bool {
        let path = self.digest_path(expected);
        match fs::read(&path) {
            Ok(bytes) if bytes.len() <= CHUNK_BYTES => digest(&bytes) == *expected,
            _ => false,
        }
    }

    /// Store a chunk in the cache atomically.
    ///
    /// Verifies that `bytes.len() <= CHUNK_BYTES`, `digest(bytes) == *hash`,
    /// and (when a quota is configured) `bytes.len() <= max_bytes` before
    /// creating directories or files. Writes first to a `.partial` sibling
    /// file, flushes to disk, and renames, then evicts older chunks if needed so
    /// stored valid chunk bytes remain at or below `max_bytes`.
    pub fn store(&self, hash: &Digest, bytes: &[u8]) -> io::Result<()> {
        if bytes.len() > CHUNK_BYTES {
            return Err(invalid("chunk exceeds CHUNK_BYTES limit"));
        }
        if digest(bytes) != *hash {
            return Err(invalid("chunk content does not match expected digest"));
        }
        if let Some(max) = self.max_bytes {
            if (bytes.len() as u64) > max {
                return Err(invalid("chunk exceeds configured max_bytes quota"));
            }
        }
        let path = self.digest_path(hash);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        if self.is_valid(hash) {
            if self.max_bytes.is_some() {
                self.evict_to_quota_internal(Some(&path))?;
            }
            return Ok(());
        }

        let tmp = path.with_extension("partial");
        let mut file = File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, &path)?;
        if self.max_bytes.is_some() {
            self.evict_to_quota_internal(Some(&path))?;
        }
        Ok(())
    }

    /// Retrieve a chunk from the cache.
    ///
    /// If the chunk exists and is valid, returns `Ok(Some(bytes))`.
    /// If the chunk file exists on disk but is corrupt (digest mismatch or length out of bounds),
    /// it automatically invalidates (deletes) the corrupt file and returns `Ok(None)`.
    /// If the chunk does not exist, returns `Ok(None)`.
    pub fn get(&self, hash: &Digest) -> io::Result<Option<Vec<u8>>> {
        let path = self.digest_path(hash);
        if !path.exists() {
            return Ok(None);
        }
        match fs::read(&path) {
            Ok(bytes) => {
                if bytes.len() <= CHUNK_BYTES && digest(&bytes) == *hash {
                    Ok(Some(bytes))
                } else {
                    // Corrupted on disk: invalidate immediately.
                    let _ = fs::remove_file(&path);
                    Ok(None)
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Invalidate (delete) a specific chunk from the cache.
    ///
    /// Returns `Ok(true)` if the chunk file was present and removed, `Ok(false)` if absent.
    pub fn invalidate(&self, hash: &Digest) -> io::Result<bool> {
        let path = self.digest_path(hash);
        match fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Scan all chunk files in the cache and purge (delete) any that are corrupted,
    /// truncated, or have digests that do not match their filename.
    ///
    /// Returns the count of purged invalid chunk files.
    pub fn purge_invalid(&self) -> io::Result<usize> {
        let chunks_dir = self.root.join("chunks");
        if !chunks_dir.exists() {
            return Ok(0);
        }
        let mut purged = 0;
        for entry in fs::read_dir(&chunks_dir)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                for sub in fs::read_dir(entry.path())? {
                    let sub = sub?;
                    let path = sub.path();
                    if path.is_file() {
                        let filename = match path.file_name().and_then(|f| f.to_str()) {
                            Some(s) if s.len() == 64 => s,
                            _ => {
                                // Non-digest file (or stale .partial): remove
                                let _ = fs::remove_file(&path);
                                purged += 1;
                                continue;
                            }
                        };
                        let bytes = match fs::read(&path) {
                            Ok(b) => b,
                            Err(_) => {
                                let _ = fs::remove_file(&path);
                                purged += 1;
                                continue;
                            }
                        };
                        let d = digest(&bytes);
                        let computed_hex = hex(&d);
                        if bytes.len() > CHUNK_BYTES || computed_hex != filename {
                            let _ = fs::remove_file(&path);
                            purged += 1;
                        }
                    }
                }
            }
        }
        Ok(purged)
    }

    /// Calculate total stored bytes across all valid chunk files.
    pub fn total_bytes(&self) -> io::Result<u64> {
        let chunks_dir = self.root.join("chunks");
        if !chunks_dir.exists() {
            return Ok(0);
        }
        let mut total = 0u64;
        for entry in fs::read_dir(&chunks_dir)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                for sub in fs::read_dir(entry.path())? {
                    let sub = sub?;
                    if sub.file_type()?.is_file() {
                        total += sub.metadata()?.len();
                    }
                }
            }
        }
        Ok(total)
    }

    /// Count total cached chunks.
    pub fn count(&self) -> io::Result<usize> {
        let chunks_dir = self.root.join("chunks");
        if !chunks_dir.exists() {
            return Ok(0);
        }
        let mut total = 0;
        for entry in fs::read_dir(&chunks_dir)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                for sub in fs::read_dir(entry.path())? {
                    let sub = sub?;
                    if sub.file_type()?.is_file() {
                        total += 1;
                    }
                }
            }
        }
        Ok(total)
    }
}
