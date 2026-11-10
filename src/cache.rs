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
    /// Verifies that `bytes.len() <= CHUNK_BYTES` and that `digest(bytes) == *hash`.
    /// Writes first to a `.partial` sibling file, flushes to disk, and renames.
    pub fn store(&self, hash: &Digest, bytes: &[u8]) -> io::Result<()> {
        if bytes.len() > CHUNK_BYTES {
            return Err(invalid("chunk exceeds CHUNK_BYTES limit"));
        }
        if digest(bytes) != *hash {
            return Err(invalid("chunk content does not match expected digest"));
        }
        let path = self.digest_path(hash);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        if self.is_valid(hash) {
            return Ok(());
        }

        let tmp = path.with_extension("partial");
        let mut file = File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, &path)?;
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
