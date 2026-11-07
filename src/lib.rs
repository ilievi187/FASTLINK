//! FASTLINK: conservative, lossless content-addressed transfer primitives.
//! This is an independent experimental protocol, **not** a GPU driver.
//!
//! The wire/session logic lives in the library so it can be exercised by
//! integration tests over a real loopback TCP connection (`tests/protocol.rs`).
//!
//! Wire format v0.1 — see `docs/PROTOCOL.md`.

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

pub const MAGIC: &[u8; 4] = b"FLNK";
pub const VERSION: u16 = 1;
pub const CHUNK_BYTES: usize = 64 * 1024;
pub const MAX_CHUNKS: u32 = 1_000_000;
pub const MAX_FILE_BYTES: u64 = CHUNK_BYTES as u64 * MAX_CHUNKS as u64;
pub const MAX_ENCODED_BYTES: usize = CHUNK_BYTES * 2;
/// The longest file name accepted by the server. Basenames only.
pub const MAX_NAME_BYTES: u16 = 240;
/// Message ID for a SYNC operation.
pub const CMD_SYNC: u8 = 1;
/// Terminal status byte meaning the session completed and output was committed.
pub const STATUS_OK: u8 = 0;

pub type Digest = [u8; 32];

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

/// BLAKE3 digest used to address cache chunks.
pub fn digest(data: &[u8]) -> Digest {
    *blake3::hash(data).as_bytes()
}

/// Lowercase hex of a digest (64 characters).
pub fn hex(hash: &Digest) -> String {
    blake3::Hash::from(*hash).to_hex().to_string()
}

/// Number of bytes in `index` for a file of `total_bytes` (64 KiB fixed chunks).
pub fn expected_chunk_len(total_bytes: u64, index: u32) -> io::Result<usize> {
    let start = index as u64 * CHUNK_BYTES as u64;
    if start >= total_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid chunk index",
        ));
    }
    Ok((total_bytes - start).min(CHUNK_BYTES as u64) as usize)
}

/// Number of fixed-size chunks in a file of `total_bytes`, enforcing the cap.
pub fn chunk_count(total_bytes: u64) -> io::Result<u32> {
    if total_bytes > MAX_FILE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "file exceeds transfer limit",
        ));
    }
    let count = total_bytes.div_ceil(CHUNK_BYTES as u64);
    u32::try_from(count).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "too many chunks"))
}

pub const RAW: u8 = 0;
pub const RLE: u8 = 1;

/// A lossless RLE codec: `(run: u8, value: u8)` pairs with runs of 1..=255.
/// The payload is used only when it is genuinely smaller than the raw chunk.
pub fn encode_chunk(chunk: &[u8]) -> (u8, Vec<u8>) {
    // A tiny lossless codec for easy-to-test sparse/uniform data.
    // Future milestone: benchmark LZ4/Zstd and choose by measured wall-clock time.
    let mut encoded = Vec::new();
    let mut idx = 0;
    while idx < chunk.len() {
        let byte = chunk[idx];
        let mut count = 1usize;
        while idx + count < chunk.len() && count < u8::MAX as usize && chunk[idx + count] == byte {
            count += 1;
        }
        encoded.push(count as u8);
        encoded.push(byte);
        idx += count;
    }
    if encoded.len().saturating_add(32) < chunk.len() {
        (RLE, encoded)
    } else {
        (RAW, chunk.to_vec())
    }
}

/// Decode a chunk, enforcing the expected size and the codec's own bounds.
///
/// RAW must be exactly `expected` bytes. RLE must be an even-length stream of
/// `(run, value)` pairs, at most `2 * expected` bytes, and must expand to
/// exactly `expected` bytes. `expected` cannot exceed one chunk.
pub fn decode_chunk(codec: u8, bytes: &[u8], expected: usize) -> io::Result<Vec<u8>> {
    if expected > CHUNK_BYTES || bytes.len() > MAX_ENCODED_BYTES {
        return Err(invalid("chunk exceeds limit"));
    }
    match codec {
        RAW => {
            if bytes.len() != expected {
                return Err(invalid("decoded size mismatch"));
            }
            Ok(bytes.to_vec())
        }
        RLE => {
            // Each output byte costs at most one (run, value) pair.
            if bytes.len() > expected.saturating_mul(2) || !bytes.len().is_multiple_of(2) {
                return Err(invalid("invalid RLE payload"));
            }
            let mut output = Vec::with_capacity(expected);
            for pair in bytes.as_chunks::<2>().0 {
                let run = pair[0] as usize;
                if run == 0 || output.len() + run > expected {
                    return Err(invalid("invalid RLE run length"));
                }
                output.extend(std::iter::repeat_n(pair[1], run));
            }
            if output.len() != expected {
                return Err(invalid("decoded size mismatch"));
            }
            Ok(output)
        }
        _ => Err(invalid("unknown codec")),
    }
}

/// Accept only plain basenames. Rejects empty names, `.`, `..`, overlong
/// names, path separators (POSIX and Windows) and NUL/control bytes, so
/// rejected input can never create files outside the cache/output root.
pub fn safe_basename(name: &str) -> io::Result<&str> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.len() > MAX_NAME_BYTES as usize
        || name.contains('/')
        || name.contains('\\')
        || name.contains('\0')
        || !name
            .chars()
            .all(|c| (c as u32) > 0x1f && (c as u32) != 0x7f)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsafe or overlong file name",
        ));
    }
    Ok(name)
}

/// Pre-alpha guard: the unauthenticated listener may only bind to loopback.
/// Remote use must go through an authenticated, encrypted tunnel.
pub fn validate_listen_address(address: &str) -> io::Result<SocketAddr> {
    let socket: SocketAddr = address
        .parse()
        .map_err(|_| invalid("invalid listen address"))?;
    if !socket.ip().is_loopback() {
        return Err(invalid(
            "pre-alpha server must bind to loopback; use SSH forwarding for remote hosts",
        ));
    }
    Ok(socket)
}

