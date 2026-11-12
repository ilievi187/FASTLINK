//! Discard uncommitted atomic-write leftovers without promoting them.
//!
//! WP-D temp names are `chunks/**/.partial-*` and `outputs/.fastlink-inprogress-*`.
//! Those names are never renamed into a cache object or a final output.

use std::fs;
use std::io;
use std::path::Path;

/// Counts of uncommitted temp files removed by [`recover_atomic_writes`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecoveryReport {
    pub partial_chunks_removed: u64,
    pub in_progress_outputs_removed: u64,
}

/// Discard uncommitted atomic-write leftovers.
///
/// Removes `chunks/**/.partial-*` and `outputs/.fastlink-inprogress-*`.
/// Those names are never renamed into a cache object or a final output.
/// Verified chunk files and already committed outputs are left in place.
/// Symlinks are not followed. A directory sitting on a temp name is an error.
pub fn recover_atomic_writes(root: &Path) -> io::Result<RecoveryReport> {
    let mut report = RecoveryReport {
        partial_chunks_removed: 0,
        in_progress_outputs_removed: 0,
    };
    let chunks = root.join("chunks");
    if chunks.exists() {
        remove_prefixed(&chunks, 0, ".partial-", &mut report.partial_chunks_removed)?;
    }
    let outputs = root.join("outputs");
    if outputs.exists() {
        ensure_not_symlink(&outputs)?;
        for entry in fs::read_dir(&outputs)? {
            let entry = entry?;
            if !os_starts_with(&entry.file_name(), ".fastlink-inprogress-") {
                continue;
            }
            remove_uncommitted(&entry)?;
            report.in_progress_outputs_removed += 1;
        }
    }
    Ok(report)
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn os_starts_with(name: &std::ffi::OsStr, prefix: &str) -> bool {
    name.to_string_lossy().starts_with(prefix)
}

fn ensure_not_symlink(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(md) if md.file_type().is_symlink() => {
            Err(invalid("refusing to follow a symlink in the data root"))
        }
        _ => Ok(()),
    }
}

fn remove_uncommitted(entry: &fs::DirEntry) -> io::Result<()> {
    let file_type = entry.file_type()?;
    let path = entry.path();
    if file_type.is_symlink() || file_type.is_file() {
        // `remove_file` on a symlink removes the link, not its target.
        fs::remove_file(path)?;
        return Ok(());
    }
    if file_type.is_dir() {
        return Err(invalid("uncommitted transfer path is a directory"));
    }
    Err(invalid("uncommitted transfer path has an unknown type"))
}

fn remove_prefixed(dir: &Path, depth: u32, prefix: &str, count: &mut u64) -> io::Result<()> {
    if depth > 3 {
        return Ok(());
    }
    ensure_not_symlink(dir)?;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let prefixed = os_starts_with(&entry.file_name(), prefix);
        if file_type.is_dir() {
            if prefixed {
                return Err(invalid("uncommitted transfer path is a directory"));
            }
            remove_prefixed(&entry.path(), depth + 1, prefix, count)?;
            continue;
        }
        if prefixed {
            remove_uncommitted(&entry)?;
            *count += 1;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::recover_atomic_writes;
    use crate::{cached_and_valid, digest, digest_path};
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_root() -> std::path::PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("fastlink-recover-{}-{}", std::process::id(), id));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("chunks")).unwrap();
        fs::create_dir_all(root.join("outputs")).unwrap();
        root
    }

    #[test]
    fn recover_discards_partials_and_keeps_committed_output() {
        let root = temp_root();
        let chunk = vec![3u8; 32];
        let hash = digest(&chunk);
        let committed_path = digest_path(&root, &hash);
        fs::create_dir_all(committed_path.parent().unwrap()).unwrap();
        fs::write(&committed_path, &chunk).unwrap();
        let shard = committed_path.parent().unwrap().to_path_buf();
        let partial = shard.join(".partial-9-aabbccddeeff");
        fs::write(&partial, &chunk).unwrap();
        let orphan_dir = root.join("chunks").join("ff");
        fs::create_dir_all(&orphan_dir).unwrap();
        let orphan = orphan_dir.join(".partial-1-001122334455");
        fs::write(&orphan, &chunk).unwrap();
        let out = root.join("outputs").join("out.bin");
        fs::write(&out, b"committed").unwrap();
        let stale = root.join("outputs").join(".fastlink-inprogress-out.bin");
        fs::write(&stale, b"do-not-promote").unwrap();

        let report = recover_atomic_writes(&root).unwrap();
        assert_eq!(report.partial_chunks_removed, 2);
        assert_eq!(report.in_progress_outputs_removed, 1);
        assert!(!partial.exists());
        assert!(!orphan.exists());
        let promoted: Vec<_> = fs::read_dir(&orphan_dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name())
            .collect();
        assert!(
            promoted.is_empty(),
            "a partial must not be promoted into the cache: {promoted:?}"
        );
        assert!(!stale.exists());
        assert_eq!(fs::read(&out).unwrap(), b"committed");
        assert!(cached_and_valid(&digest_path(&root, &hash), &hash));

        let again = recover_atomic_writes(&root).unwrap();
        assert_eq!(again.partial_chunks_removed, 0);
        assert_eq!(again.in_progress_outputs_removed, 0);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn recover_does_not_promote_in_progress_when_output_is_absent() {
        let root = temp_root();
        let stale = root.join("outputs").join(".fastlink-inprogress-out.bin");
        fs::write(&stale, vec![9u8; 32]).unwrap();
        recover_atomic_writes(&root).unwrap();
        assert!(!stale.exists());
        assert!(!root.join("outputs").join("out.bin").exists());
        fs::remove_dir_all(&root).ok();
    }
}
