//! FASTLINK CPU-only proof of concept: content-addressed, lossless delta sync.
//! This does NOT yet accelerate llama.cpp RPC or GPU execution.
use fastlink::{
    chunk_count, cli_plan, decode_chunk, devices, digest, discovery, encode_chunk,
    expected_chunk_len, hex, llama_rpc, read_u16, read_u32, read_u64, read_u8, write_u16,
    write_u32, write_u64, write_u8, Digest, CHUNK_BYTES, MAGIC, MAX_CHUNKS, MAX_ENCODED_BYTES,
    VERSION,
};
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
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

fn create_staging_file(output_dir: &Path, output: &Path) -> io::Result<(PathBuf, File)> {
    for attempt in 0..1024u32 {
        let candidate = output_dir.join(format!(
            ".fastlink-partial-{}-{}",
            std::process::id(),
            attempt
        ));
        if candidate == output {
            continue;
        }
        if let (Some(candidate_name), Some(output_name)) = (
            candidate.file_name().and_then(|name| name.to_str()),
            output.file_name().and_then(|name| name.to_str()),
        ) {
            if candidate_name.eq_ignore_ascii_case(output_name) {
                continue;
            }
        }
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => return Ok((candidate, file)),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        }
    }
    Err(invalid("could not create a unique output staging file"))
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