/// Defence-in-depth guard: the client may only connect to a loopback peer.
/// This stops the unauthenticated protocol being pointed at a non-loopback
/// host by mistake. It is not a substitute for authentication/encryption.
pub fn validate_connect_address(server: &str) -> io::Result<SocketAddr> {
    let socket: SocketAddr = server
        .parse()
        .map_err(|_| invalid("invalid server address"))?;
    if !socket.ip().is_loopback() {
        return Err(invalid(
            "pre-alpha client may only connect to loopback; use an SSH tunnel for remote hosts",
        ));
    }
    Ok(socket)
}

pub fn read_u8(r: &mut impl Read) -> io::Result<u8> {
    let mut bytes = [0u8; 1];
    r.read_exact(&mut bytes)?;
    Ok(bytes[0])
}
pub fn read_u16(r: &mut impl Read) -> io::Result<u16> {
    let mut bytes = [0u8; 2];
    r.read_exact(&mut bytes)?;
    Ok(u16::from_le_bytes(bytes))
}
pub fn read_u32(r: &mut impl Read) -> io::Result<u32> {
    let mut bytes = [0u8; 4];
    r.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}
pub fn read_u64(r: &mut impl Read) -> io::Result<u64> {
    let mut bytes = [0u8; 8];
    r.read_exact(&mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}
pub fn write_u8(w: &mut impl Write, value: u8) -> io::Result<()> {
    w.write_all(&[value])
}
pub fn write_u16(w: &mut impl Write, value: u16) -> io::Result<()> {
    w.write_all(&value.to_le_bytes())
}
pub fn write_u32(w: &mut impl Write, value: u32) -> io::Result<()> {
    w.write_all(&value.to_le_bytes())
}
pub fn write_u64(w: &mut impl Write, value: u64) -> io::Result<()> {
    w.write_all(&value.to_le_bytes())
}

/// A parsed and bounds-checked SYNC request.
#[derive(Debug, Clone)]
pub struct Manifest {
    pub name: String,
    pub size: u64,
    pub count: u32,
    pub hashes: Vec<Digest>,
}

impl Manifest {
    /// Recompute BLAKE3 digests for each chunk of a local source file.
    pub fn hashes_from_file(path: &Path, size: u64, count: u32) -> io::Result<Vec<Digest>> {
        let mut file = File::open(path)?;
        let mut hashes = Vec::<Digest>::with_capacity(count as usize);
        for index in 0..count {
            let len = expected_chunk_len(size, index)?;
            let mut chunk = vec![0u8; len];
            file.seek(SeekFrom::Start(index as u64 * CHUNK_BYTES as u64))?;
            file.read_exact(&mut chunk)?;
            hashes.push(digest(&chunk));
        }
        Ok(hashes)
    }
}

/// Parse a SYNC manifest from the wire, enforcing every documented bound.
///
/// `chunk_bytes` and `count` must both be consistent with `size` and with the
/// protocol constants; a manifest whose declared chunk size or count does not
/// match `chunk_count(size)` is rejected before any hashes are read.
pub fn read_manifest(r: &mut impl Read) -> io::Result<Manifest> {
    let name_len = read_u16(r)? as usize;
    if name_len == 0 || name_len > MAX_NAME_BYTES as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid filename length",
        ));
    }
    let mut name_data = vec![0u8; name_len];
    r.read_exact(&mut name_data)?;
    let name = String::from_utf8(name_data)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "filename is not UTF-8"))?;
    safe_basename(&name)?;
    let size = read_u64(r)?;
    let chunk_bytes = read_u32(r)?;
    let count = read_u32(r)?;
    if size > MAX_FILE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "file exceeds transfer limit",
        ));
    }
    if count > MAX_CHUNKS || chunk_bytes as usize != CHUNK_BYTES || count != chunk_count(size)? {
        return Err(invalid("invalid chunk size or count"));
    }
    let mut hashes = Vec::<Digest>::with_capacity(count as usize);
    for _ in 0..count {
        let mut hash = [0u8; 32];
        r.read_exact(&mut hash)?;
        hashes.push(hash);
    }
    Ok(Manifest {
        name,
        size,
        count,
        hashes,
    })
}

/// Decode one CHUNK payload (`index`, `codec`, `encoded_length`, bytes) and
/// verify it against the manifest digest. Returns the decoded chunk.
pub fn read_chunk(
    r: &mut impl Read,
    expected_index: u32,
    manifest: &Manifest,
) -> io::Result<Vec<u8>> {
    let index = read_u32(r)?;
    if index != expected_index || index >= manifest.count {
        return Err(invalid("chunk order mismatch"));
    }
    let expected = expected_chunk_len(manifest.size, index)?;
    let codec = read_u8(r)?;
    if codec != RAW && codec != RLE {
        // Reject before reading a declared length, so an unknown codec cannot
        // force a large allocation.
        return Err(invalid("unknown codec"));
    }
    let compressed_size = read_u32(r)? as usize;
    let size_ok = match codec {
        RAW => compressed_size == expected,
        RLE => {
            compressed_size <= expected.saturating_mul(2).min(MAX_ENCODED_BYTES)
                && compressed_size.is_multiple_of(2)
        }
        _ => false,
    };
    if !size_ok || compressed_size > MAX_ENCODED_BYTES {
        return Err(invalid("chunk payload too large"));
    }
    let mut encoded = vec![0u8; compressed_size];
    r.read_exact(&mut encoded)?;
    let chunk = decode_chunk(codec, &encoded, expected)?;
    if digest(&chunk) != manifest.hashes[index as usize] {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "chunk hash mismatch",
        ));
    }
    Ok(chunk)
}

// ---------------------------------------------------------------------------
// Content-addressed filesystem cache
// ---------------------------------------------------------------------------

