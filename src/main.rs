//! FASTLINK CPU-only proof of concept: content-addressed, lossless delta sync.
//! This does NOT yet accelerate llama.cpp RPC or GPU execution.
use fastlink::{
    chunk_count, decode_chunk, digest, encode_chunk, expected_chunk_len, hex, mib_per_sec,
    read_u16, read_u32, read_u64, read_u8, write_u16, write_u32, write_u64, write_u8,
    CountingStream, Digest, TransferMetrics, WireBreakdown, CHUNK_BYTES, MAGIC, MAX_CHUNKS,
    MAX_ENCODED_BYTES, VERSION,
};
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const CMD_SYNC: u8 = 1;

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn digest_path(root: &Path, hash: &Digest) -> PathBuf {
    let s = hex(hash);
    root.join("chunks").join(&s[..2]).join(s)
}

fn cached_and_valid(path: &Path, expected: &Digest) -> bool {
    match fs::read(path) {
        Ok(bytes) if bytes.len() <= CHUNK_BYTES => digest(&bytes) == *expected,
        _ => false,
    }
}

fn store_chunk(root: &Path, hash: &Digest, bytes: &[u8]) -> io::Result<()> {
    let path = digest_path(root, hash);
    fs::create_dir_all(path.parent().ok_or_else(|| invalid("invalid cache path"))?)?;
    if cached_and_valid(&path, hash) {
        return Ok(());
    }
    let tmp = path.with_extension("partial");
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, &path)?;
    Ok(())
}

fn safe_basename(name: &str) -> io::Result<&str> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.len() > 240
        || name.contains('/')
        || name.contains('\\')
        || name.contains('\0')
    {
        return Err(invalid("unsafe or overlong file name"));
    }
    Ok(name)
}

fn client_handshake(stream: &mut (impl Read + Write)) -> io::Result<()> {
    stream.write_all(MAGIC)?;
    write_u16(stream, VERSION)?;
    stream.flush()?;
    let mut magic = [0; 4];
    stream.read_exact(&mut magic)?;
    let version = read_u16(stream)?;
    if &magic != MAGIC || version != VERSION {
        return Err(invalid("incompatible FASTLINK protocol"));
    }
    Ok(())
}

fn server_handshake(stream: &mut (impl Read + Write)) -> io::Result<()> {
    let mut magic = [0; 4];
    stream.read_exact(&mut magic)?;
    let version = read_u16(stream)?;
    if &magic != MAGIC || version != VERSION {
        return Err(invalid("incompatible FASTLINK protocol"));
    }
    stream.write_all(MAGIC)?;
    write_u16(stream, VERSION)?;
    Ok(())
}

