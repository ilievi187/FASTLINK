//! FASTLINK CPU-only proof of concept: content-addressed, lossless delta sync.
//! This does NOT yet accelerate llama.cpp RPC or GPU execution.
//!
//! The CLI is a thin shell over the `fastlink` library: session logic lives in
//! `src/lib.rs` so it can be exercised by integration tests over real TCP.

use fastlink::{
    client_sync, recover_atomic_writes, server_sync, validate_connect_address,
    validate_listen_address, TransferStats,
};
use std::fs;
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

/// Default session timeout for both directions on loopback.
const SESSION_TIMEOUT: Duration = Duration::from_secs(300);

fn setup_timeouts(stream: &TcpStream) -> std::io::Result<()> {
    stream.set_read_timeout(Some(SESSION_TIMEOUT))?;
    stream.set_write_timeout(Some(SESSION_TIMEOUT))?;
    Ok(())
}

fn print_stats(stats: &TransferStats, direction: &str) {
    println!("{direction}: {} ({})", stats.filename, stats.file_bytes);
    println!("  file_bytes: {}", stats.file_bytes);
    println!("  chunks_total: {}", stats.chunks_total);
    println!("  chunks_reused: {}", stats.chunks_reused);
    println!("  chunks_transferred: {}", stats.chunks_transferred);
    println!("  raw_missing_bytes: {}", stats.raw_missing_bytes);
    println!("  encoded_payload_bytes: {}", stats.encoded_payload_bytes);
    println!(
        "  payload_bytes_avoided_vs_full_file: {}",
        stats.file_bytes.saturating_sub(stats.encoded_payload_bytes)
    );
    println!("  note: payload counts exclude wire framing, hashes, and TCP overhead");
}

fn serve(address: &str, root: &Path) -> std::io::Result<()> {
    let socket = validate_listen_address(address)?;
    fs::create_dir_all(root.join("chunks"))?;
    fs::create_dir_all(root.join("outputs"))?;
    recover_atomic_writes(root)?;
    let listener = TcpListener::bind(socket)?;
    println!("FASTLINK pre-alpha listening at {socket}");
    println!("Cache: {}", root.display());
    println!("UNAUTHENTICATED MVP: DO NOT expose port publicly; use an SSH tunnel");
    for connection in listener.incoming() {
        match connection {
            Ok(stream) => {
                let root = root.to_path_buf();
                // One thread per session so a single misbehaving peer cannot
                // stall the accept loop indefinitely.
                let handle = thread::spawn(move || {
                    if let Err(e) = serve_session(stream, &root) {
                        eprintln!("session error: {e}");
                    }
                });
                let _ = handle.join();
            }
            Err(err) => eprintln!("accept error: {err}"),
        }
    }
    Ok(())
}

fn serve_session(stream: TcpStream, root: &Path) -> std::io::Result<()> {
    let mut stream = stream;
    setup_timeouts(&stream)?;
    let stats = server_sync(&mut stream, root)?;
    println!(
        "materialized {} ({} bytes)",
        root.join("outputs").join(stats.filename).display(),
        stats.file_bytes
    );
    Ok(())
}

fn send_file(file_path: &Path, server: &str) -> std::io::Result<()> {
    let started = Instant::now();
    let filename = file_path
        .file_name()
        .and_then(|x| x.to_str())
        .ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid input file name")
        })?;
    let socket = validate_connect_address(server)?;
    let mut stream = TcpStream::connect(socket)?;
    setup_timeouts(&stream)?;
    let stats = client_sync(&mut stream, filename, file_path)?;
    print_stats(&stats, "FASTLINK sync complete");
    println!("  elapsed_ms: {}", started.elapsed().as_millis());
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