/// Path of a cached chunk: `root/chunks/<first 2 hex>/<64 hex>`.
fn digest_path(root: &Path, hash: &Digest) -> PathBuf {
    let s = hex(hash);
    root.join("chunks").join(&s[..2]).join(s)
}

/// Refuse to read or write through a symlink. Regular files, directories and
/// missing paths are fine. This stops a link from redirecting I/O to a file
/// outside the data root.
fn ensure_not_symlink(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(md) if md.file_type().is_symlink() => {
            Err(invalid("refusing to follow a symlink in the data root"))
        }
        _ => Ok(()),
    }
}

/// Read a regular file that is at most `limit` bytes.
///
/// The size is taken from metadata before the read, so a planted multi-megabyte
/// cache object cannot be loaded to test its digest.
fn read_bounded_file(path: &Path, limit: usize) -> io::Result<Vec<u8>> {
    ensure_not_symlink(path)?;
    let meta = fs::symlink_metadata(path)?;
    if !meta.file_type().is_file() {
        return Err(invalid("cache entry is not a regular file"));
    }
    let len = meta.len();
    if len > limit as u64 {
        return Err(invalid("cached object exceeds size cap"));
    }
    let mut file = File::open(path)?;
    let mut buf = vec![0u8; len as usize];
    file.read_exact(&mut buf)?;
    Ok(buf)
}

/// True if the path holds a valid copy of the expected chunk.
fn cached_and_valid(path: &Path, expected: &Digest) -> bool {
    match read_bounded_file(path, CHUNK_BYTES) {
        Ok(bytes) => digest(&bytes) == *expected,
        Err(_) => false,
    }
}

/// Flush directory metadata after an atomic rename. Windows directory handles
/// are not opened here; file `sync_all` before the rename is the durability
/// step on that target.
fn sync_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        File::open(dir)?.sync_all()?;
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
    }
    Ok(())
}

static STORE_SEQ: AtomicU64 = AtomicU64::new(0);

/// Store a verified chunk in the cache. Idempotent: an already-valid chunk is
/// left untouched. The write goes through a private, exclusive temp file so a
/// stale temp or a pre-placed link can never clobber existing content.
fn store_chunk(root: &Path, hash: &Digest, bytes: &[u8]) -> io::Result<()> {
    if bytes.len() > CHUNK_BYTES || digest(bytes) != *hash {
        return Err(invalid(
            "refusing to store an oversized or misaddressed chunk",
        ));
    }
    let path = digest_path(root, hash);
    if cached_and_valid(&path, hash) {
        return Ok(());
    }
    let dir = path.parent().ok_or_else(|| invalid("invalid cache path"))?;
    fs::create_dir_all(dir)?;
    let seq = STORE_SEQ.fetch_add(1, Ordering::Relaxed) + 1;
    let short = &hex(hash)[..12];
    let tmp = dir.join(format!(".partial-{seq}-{short}"));
    let write_result = (|| -> io::Result<()> {
        let mut file = exclusive_create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, &path)?;
        sync_dir(dir)?;
        Ok(())
    })();
    if write_result.is_err() {
        // The temp name is never a committed chunk. If rename already succeeded,
        // the temp path is gone and this remove is a no-op.
        let _ = fs::remove_file(&tmp);
    }
    write_result
}

/// Open a private file that must not exist yet. If something is already at the
/// path (regular file, directory or symlink) the open fails cleanly and
/// nothing existing is truncated or replaced.
fn exclusive_create(path: &Path) -> io::Result<File> {
    match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(file) => Ok(file),
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
            ensure_not_symlink(path)?;
            Err(invalid(
                "private transfer file already exists; refusing to overwrite",
            ))
        }
        Err(err) => Err(err),
    }
}

// ---------------------------------------------------------------------------
// Atomic, collision-safe output materialization
// ---------------------------------------------------------------------------

/// The private in-progress file used to reconstruct an output atomically.
/// It is always a *distinct* name from the final output (no extension
/// collision), and it lives in the same directory so the commit rename stays
/// on one filesystem. A failed transfer therefore can never truncate or
/// delete a previously committed output.
pub fn in_progress_path(output: &Path) -> PathBuf {
    let name = output
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("output");
    let dir = output.parent().unwrap_or_else(|| Path::new("."));
    dir.join(format!(".fastlink-inprogress-{}", name))
}

/// Output path must be a regular file or absent — never a symlink or a
/// directory. This prevents a link from redirecting the final rename.
fn output_is_safe(output: &Path) -> io::Result<()> {
    match fs::symlink_metadata(output) {
        Ok(md) if md.file_type().is_symlink() => {
            Err(invalid("refusing to write through a symlink"))
        }
        Ok(md) if md.is_dir() => Err(invalid("output path is a directory")),
        _ => Ok(()),
    }
}

/// Reconstruct the full file from verified cache chunks into `output`,
/// replacing any pre-existing file atomically only after every chunk has been
/// re-verified and every byte has been written and flushed.
///
/// On any error the private in-progress file is removed and the previous
/// committed file (if any) is left untouched.
pub fn materialize_file(root: &Path, manifest: &Manifest, output: &Path) -> io::Result<()> {
    let parent = output
        .parent()
        .ok_or_else(|| invalid("invalid output path"))?;
    fs::create_dir_all(parent)?;
    output_is_safe(output)?;

    let in_progress = in_progress_path(output);
    // Clean up a stale private leftover from a previously crashed session, but
    // never remove or follow a symlink or directory.
    match fs::symlink_metadata(&in_progress) {
        Ok(md) if md.is_file() => fs::remove_file(&in_progress)?,
        Ok(md) if md.is_symlink() => {
            return Err(invalid("refusing to follow a symlink in the data root"))
        }
        Ok(md) if md.is_dir() => return Err(invalid("in-progress path is a directory")),
        Ok(_) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(err),
    }

    let result = (|| -> io::Result<()> {
        let mut destination = exclusive_create(&in_progress)?;
        for (idx, hash) in manifest.hashes.iter().enumerate() {
            let chunk_path = digest_path(root, hash);
            let bytes = read_bounded_file(&chunk_path, CHUNK_BYTES)?;
            if bytes.len() != expected_chunk_len(manifest.size, idx as u32)?
                || digest(&bytes) != *hash
            {
                return Err(invalid("cache integrity failure"));
            }
            destination.write_all(&bytes)?;
        }
        destination.sync_all()?;
        drop(destination);
        fs::rename(&in_progress, output)?;
        if let Some(parent) = output.parent() {
            sync_dir(parent)?;
        }
        Ok(())
    })();
    if result.is_err() {
        // Only ever remove our private in-progress file, never the committed output.
        let _ = fs::remove_file(&in_progress);
    }
    result
}

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