fn send_file_with_metrics(file_path: &Path, server: &str) -> io::Result<(String, TransferMetrics)> {
    let started = Instant::now();
    let filename = file_path
        .file_name()
        .and_then(|x| x.to_str())
        .ok_or_else(|| invalid("invalid input file name"))?;
    safe_basename(filename)?;
    let mut file = File::open(file_path)?;
    let size = file.metadata()?.len();
    let count = chunk_count(size)?;
    let name = filename.as_bytes();
    let mut hashes = Vec::<Digest>::with_capacity(count as usize);
    for index in 0..count {
        let mut chunk = vec![0u8; expected_chunk_len(size, index)?];
        file.read_exact(&mut chunk)?;
        hashes.push(digest(&chunk));
    }

    let tcp = TcpStream::connect(server)?;
    tcp.set_read_timeout(Some(Duration::from_secs(300)))?;
    tcp.set_write_timeout(Some(Duration::from_secs(300)))?;
    let mut stream = CountingStream::new(tcp);

    let t_handshake = Instant::now();
    client_handshake(&mut stream)?;
    let handshake_us = t_handshake.elapsed().as_micros() as u64;

    let t_manifest = Instant::now();
    write_u8(&mut stream, CMD_SYNC)?;
    write_u16(&mut stream, name.len() as u16)?;
    stream.write_all(name)?;
    write_u64(&mut stream, size)?;
    write_u32(&mut stream, CHUNK_BYTES as u32)?;
    write_u32(&mut stream, count)?;
    for hash in &hashes {
        stream.write_all(hash)?;
    }
    stream.flush()?;
    let missing_len = read_u32(&mut stream)?;
    if missing_len > count || missing_len > MAX_CHUNKS {
        return Err(invalid("server requested invalid missing count"));
    }
    let mut missing = Vec::with_capacity(missing_len as usize);
    let mut last = None;
    for _ in 0..missing_len {
        let idx = read_u32(&mut stream)?;
        if idx >= count || last.is_some_and(|x| idx <= x) {
            return Err(invalid("invalid or unordered missing index"));
        }
        missing.push(idx);
        last = Some(idx);
    }
    let manifest_us = t_manifest.elapsed().as_micros() as u64;

    let t_transfer = Instant::now();
    let mut wire_payload_bytes = 0u64;
    let mut raw_bytes_selected = 0u64;
    for idx in &missing {
        let len = expected_chunk_len(size, *idx)?;
        let mut chunk = vec![0u8; len];
        file.seek(SeekFrom::Start(*idx as u64 * CHUNK_BYTES as u64))?;
        file.read_exact(&mut chunk)?;
        if digest(&chunk) != hashes[*idx as usize] {
            return Err(invalid("input file changed while syncing"));
        }
        let (codec, encoded) = encode_chunk(&chunk);
        write_u32(&mut stream, *idx)?;
        write_u8(&mut stream, codec)?;
        write_u32(&mut stream, encoded.len() as u32)?;
        stream.write_all(&encoded)?;
        wire_payload_bytes += encoded.len() as u64;
        raw_bytes_selected += chunk.len() as u64;
    }
    stream.flush()?;
    let status = read_u8(&mut stream)?;
    if status != 0 {
        return Err(invalid("server failed to materialize output"));
    }
    let transfer_us = t_transfer.elapsed().as_micros() as u64;
    let elapsed = started.elapsed();

    let breakdown = WireBreakdown::compute(name.len(), count, missing_len, wire_payload_bytes);
    let wire_bytes_tx = stream.bytes_tx();
    let wire_bytes_rx = stream.bytes_rx();
    let wire_bytes_total = stream.bytes_total();
    debug_assert_eq!(wire_bytes_tx, breakdown.expected_wire_bytes_tx);
    debug_assert_eq!(wire_bytes_rx, breakdown.expected_wire_bytes_rx);
    debug_assert_eq!(wire_bytes_total, breakdown.expected_wire_bytes_total);

    let metrics = TransferMetrics {
        file_bytes: size,
        chunks_total: count,
        chunks_reused: count - missing_len,
        chunks_transferred: missing_len,
        raw_missing_bytes: raw_bytes_selected,
        encoded_payload_bytes: wire_payload_bytes,
        handshake_wire_bytes: breakdown.handshake_wire_bytes,
        manifest_wire_bytes: breakdown.manifest_wire_bytes,
        need_wire_bytes: breakdown.need_wire_bytes,
        chunk_framing_wire_bytes: breakdown.chunk_framing_wire_bytes,
        status_wire_bytes: breakdown.status_wire_bytes,
        headers_and_framing_wire_bytes: breakdown.headers_and_framing_wire_bytes,
        wire_bytes_tx,
        wire_bytes_rx,
        wire_bytes_total,
        handshake_us,
        manifest_us,
        transfer_us,
        elapsed_us: elapsed.as_micros() as u64,
        elapsed_ms: elapsed.as_millis() as u64,
        wire_mib_per_s: mib_per_sec(wire_bytes_total, elapsed),
        logical_mib_per_s: mib_per_sec(size, elapsed),
    };
    Ok((filename.to_string(), metrics))
}

fn send_file(file_path: &Path, server: &str) -> io::Result<()> {
    let (filename, m) = send_file_with_metrics(file_path, server)?;
    println!("FASTLINK sync complete: {filename}");
    println!("  file_bytes: {}", m.file_bytes);
    println!("  chunks_total: {}", m.chunks_total);
    println!("  chunks_reused: {}", m.chunks_reused);
    println!("  chunks_transferred: {}", m.chunks_transferred);
    println!("  raw_missing_bytes: {}", m.raw_missing_bytes);
    println!("  encoded_payload_bytes: {}", m.encoded_payload_bytes);
    println!("  handshake_wire_bytes: {}", m.handshake_wire_bytes);
    println!("  manifest_wire_bytes: {}", m.manifest_wire_bytes);
    println!("  need_wire_bytes: {}", m.need_wire_bytes);
    println!("  chunk_framing_wire_bytes: {}", m.chunk_framing_wire_bytes);
    println!("  status_wire_bytes: {}", m.status_wire_bytes);
    println!(
        "  headers_and_framing_wire_bytes: {}",
        m.headers_and_framing_wire_bytes
    );
    println!("  wire_bytes_tx: {}", m.wire_bytes_tx);
    println!("  wire_bytes_rx: {}", m.wire_bytes_rx);
    println!("  wire_bytes_total: {}", m.wire_bytes_total);
    println!(
        "  payload_bytes_avoided_vs_full_file: {}",
        m.file_bytes.saturating_sub(m.encoded_payload_bytes)
    );
    println!(
        "  wire_bytes_avoided_vs_full_file: {}",
        m.file_bytes.saturating_sub(m.wire_bytes_total)
    );
    println!("  handshake_us: {}", m.handshake_us);
    println!("  manifest_us: {}", m.manifest_us);
    println!("  transfer_us: {}", m.transfer_us);
    println!("  elapsed_us: {}", m.elapsed_us);
    println!("  elapsed_ms: {}", m.elapsed_ms);
    println!("  wire_mib_per_s: {:.3}", m.wire_mib_per_s);
    println!("  logical_mib_per_s: {:.3}", m.logical_mib_per_s);
    println!("  note: wire_bytes_total includes handshake, manifest, NEED list, chunk framing, encoded payloads, and status (excludes TCP/IP headers)");
    Ok(())
}