fn client_handshake(stream: &mut TcpStream) -> io::Result<()> {
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

fn server_handshake(stream: &mut TcpStream) -> io::Result<()> {
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

fn send_file(file_path: &Path, server: &str) -> io::Result<()> {
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

    let mut stream = TcpStream::connect(server)?;
    stream.set_read_timeout(Some(Duration::from_secs(300)))?;
    stream.set_write_timeout(Some(Duration::from_secs(300)))?;
    client_handshake(&mut stream)?;

    write_u8(&mut stream, CMD_SYNC)?;
    write_u16(&mut stream, name.len() as u16)?;
    stream.write_all(name)?;
    write_u64(&mut stream, size)?;
    write_u32(&mut stream, CHUNK_BYTES as u32)?;
    write_u32(&mut stream, count)?;
    for hash in &hashes {
        stream.write_all(hash)?;
    }
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
    let status = read_u8(&mut stream)?;
    if status != 0 {
        return Err(invalid("server failed to materialize output"));
    }
    println!("FASTLINK sync complete: {filename}");
    println!("  file_bytes: {size}");
    println!("  chunks_total: {count}");
    println!("  chunks_reused: {}", count - missing_len);
    println!("  chunks_transferred: {missing_len}");
    println!("  raw_missing_bytes: {raw_bytes_selected}");
    println!("  encoded_payload_bytes: {wire_payload_bytes}");
    println!(
        "  payload_bytes_avoided_vs_full_file: {}",
        size.saturating_sub(wire_payload_bytes)
    );
    println!("  elapsed_ms: {}", started.elapsed().as_millis());
    println!("  note: payload counts exclude wire framing, hashes, and TCP overhead");
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
    let (partial, mut destination) = create_staging_file(&output_dir, &output)?;
    let mut result: io::Result<()> = Ok(());
    for (idx, hash) in hashes.iter().enumerate() {
        let bytes = match fs::read(digest_path(root, hash)) {
            Ok(bytes) => bytes,
            Err(err) => {
                result = Err(err);
                break;
            }
        };
        let expected = match expected_chunk_len(size, idx as u32) {
            Ok(expected) => expected,
            Err(err) => {
                result = Err(err);
                break;
            }
        };
        if bytes.len() != expected || digest(&bytes) != *hash {
            result = Err(invalid("cache integrity failure"));
            break;
        }
        if let Err(err) = destination.write_all(&bytes) {
            result = Err(err);
            break;
        }
    }
    if result.is_ok() {
        if let Err(err) = destination.sync_all() {
            result = Err(err);
        }
    }
    drop(destination);
    if result.is_ok() {
        if let Err(err) = fs::rename(&partial, &output) {
            result = Err(err);
        }
    }
    if result.is_err() {
        let _ = fs::remove_file(&partial);
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
    let status_usage = if cfg!(feature = "rpc-v3-transport") {
        "  fastlink status (rpc-v3-transport)\n"
    } else {
        ""
    };
    eprintln!(
        "FASTLINK (CPU proof of concept)\n  fastlink serve [127.0.0.1:18777] [./.fastlink-data]\n  fastlink send FILE [127.0.0.1:18777]\n  fastlink devices [--json]   (Linux-only live inventory)\n  fastlink discover [--json]\n  fastlink plan [--nodes 1|2|3|4|7|16] [--objective largest|latency|throughput] [--model small|70b] [--json]\n  fastlink api --help\n  fastlink rpc-bench --server PATH --model PATH [--rpc HOST:PORT] [--fastlink]\n{status_usage}No remote GPU scheduling yet."
    );
}

fn main() {
    // User-facing command dispatch only; these arguments are not an
    // authenticated identity and grant no network or compute permission.
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
        Some("status") if cfg!(feature = "rpc-v3-transport") && args.len() == 2 => {
            println!("FASTLINK-RPC/1 status (rpc-v3-transport feature gate)");
            println!("  Synchronous blocking loopback Pool/AsyncClient (legacy name) with serialized round-robin batch_send");
            println!("  Telemetry: bytes_sent and latency_samples tracked per client");
            println!("  Not implemented: adaptive batch sizing, pool shrink/expand, connection warmup, async I/O, keep-alive");
            println!(
                "  (run 'cargo bench --bench rpc_latency' for loopback transport measurements)"
            );
            Ok(())
        }
        Some("devices") => devices::run_devices(&args[2..]).map(|output| print!("{output}")),
        Some("discover") => {
            let rest: Vec<String> = args[2..].to_vec();
            let output = match discovery::run_discover(&rest) {
                Ok(output) => output,
                Err(err) => {
                    eprintln!("FASTLINK error: {err}");
                    std::process::exit(1);
                }
            };
            print!("{output}");
            Ok(())
        }
        Some("plan") => {
            let rest: Vec<String> = args[2..].to_vec();
            let output = match cli_plan::run_plan(&rest) {
                Ok(output) => output,
                Err(err) => {
                    eprintln!("FASTLINK error: {err}");
                    std::process::exit(1);
                }
            };
            print!("{output}");
            Ok(())
        }
        Some("api") => fastlink::api_server::run_api_cli(&args[2..]),
        Some("rpc-bench") => {
            let rest: Vec<String> = args[2..].to_vec();
            let output = match llama_rpc::run_rpc_bench_cli(&rest) {
                Ok(output) => output,
                Err(err) => {
                    eprintln!("FASTLINK error: {err}");
                    std::process::exit(1);
                }
            };
            print!("{output}");
            Ok(())
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
    use std::thread;

    #[test]
    fn sync_does_not_clobber_existing_partial_named_output() {
        let root = tempfile::tempdir().expect("temp root");
        let source_dir = root.path().join("source");
        let outputs = root.path().join("outputs");
        fs::create_dir_all(&source_dir).expect("source dir");
        fs::create_dir_all(&outputs).expect("outputs dir");

        let sentinel = b"pre-existing completed output";
        let sentinel_path = outputs.join("victim.fastlink-partial");
        fs::write(&sentinel_path, sentinel).expect("sentinel");

        let payload = b"new completed output payload";
        let source = source_dir.join("victim");
        fs::write(&source, payload).expect("source file");

        let listener = TcpListener::bind("127.0.0.1:0").expect("loopback listener");
        let address = listener.local_addr().expect("listener address");
        let server_root = root.path().to_path_buf();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept client");
            process_session(stream, &server_root)
        });

        send_file(&source, &address.to_string()).expect("sync source");
        server.join().expect("server thread").expect("session");

        assert_eq!(
            fs::read(outputs.join("victim")).expect("final output"),
            payload,
            "the completed output must match the source"
        );
        assert_eq!(
            fs::read(&sentinel_path).expect("pre-existing output"),
            sentinel,
            "a valid output named like the old staging suffix must not be clobbered"
        );
    }

    #[test]
    fn bad_digest_chunk_is_rejected_without_caching_or_replacing_output() {
        let root = tempfile::tempdir().expect("temp root");
        let outputs = root.path().join("outputs");
        fs::create_dir_all(&outputs).expect("outputs dir");

        let sentinel = b"pre-existing completed output";
        let output = outputs.join("victim");
        fs::write(&output, sentinel).expect("pre-existing output");

        let expected = b"expected bytes A";
        let advertised_hash = digest(expected);
        let mut corrupt = expected.to_vec();
        corrupt[0] ^= 0xFF;
        assert_ne!(digest(&corrupt), advertised_hash);

        let listener = TcpListener::bind("127.0.0.1:0").expect("loopback listener");
        let address = listener.local_addr().expect("listener address");
        let server_root = root.path().to_path_buf();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept client");
            process_session(stream, &server_root)
        });

        let mut client = TcpStream::connect(address).expect("connect");
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("client read timeout");
        client
            .set_write_timeout(Some(Duration::from_secs(5)))
            .expect("client write timeout");

        let client_result = (|| -> io::Result<()> {
            client_handshake(&mut client)?;
            write_u8(&mut client, CMD_SYNC)?;
            write_u16(&mut client, "victim".len() as u16)?;
            client.write_all(b"victim")?;
            write_u64(&mut client, expected.len() as u64)?;
            write_u32(&mut client, CHUNK_BYTES as u32)?;
            write_u32(&mut client, 1)?;
            client.write_all(&advertised_hash)?;

            let missing_len = read_u32(&mut client)?;
            if missing_len != 1 {
                return Err(invalid("expected one missing chunk"));
            }
            let missing_index = read_u32(&mut client)?;
            if missing_index != 0 {
                return Err(invalid("expected missing chunk index zero"));
            }

            write_u32(&mut client, 0)?;
            write_u8(&mut client, fastlink::RAW)?;
            write_u32(&mut client, corrupt.len() as u32)?;
            client.write_all(&corrupt)?;
            client.flush()?;
            Ok(())
        })();
        drop(client);
        let server_result = server.join().expect("server thread");
        client_result.expect("malicious client protocol");

        let error = server_result.expect_err("bad digest must fail the session");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(
            fs::read(&output).expect("pre-existing output"),
            sentinel,
            "a rejected chunk must not replace the completed output"
        );
        assert!(
            !digest_path(root.path(), &advertised_hash).exists(),
            "a rejected chunk must not be stored under its advertised digest"
        );
        assert!(
            error.to_string().contains("chunk hash mismatch"),
            "unexpected server error: {error}"
        );
    }

    #[test]
    fn staging_skips_ascii_case_folded_final_output_alias() {
        let root = tempfile::tempdir().expect("temp root");
        let outputs = root.path().join("outputs");
        fs::create_dir_all(&outputs).expect("outputs dir");

        let final_name = format!(".FASTLINK-PARTIAL-{}-0", std::process::id());
        let output = outputs.join(&final_name);
        let (staging, handle) = create_staging_file(&outputs, &output).expect("staging file");
        let staging_name = staging
            .file_name()
            .and_then(|name| name.to_str())
            .expect("ASCII staging name");

        assert!(
            !staging_name.eq_ignore_ascii_case(&final_name),
            "staging basename {staging_name:?} must not alias final basename {final_name:?}"
        );
        assert!(
            !output.exists(),
            "the final output path must not exist while its staging handle is open"
        );

        drop(handle);
        fs::remove_file(&staging).expect("remove staging file");
    }
}