fn os_starts_with(name: &std::ffi::OsStr, prefix: &str) -> bool {
    name.to_string_lossy().starts_with(prefix)
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

// ---------------------------------------------------------------------------
// Session engine (client + server), generic over any Read + Write stream
// ---------------------------------------------------------------------------

/// Measured transfer outcome for a single SYNC session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferStats {
    pub filename: String,
    pub file_bytes: u64,
    pub chunks_total: u32,
    pub chunks_reused: u32,
    pub chunks_transferred: u32,
    /// Raw (unencoded) bytes of the chunks that had to be sent.
    pub raw_missing_bytes: u64,
    /// Encoded payload bytes actually placed on the wire (client-side metric).
    pub encoded_payload_bytes: u64,
}

/// Versioned handshake. Both sides send `FLNK` + version and accept only an
/// identical reply; the exchange is wire-compatible in either order.
pub fn handshake<R: Read + Write>(stream: &mut R) -> io::Result<()> {
    stream.write_all(MAGIC)?;
    write_u16(stream, VERSION)?;
    stream.flush()?;
    let mut magic = [0u8; 4];
    stream.read_exact(&mut magic)?;
    let version = read_u16(stream)?;
    if &magic != MAGIC || version != VERSION {
        return Err(invalid("incompatible FASTLINK protocol"));
    }
    Ok(())
}

/// Client side of one SYNC session over `stream`.
///
/// Builds the manifest from `source`, sends it, reads the server's list of
/// missing chunks, streams exactly those chunks (re-verified against the
/// manifest), and reads the terminal status. On any error the session is
/// aborted and no partial state is committed by the client.
pub fn client_sync<R: Read + Write>(
    stream: &mut R,
    filename: &str,
    source: &Path,
) -> io::Result<TransferStats> {
    safe_basename(filename)?;
    let size = fs::metadata(source)?.len();
    let count = chunk_count(size)?;
    let hashes = Manifest::hashes_from_file(source, size, count)?;
    let manifest = Manifest {
        name: filename.to_string(),
        size,
        count,
        hashes,
    };

    handshake(stream)?;

    write_u8(stream, CMD_SYNC)?;
    let name = filename.as_bytes();
    write_u16(stream, name.len() as u16)?;
    stream.write_all(name)?;
    write_u64(stream, size)?;
    write_u32(stream, CHUNK_BYTES as u32)?;
    write_u32(stream, count)?;
    for hash in &manifest.hashes {
        stream.write_all(hash)?;
    }

    let missing_count = read_u32(stream)?;
    if missing_count > count || missing_count > MAX_CHUNKS {
        return Err(invalid("server requested more chunks than declared"));
    }
    let mut missing = Vec::with_capacity(missing_count as usize);
    let mut last = None;
    for _ in 0..missing_count {
        let idx = read_u32(stream)?;
        if idx >= count || last.is_some_and(|prev| idx <= prev) {
            return Err(invalid("invalid or unordered missing index"));
        }
        missing.push(idx);
        last = Some(idx);
    }

    let mut wire_payload_bytes = 0u64;
    let mut raw_bytes = 0u64;
    let mut source_file = File::open(source)?;
    for idx in &missing {
        let len = expected_chunk_len(size, *idx)?;
        source_file.seek(SeekFrom::Start(*idx as u64 * CHUNK_BYTES as u64))?;
        let mut chunk = vec![0u8; len];
        source_file.read_exact(&mut chunk)?;
        if digest(&chunk) != manifest.hashes[*idx as usize] {
            return Err(invalid("input file changed while syncing"));
        }
        let (codec, encoded) = encode_chunk(&chunk);
        write_u32(stream, *idx)?;
        write_u8(stream, codec)?;
        write_u32(stream, encoded.len() as u32)?;
        stream.write_all(&encoded)?;
        wire_payload_bytes += encoded.len() as u64;
        raw_bytes += len as u64;
    }

    let status = read_u8(stream)?;
    if status != STATUS_OK {
        return Err(invalid("server failed to materialize output"));
    }

    Ok(TransferStats {
        filename: manifest.name,
        file_bytes: size,
        chunks_total: count,
        chunks_reused: count - missing_count,
        chunks_transferred: missing_count,
        raw_missing_bytes: raw_bytes,
        encoded_payload_bytes: wire_payload_bytes,
    })
}