fn process_session(mut stream: TcpStream, root: &Path) -> io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(300)))?;
    stream.set_write_timeout(Some(Duration::from_secs(300)))?;
    server_handshake(&mut stream)?;
    let command = read_u8(&mut stream)?;
    if command != CMD_SYNC {
        return Err(invalid("unknown command"));
    }
    let name_len = read_u16(&mut stream)? as usize;
    if name_len == 0 || name_len > 240 {
        return Err(invalid("invalid filename length"));
    }
    let mut name_data = vec![0u8; name_len];
    stream.read_exact(&mut name_data)?;
    let name = String::from_utf8(name_data).map_err(|_| invalid("filename is not UTF-8"))?;
    safe_basename(&name)?;
    let size = read_u64(&mut stream)?;
    let chunk_bytes = read_u32(&mut stream)?;
    let count = read_u32(&mut stream)?;
    if chunk_bytes as usize != CHUNK_BYTES || count != chunk_count(size)? {
        return Err(invalid("invalid chunk size or count"));
    }
    let mut hashes = Vec::<Digest>::with_capacity(count as usize);
    for _ in 0..count {
        let mut hash = [0u8; 32];
        stream.read_exact(&mut hash)?;
        hashes.push(hash);
    }

    let mut missing = Vec::new();
    let mut requested_hashes = HashSet::<Digest>::new();
    for (idx, hash) in hashes.iter().enumerate() {
        if !cached_and_valid(&digest_path(root, hash), hash) && requested_hashes.insert(*hash) {
            missing.push(idx as u32);
        }
    }
    write_u32(&mut stream, missing.len() as u32)?;
    for idx in &missing {
        write_u32(&mut stream, *idx)?;
    }

    for expected_index in missing {
        let index = read_u32(&mut stream)?;
        if index != expected_index {
            return Err(invalid("chunk order mismatch"));
        }
        let codec = read_u8(&mut stream)?;
        let compressed_size = read_u32(&mut stream)? as usize;
        if compressed_size > MAX_ENCODED_BYTES {
            return Err(invalid("chunk payload too large"));
        }
        let mut encoded = vec![0u8; compressed_size];
        stream.read_exact(&mut encoded)?;
        let chunk = decode_chunk(codec, &encoded, expected_chunk_len(size, index)?)?;
        if digest(&chunk) != hashes[index as usize] {
            return Err(invalid("chunk hash mismatch"));
        }
        store_chunk(root, &hashes[index as usize], &chunk)?;
    }

    let output_dir = root.join("outputs");
    fs::create_dir_all(&output_dir)?;
    let output = output_dir.join(&name);
    let partial = output.with_extension("fastlink-partial");
    let result = (|| -> io::Result<()> {
        let mut destination = File::create(&partial)?;
        for (idx, hash) in hashes.iter().enumerate() {
            let bytes = fs::read(digest_path(root, hash))?;
            if bytes.len() != expected_chunk_len(size, idx as u32)? || digest(&bytes) != *hash {
                return Err(invalid("cache integrity failure"));
            }
            destination.write_all(&bytes)?;
        }
        destination.sync_all()?;
        drop(destination);
        fs::rename(&partial, &output)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(partial);
    }
    result?;
    write_u8(&mut stream, 0)?;
    println!("materialized {} ({} bytes)", output.display(), size);
    Ok(())
}

