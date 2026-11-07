//! FASTLINK CPU-only proof of concept: content-addressed, lossless delta sync.
//! This does NOT yet accelerate llama.cpp RPC or GPU execution.
use fastlink::{send_file_resumable, serve, SendOptions};
use std::io;
use std::path::Path;

fn send_file(file_path: &Path, server: &str) -> io::Result<()> {
    let (filename, m) = send_file_resumable(file_path, server, SendOptions::default())?;
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
    use fastlink::{process_session, send_file_with_metrics, CHUNK_BYTES};
    use std::fs;
    use std::net::TcpListener;
    use std::path::PathBuf;
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