/// Server side of one SYNC session over `stream`.
///
/// Reads the manifest, reports which chunks it does not already hold, pulls
/// exactly those chunks from the peer (verifying each against the manifest),
/// stores them in the content-addressed cache, and atomically materializes
/// the output. On any error the session aborts without committing an output.
pub fn server_sync<R: Read + Write>(stream: &mut R, root: &Path) -> io::Result<TransferStats> {
    // Drop uncommitted temps from a crashed process before this session
    // reads or writes the cache. Committed outputs are not touched.
    recover_atomic_writes(root)?;
    handshake(stream)?;

    let command = read_u8(stream)?;
    if command != CMD_SYNC {
        return Err(invalid("unknown command"));
    }
    let manifest = read_manifest(stream)?;

    let mut missing = Vec::new();
    let mut requested = HashSet::new();
    for (idx, hash) in manifest.hashes.iter().enumerate() {
        if !cached_and_valid(&digest_path(root, hash), hash) && requested.insert(*hash) {
            missing.push(idx as u32);
        }
    }
    write_u32(stream, missing.len() as u32)?;
    for idx in &missing {
        write_u32(stream, *idx)?;
    }

    let mut raw_received = 0u64;
    for idx in &missing {
        let chunk = read_chunk(stream, *idx, &manifest)?;
        raw_received += chunk.len() as u64;
        store_chunk(root, &manifest.hashes[*idx as usize], &chunk)?;
    }

    let output_dir = root.join("outputs");
    fs::create_dir_all(&output_dir)?;
    let output = output_dir.join(&manifest.name);
    materialize_file(root, &manifest, &output)?;

    write_u8(stream, STATUS_OK)?;

    Ok(TransferStats {
        filename: manifest.name,
        file_bytes: manifest.size,
        chunks_total: manifest.count,
        chunks_reused: manifest.count - missing.len() as u32,
        chunks_transferred: missing.len() as u32,
        raw_missing_bytes: raw_received,
        encoded_payload_bytes: 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_root() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "fastlink-unit-test-{}-{}",
            std::process::id(),
            TEST_COUNTER.fetch_add(1, Ordering::Relaxed) + 1
        ));
        fs::create_dir_all(dir.join("chunks")).unwrap();
        fs::create_dir_all(dir.join("outputs")).unwrap();
        dir
    }

    fn store(root: &Path, bytes: &[u8]) {
        store_chunk(root, &digest(bytes), bytes).unwrap();
    }

    fn manifest_for(chunk: &[u8]) -> Manifest {
        Manifest {
            name: "out.bin".to_string(),
            size: chunk.len() as u64,
            count: 1,
            hashes: vec![digest(chunk)],
        }
    }

    #[test]
    fn rle_sparse_roundtrip() {
        let bytes = vec![0u8; CHUNK_BYTES];
        let (codec, compressed) = encode_chunk(&bytes);
        assert_eq!(codec, RLE);
        assert!(compressed.len() < 1024);
        assert_eq!(
            decode_chunk(codec, &compressed, bytes.len()).unwrap(),
            bytes
        );
    }

    #[test]
    fn rle_roundtrip_mixed() {
        let bytes: Vec<u8> = [
            vec![7u8; 300],
            vec![0u8; 65000],
            vec![255u8; 100],
            vec![9u8; 3],
        ]
        .concat();
        let (codec, compressed) = encode_chunk(&bytes);
        assert_eq!(codec, RLE);
        assert_eq!(
            decode_chunk(codec, &compressed, bytes.len()).unwrap(),
            bytes
        );
    }

    #[test]
    fn rle_run_zero_rejected() {
        // run == 0 is malformed and must be rejected.
        assert!(decode_chunk(RLE, &[0, 42], 2).is_err());
        assert!(decode_chunk(RLE, &[0, 42, 2, 42], 2).is_err());
    }

    #[test]
    fn rle_oversized_run_rejected() {
        let bad = vec![255, 42, 255, 42];
        assert!(decode_chunk(RLE, &bad, 100).is_err());
    }

    #[test]
    fn rle_underrun_rejected() {
        // Decoded size < expected must be rejected.
        assert!(decode_chunk(RLE, &[3, 1], 5).is_err());
        // Decoded size > expected must be rejected.
        assert!(decode_chunk(RLE, &[4, 1], 3).is_err());
        // Odd-length RLE stream must be rejected.
        assert!(decode_chunk(RLE, &[3, 1, 2], 5).is_err());
    }

    #[test]
    fn raw_underrun_rejected() {
        assert!(decode_chunk(RAW, &[1, 2, 3], 5).is_err());
    }

    #[test]
    fn incompressible_roundtrip() {
        let bytes: Vec<u8> = (0..CHUNK_BYTES)
            .map(|i| (i.wrapping_mul(13) % 251) as u8)
            .collect();
        let (codec, compressed) = encode_chunk(&bytes);
        assert_eq!(codec, RAW);
        assert_eq!(
            decode_chunk(codec, &compressed, bytes.len()).unwrap(),
            bytes
        );
    }

    #[test]
    fn unknown_codec_rejected() {
        assert!(decode_chunk(2, &[], 0).is_err());
        assert!(decode_chunk(255, b"abc", 3).is_err());
    }

    #[test]
    fn chunk_limit_enforced() {
        assert!(decode_chunk(RAW, &[0; CHUNK_BYTES + 1], CHUNK_BYTES + 1).is_err());
        // 65536 RLE pairs, each a run of 255 -> decodes to ~16 MiB, over the
        // 64 KiB expected size, and must be rejected.
        let pairs: Vec<u8> = (0..65536).flat_map(|_| [255u8, 0u8]).collect();
        assert!(decode_chunk(RLE, &pairs, CHUNK_BYTES).is_err());
    }

    #[test]
    fn checksums_change_when_content_changes() {
        assert_ne!(digest(b"abc"), digest(b"abd"));
        assert_eq!(digest(b"abc"), digest(b"abc"));
    }

    #[test]
    fn chunk_count_boundaries() {
        assert_eq!(chunk_count(0).unwrap(), 0);
        assert_eq!(chunk_count(1).unwrap(), 1);
        assert_eq!(chunk_count(CHUNK_BYTES as u64).unwrap(), 1);
        assert_eq!(chunk_count(CHUNK_BYTES as u64 + 1).unwrap(), 2);
        assert_eq!(chunk_count(MAX_FILE_BYTES).unwrap(), MAX_CHUNKS);
        assert!(chunk_count(MAX_FILE_BYTES + 1).is_err());
    }

    #[test]
    fn expected_chunk_len_boundaries() {
        assert_eq!(expected_chunk_len(1, 0).unwrap(), 1);
        assert!(expected_chunk_len(1, 1).is_err());
        assert_eq!(expected_chunk_len(CHUNK_BYTES as u64 + 1, 1).unwrap(), 1);
        assert_eq!(
            expected_chunk_len(CHUNK_BYTES as u64, 0).unwrap(),
            CHUNK_BYTES
        );
        assert!(expected_chunk_len(0, 0).is_err());
    }

    #[test]
    fn safe_basename_rejects_traversal_and_control() {
        assert!(safe_basename("ok.txt").is_ok());
        assert!(safe_basename("a/b.txt").is_err());
        assert!(safe_basename("a\\b.txt").is_err());
        assert!(safe_basename("..").is_err());
        assert!(safe_basename(".").is_err());
        assert!(safe_basename("").is_err());
        assert!(safe_basename("bad\0name").is_err());
        assert!(safe_basename("bad\nname").is_err());
        assert!(safe_basename("bad\tname").is_err());
        assert!(safe_basename(&"a".repeat(241)).is_err());
        assert!(safe_basename(&"a".repeat(240)).is_ok());
    }

    #[test]
    fn validate_listen_address_only_loopback() {
        assert!(validate_listen_address("127.0.0.1:18777").is_ok());
        assert!(validate_listen_address("127.0.0.1:0").is_ok());
        assert!(validate_listen_address("0.0.0.0:18777").is_err());
        assert!(validate_listen_address("192.168.1.5:18777").is_err());
        assert!(validate_listen_address("not-an-address").is_err());
    }

    #[test]
    fn validate_connect_address_only_loopback() {
        assert!(validate_connect_address("127.0.0.1:18777").is_ok());
        assert!(validate_connect_address("0.0.0.0:18777").is_err());
        assert!(validate_connect_address("example.com:443").is_err());
        assert!(validate_connect_address("not-an-address").is_err());
    }

    #[test]
    fn in_progress_never_equals_output() {
        for name in [
            "data.bin",
            "x.fastlink-partial",
            "noext",
            "archive.tar.gz",
            "weird..fastlink-partial",
        ] {
            let out = Path::new("outputs").join(name);
            let in_progress = in_progress_path(&out);
            assert_ne!(
                in_progress, out,
                "in-progress path collides with output for {name}"
            );
            assert_eq!(in_progress.parent(), out.parent());
        }
    }

    #[test]
    fn materialize_writes_verified_output() {
        let root = temp_root();
        let chunk = vec![1u8; 65536];
        store(&root, &chunk);
        let out = root.join("outputs").join("out.bin");
        materialize_file(&root, &manifest_for(&chunk), &out).unwrap();
        assert_eq!(fs::read(&out).unwrap(), chunk);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn materialize_missing_chunk_rejected() {
        let root = temp_root();
        // Store nothing, so the single chunk is missing from the cache.
        let out = root.join("outputs").join("out.bin");
        assert!(materialize_file(&root, &manifest_for(&vec![1u8; 65536]), &out).is_err());
        assert!(!out.exists(), "no partial output should be committed");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn materialize_replaces_existing_atomically() {
        let root = temp_root();
        let chunk = vec![1u8; 65536];
        store(&root, &chunk);
        let out = root.join("outputs").join("out.bin");
        fs::write(&out, b"pre-existing").unwrap();
        materialize_file(&root, &manifest_for(&chunk), &out).unwrap();
        assert_eq!(fs::read(&out).unwrap(), chunk);
        fs::remove_dir_all(&root).ok();
    }

    // Regression: an output name whose suffix makes the old
    // `with_extension("fastlink-partial")` collide with the final path used to
    // have a failed transfer truncate+delete the previously committed output.
    #[test]
    fn materialize_collision_name_failure_preserves_prior_output() {
        let root = temp_root();
        let name = "x.fastlink-partial";
        let out = root.join("outputs").join(name);
        let good = vec![7u8; 65536];
        store(&root, &good);

        // Commit a good copy first.
        let good_manifest = Manifest {
            name: name.to_string(),
            size: good.len() as u64,
            count: 1,
            hashes: vec![digest(&good)],
        };
        materialize_file(&root, &good_manifest, &out).unwrap();
        assert_eq!(fs::read(&out).unwrap(), good);

        // Now attempt a transfer whose chunk is not in the cache -> must fail
        // and must NOT truncate or delete the committed output.
        let bad = vec![9u8; 65536];
        let bad_manifest = Manifest {
            name: name.to_string(),
            size: bad.len() as u64,
            count: 1,
            hashes: vec![digest(&bad)],
        };
        assert!(materialize_file(&root, &bad_manifest, &out).is_err());
        assert_eq!(
            fs::read(&out).unwrap(),
            good,
            "previously committed output must survive a failed transfer"
        );
        // And no private in-progress file is left behind.
        assert!(!in_progress_path(&out).exists());
        fs::remove_dir_all(&root).ok();
    }

    #[cfg(unix)]
    #[test]
    fn materialize_symlink_output_rejected() {
        let root = temp_root();
        let outside = root.join("outside-target");
        fs::write(&outside, b"sentinel-outside-root").unwrap();
        let out = root.join("outputs").join("evil.bin");
        fs::create_dir_all(out.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&outside, &out).unwrap();
        let chunk = vec![1u8; 65536];
        store(&root, &chunk);
        assert!(materialize_file(&root, &manifest_for(&chunk), &out).is_err());
        // The target outside the root must be untouched.
        assert_eq!(fs::read(&outside).unwrap(), b"sentinel-outside-root");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn read_manifest_rejects_empty_name() {
        let mut r = Cursor::new(Vec::new());
        write_u16(&mut r, 0).unwrap();
        assert!(read_manifest(&mut r).is_err());
    }

    #[test]
    fn read_manifest_rejects_bad_chunk_size() {
        let mut buf = Vec::new();
        write_u16(&mut buf, 6).unwrap();
        buf.extend_from_slice(b"ok.bin");
        write_u64(&mut buf, 65536).unwrap();
        write_u32(&mut buf, 4096).unwrap(); // wrong chunk size
        write_u32(&mut buf, 1).unwrap();
        let mut r = Cursor::new(buf);
        assert!(read_manifest(&mut r).is_err());
    }

    #[test]
    fn read_manifest_rejects_bad_count() {
        let mut buf = Vec::new();
        write_u16(&mut buf, 6).unwrap();
        buf.extend_from_slice(b"ok.bin");
        write_u64(&mut buf, 65536).unwrap();
        write_u32(&mut buf, CHUNK_BYTES as u32).unwrap();
        write_u32(&mut buf, 2).unwrap(); // wrong count
        let mut r = Cursor::new(buf);
        assert!(read_manifest(&mut r).is_err());
    }

    #[test]
    fn read_manifest_rejects_overlong_name() {
        let mut buf = Vec::new();
        let name = "a".repeat(241);
        write_u16(&mut buf, name.len() as u16).unwrap();
        buf.extend_from_slice(name.as_bytes());
        write_u64(&mut buf, 65536).unwrap();
        write_u32(&mut buf, CHUNK_BYTES as u32).unwrap();
        write_u32(&mut buf, 1).unwrap();
        let mut r = Cursor::new(buf);
        assert!(read_manifest(&mut r).is_err());
    }

    #[test]
    fn read_manifest_rejects_traversal_name() {
        let mut buf = Vec::new();
        let name = b"../evil";
        write_u16(&mut buf, name.len() as u16).unwrap();
        buf.extend_from_slice(name);
        write_u64(&mut buf, 65536).unwrap();
        write_u32(&mut buf, CHUNK_BYTES as u32).unwrap();
        write_u32(&mut buf, 1).unwrap();
        let mut r = Cursor::new(buf);
        assert!(read_manifest(&mut r).is_err());
    }

    #[test]
    fn read_chunk_rejects_wrong_index() {
        let chunk = vec![7u8; 100];
        let manifest = manifest_for(&chunk);
        let mut buf = Vec::new();
        write_u32(&mut buf, 1).unwrap(); // expected index 0
        write_u8(&mut buf, RAW).unwrap();
        write_u32(&mut buf, 100).unwrap();
        buf.extend_from_slice(&chunk);
        let mut r = Cursor::new(buf);
        assert!(read_chunk(&mut r, 0, &manifest).is_err());
    }

    #[test]
    fn read_chunk_accepts_valid_raw() {
        let chunk = vec![7u8; 100];
        let manifest = manifest_for(&chunk);
        let mut buf = Vec::new();
        write_u32(&mut buf, 0).unwrap();
        write_u8(&mut buf, RAW).unwrap();
        write_u32(&mut buf, 100).unwrap();
        buf.extend_from_slice(&chunk);
        let mut r = Cursor::new(buf);
        assert_eq!(read_chunk(&mut r, 0, &manifest).unwrap(), chunk);
    }

    #[test]
    fn read_chunk_rejects_bad_digest() {
        let chunk = vec![7u8; 100];
        let manifest = Manifest {
            name: "f".to_string(),
            size: 100,
            count: 1,
            hashes: vec![digest(&[8u8; 100])], // does not match `chunk`
        };
        let mut buf = Vec::new();
        write_u32(&mut buf, 0).unwrap();
        write_u8(&mut buf, RAW).unwrap();
        write_u32(&mut buf, 100).unwrap();
        buf.extend_from_slice(&chunk);
        let mut r = Cursor::new(buf);
        assert!(read_chunk(&mut r, 0, &manifest).is_err());
    }

    #[test]
    fn read_chunk_rejects_oversized_payload() {
        let chunk = vec![7u8; 100];
        let manifest = manifest_for(&chunk);
        let mut buf = Vec::new();
        write_u32(&mut buf, 0).unwrap();
        write_u8(&mut buf, RAW).unwrap();
        write_u32(&mut buf, (MAX_ENCODED_BYTES + 1) as u32).unwrap();
        // The size check fires before the read, so no payload bytes are needed.
        let mut r = Cursor::new(buf);
        assert!(read_chunk(&mut r, 0, &manifest).is_err());
    }

    /// A minimal full-duplex stream: reads come from a pre-seeded peer reply,
    /// writes are captured separately. This keeps the two directions distinct
    /// (a `Cursor<Vec<u8>>` would let the local write overwrite the peer data).
    struct TwoWay {
        peer: Cursor<Vec<u8>>,
        local: Vec<u8>,
    }
    impl Read for TwoWay {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.peer.read(buf)
        }
    }
    impl Write for TwoWay {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.local.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn two_way(peer_reply: Vec<u8>) -> TwoWay {
        TwoWay {
            peer: Cursor::new(peer_reply),
            local: Vec::new(),
        }
    }

    #[test]
    fn handshake_rejects_bad_magic() {
        let mut buf = Vec::new();
        buf.extend_from_slice(b"XXXX"); // peer magic
        write_u16(&mut buf, VERSION).unwrap();
        let mut stream = two_way(buf);
        assert!(handshake(&mut stream).is_err());
        // The local side must have emitted its own magic + version.
        assert_eq!(&stream.local[..4], MAGIC);
    }

    #[test]
    fn handshake_rejects_bad_version() {
        let mut buf = Vec::new();
        buf.extend_from_slice(b"FLNK");
        write_u16(&mut buf, 99).unwrap(); // wrong version
        let mut stream = two_way(buf);
        assert!(handshake(&mut stream).is_err());
    }

    #[test]
    fn handshake_accepts_matching_reply() {
        let mut buf = Vec::new();
        buf.extend_from_slice(b"FLNK");
        write_u16(&mut buf, VERSION).unwrap();
        let mut stream = two_way(buf);
        assert!(handshake(&mut stream).is_ok());
        // Local side must emit exactly FLNK + version (little-endian).
        let mut expected = Vec::new();
        expected.extend_from_slice(MAGIC);
        expected.extend_from_slice(&VERSION.to_le_bytes());
        assert_eq!(stream.local, expected);
    }

    #[test]
    fn read_manifest_rejects_name_length_before_reading_bytes() {
        let mut buf = Vec::new();
        write_u16(&mut buf, u16::MAX).unwrap();
        let err = read_manifest(&mut Cursor::new(buf)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn read_chunk_rejects_unknown_codec_before_length() {
        let manifest = manifest_for(&[1u8; 4]);
        let mut buf = Vec::new();
        write_u32(&mut buf, 0).unwrap();
        write_u8(&mut buf, 9).unwrap();
        let err = read_chunk(&mut Cursor::new(buf), 0, &manifest).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("unknown codec"));
    }

    #[test]
    fn read_chunk_rejects_raw_length_before_payload() {
        let manifest = manifest_for(&[1u8; 4]);
        let mut buf = Vec::new();
        write_u32(&mut buf, 0).unwrap();
        write_u8(&mut buf, RAW).unwrap();
        write_u32(&mut buf, 50).unwrap();
        let err = read_chunk(&mut Cursor::new(buf), 0, &manifest).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(!err.to_string().to_lowercase().contains("eof"));
    }

    #[test]
    fn rle_encoded_longer_than_twice_expected_is_rejected() {
        assert!(decode_chunk(RLE, &[1, 9, 1, 8], 1).is_err());
    }

    #[test]
    fn client_rejects_unbounded_missing_list() {
        let root = temp_root();
        let path = root.join("a.bin");
        fs::write(&path, b"hi").unwrap();
        let mut peer = Vec::new();
        peer.extend_from_slice(MAGIC);
        peer.extend_from_slice(&VERSION.to_le_bytes());
        peer.extend_from_slice(&u32::MAX.to_le_bytes());
        let mut stream = two_way(peer);
        let err = client_sync(&mut stream, "a.bin", &path).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("more chunks"));
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn recover_discards_partials_and_keeps_committed_output() {
        let root = temp_root();
        let chunk = vec![3u8; 32];
        store(&root, &chunk);
        let hash = digest(&chunk);
        let shard = digest_path(&root, &hash).parent().unwrap().to_path_buf();
        let partial = shard.join(".partial-9-aabbccddeeff");
        fs::write(&partial, &chunk).unwrap();
        let orphan_dir = root.join("chunks").join("ff");
        fs::create_dir_all(&orphan_dir).unwrap();
        let orphan = orphan_dir.join(".partial-1-001122334455");
        fs::write(&orphan, &chunk).unwrap();
        let out = root.join("outputs").join("out.bin");
        fs::write(&out, b"committed").unwrap();
        let stale = in_progress_path(&out);
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
        let out = root.join("outputs").join("out.bin");
        let stale = in_progress_path(&out);
        fs::write(&stale, vec![9u8; 32]).unwrap();
        recover_atomic_writes(&root).unwrap();
        assert!(!stale.exists());
        assert!(!out.exists());
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn oversized_cache_object_is_not_a_hit() {
        let root = temp_root();
        let chunk = vec![1u8; 32];
        let hash = digest(&chunk);
        let path = digest_path(&root, &hash);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, vec![1u8; CHUNK_BYTES + 8]).unwrap();
        assert!(!cached_and_valid(&path, &hash));
        let out = root.join("outputs").join("out.bin");
        let manifest = Manifest {
            name: "out.bin".into(),
            size: 32,
            count: 1,
            hashes: vec![hash],
        };
        assert!(materialize_file(&root, &manifest, &out).is_err());
        assert!(!out.exists());
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn store_chunk_rejects_misaddressed_bytes() {
        let root = temp_root();
        let bytes = vec![1u8; 8];
        let err = store_chunk(&root, &digest(b"other"), &bytes).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn fuzz_corpus_decode_paths_do_not_panic() {
        let mut samples = Vec::<Vec<u8>>::new();
        samples.push(Vec::new());
        for len in 0..64 {
            samples.push(vec![0xff; len]);
            samples.push(vec![0x00; len]);
        }
        let mut state = 0x1234_5678u32;
        for _ in 0..64 {
            let mut buf = Vec::new();
            let len = (state % 80) as usize;
            for _ in 0..len {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                buf.push((state >> 16) as u8);
            }
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            samples.push(buf);
        }
        let mut huge_name = Vec::new();
        write_u16(&mut huge_name, u16::MAX).unwrap();
        samples.push(huge_name);
        let mut huge_chunk = Vec::new();
        write_u32(&mut huge_chunk, 0).unwrap();
        write_u8(&mut huge_chunk, RAW).unwrap();
        write_u32(&mut huge_chunk, u32::MAX).unwrap();
        samples.push(huge_chunk);

        let manifest = Manifest {
            name: "a.bin".into(),
            size: 4,
            count: 1,
            hashes: vec![[7u8; 32]],
        };
        for sample in samples {
            let _ = read_manifest(&mut Cursor::new(sample.clone()));
            let _ = decode_chunk(RAW, &sample, 0);
            let _ = decode_chunk(RAW, &sample, 4);
            let _ = decode_chunk(RLE, &sample, 4);
            let _ = decode_chunk(255, &sample, CHUNK_BYTES);
            let _ = read_chunk(&mut Cursor::new(sample.clone()), 0, &manifest);
            let _ = handshake(&mut two_way(sample));
        }
    }
}