fn serve(address: &str, root: &Path) -> io::Result<()> {
    let socket: SocketAddr = address
        .parse()
        .map_err(|_| invalid("invalid listen address"))?;
    if !socket.ip().is_loopback() {
        return Err(invalid(
            "pre-alpha server must bind to loopback; use SSH forwarding for remote hosts",
        ));
    }
    fs::create_dir_all(root.join("chunks"))?;
    fs::create_dir_all(root.join("outputs"))?;
    let listener = TcpListener::bind(socket)?;
    println!("FASTLINK pre-alpha listening at {socket}");
    println!("Cache: {}", root.display());
    println!("UNAUTHENTICATED MVP: DO NOT expose port publicly; use an SSH tunnel");
    for connection in listener.incoming() {
        match connection {
            Ok(stream) => {
                if let Err(e) = process_session(stream, root) {
                    eprintln!("session error: {e}");
                }
            }
            Err(err) => eprintln!("accept error: {err}"),
        }
    }
    Ok(())
}

fn usage() {
    eprintln!("FASTLINK (CPU proof of concept)\n  fastlink serve [127.0.0.1:18777] [./.fastlink-data]\n  fastlink send FILE [127.0.0.1:18777]\nNo remote GPU scheduling yet.");
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let outcome = match args.get(1).map(String::as_str) {
        Some("serve") if args.len() <= 4 => {
            let address = args.get(2).map(String::as_str).unwrap_or("127.0.0.1:18777");
            let root = args
                .get(3)
                .map(String::as_str)
                .unwrap_or("./.fastlink-data");
            serve(address, Path::new(root))
        }
        Some("send") if (3..=4).contains(&args.len()) => {
            let server = args.get(3).map(String::as_str).unwrap_or("127.0.0.1:18777");
            send_file(Path::new(&args[2]), server)
        }
        _ => {
            usage();
            std::process::exit(2);
        }
    };
    if let Err(err) = outcome {
        eprintln!("FASTLINK error: {err}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::thread;

    fn temp_test_dir(prefix: &str) -> PathBuf {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "fastlink-test-{prefix}-{}-{id}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn loopback_cold_and_warm_wire_accounting_is_exact() {
        let dir = temp_test_dir("wire-accounting");
        let root = dir.join("cache");
        fs::create_dir_all(root.join("chunks")).unwrap();
        fs::create_dir_all(root.join("outputs")).unwrap();

        let src_path = dir.join("sample.bin");
        let payload: Vec<u8> = (0..(CHUNK_BYTES * 2))
            .map(|i| (i.wrapping_mul(31) % 251) as u8)
            .collect();
        fs::write(&src_path, &payload).unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let server_root = root.clone();

        let handle = thread::spawn(move || {
            for _ in 0..2 {
                let (stream, _) = listener.accept().unwrap();
                process_session(stream, &server_root).unwrap();
            }
        });

        let (_, cold) = send_file_with_metrics(&src_path, &addr).unwrap();
        assert_eq!(cold.file_bytes, (CHUNK_BYTES * 2) as u64);
        assert_eq!(cold.chunks_total, 2);
        assert_eq!(cold.chunks_reused, 0);
        assert_eq!(cold.chunks_transferred, 2);
        assert_eq!(cold.handshake_wire_bytes, 12);
        // "sample.bin" = 10 bytes; manifest = 19 + 10 + 2 * 32 = 93
        assert_eq!(cold.manifest_wire_bytes, 93);
        assert_eq!(cold.need_wire_bytes, 12);
        assert_eq!(cold.chunk_framing_wire_bytes, 18);
        assert_eq!(cold.status_wire_bytes, 1);
        assert_eq!(cold.headers_and_framing_wire_bytes, 136);
        assert_eq!(
            cold.wire_bytes_total,
            cold.headers_and_framing_wire_bytes + cold.encoded_payload_bytes
        );
        assert_eq!(
            cold.wire_bytes_tx + cold.wire_bytes_rx,
            cold.wire_bytes_total
        );
        assert!(cold.wire_mib_per_s > 0.0);
        assert!(cold.logical_mib_per_s > 0.0);

        let (_, warm) = send_file_with_metrics(&src_path, &addr).unwrap();
        assert_eq!(warm.chunks_reused, 2);
        assert_eq!(warm.chunks_transferred, 0);
        assert_eq!(warm.encoded_payload_bytes, 0);
        assert_eq!(warm.need_wire_bytes, 4);
        assert_eq!(warm.chunk_framing_wire_bytes, 0);
        assert_eq!(warm.wire_bytes_total, 12 + 93 + 4 + 1);
        assert_eq!(warm.wire_bytes_tx, 6 + 93);
        assert_eq!(warm.wire_bytes_rx, 6 + 4 + 1);
        assert!(warm.wire_mib_per_s > 0.0);
        assert!(warm.logical_mib_per_s > 0.0);

        handle.join().unwrap();
        let output_bytes = fs::read(root.join("outputs").join("sample.bin")).unwrap();
        assert_eq!(output_bytes, payload);
        let _ = fs::remove_dir_all(&dir);
    }
}
