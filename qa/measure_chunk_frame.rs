//! Standalone loopback microbench for v0.1 chunk framing.
//!
//! Not part of the Cargo package. Compile and run with:
//!
//! ```text
//! rustc -O -o /tmp/fl3-chunk-frame qa/measure_chunk_frame.rs
//! /tmp/fl3-chunk-frame
//! ```
//!
//! kind 0 writes the 9-byte header as three `write_all` calls plus the payload.
//! kind 1 copies the header and payload into one buffer, then one `write_all`.
//! kind 2 sends the same four slices with `write_vectored` (the library path).
//! The listener is `127.0.0.1` and the socket leaves `TCP_NODELAY` unset, which
//! matches the FASTLINK CLI. This process does not hash, touch disk, or use a GPU.

use std::hint::black_box;
use std::io::{IoSlice, Read, Write};
use std::net::TcpListener;
use std::time::Instant;

fn median(mut values: Vec<u128>) -> u128 {
    values.sort_unstable();
    values[values.len() / 2]
}

fn write_vectored_all(w: &mut impl Write, mut bufs: &mut [IoSlice<'_>]) -> std::io::Result<()> {
    while !bufs.is_empty() {
        match w.write_vectored(bufs) {
            Ok(0) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "short write",
                ))
            }
            Ok(n) => IoSlice::advance_slices(&mut bufs, n),
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        }
    }
    Ok(())
}

fn copy_bench() {
    let src = vec![0x5Au8; 65_536];
    let iters = 2_000u32;
    let mut two = Vec::new();
    let mut one = Vec::new();
    for _trial in 0..5 {
        let started = Instant::now();
        for _ in 0..iters {
            let owned = src.clone();
            let decoded = owned.clone();
            black_box(decoded);
        }
        two.push(started.elapsed().as_nanos());
        let started = Instant::now();
        for _ in 0..iters {
            let owned = src.clone();
            let decoded = owned;
            black_box(decoded);
        }
        one.push(started.elapsed().as_nanos());
    }
    println!(
        "copy median_ns two_copies={} move={} iters={} chunk={}",
        median(two),
        median(one),
        iters,
        src.len()
    );
}

fn tcp_once(kind: u8, chunks: usize) -> u128 {
    let payload = vec![0xABu8; 65_536];
    let frame_len = 9 + payload.len();
    let total = chunks * frame_len;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        let mut buf = vec![0u8; 256 * 1024];
        let mut got = 0usize;
        while got < total {
            let n = sock.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            got += n;
        }
        got
    });
    let mut client = std::net::TcpStream::connect(addr).unwrap();
    let started = Instant::now();
    match kind {
        0 => {
            for index in 0..chunks as u32 {
                client.write_all(&index.to_le_bytes()).unwrap();
                client.write_all(&[0u8]).unwrap();
                client
                    .write_all(&(payload.len() as u32).to_le_bytes())
                    .unwrap();
                client.write_all(&payload).unwrap();
            }
        }
        1 => {
            let mut frame = Vec::with_capacity(frame_len);
            for index in 0..chunks as u32 {
                frame.clear();
                frame.extend_from_slice(&index.to_le_bytes());
                frame.push(0);
                frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
                frame.extend_from_slice(&payload);
                client.write_all(&frame).unwrap();
            }
        }
        _ => {
            for index in 0..chunks as u32 {
                let index_bytes = index.to_le_bytes();
                let codec = [0u8];
                let len_bytes = (payload.len() as u32).to_le_bytes();
                let mut bufs = [
                    IoSlice::new(&index_bytes),
                    IoSlice::new(&codec),
                    IoSlice::new(&len_bytes),
                    IoSlice::new(&payload),
                ];
                write_vectored_all(&mut client, &mut bufs).unwrap();
            }
        }
    }
    drop(client);
    let elapsed = started.elapsed().as_nanos();
    assert_eq!(server.join().unwrap(), total);
    elapsed
}

fn main() {
    copy_bench();
    let chunks = 128usize;
    for _ in 0..2 {
        let _ = tcp_once(0, 4);
        let _ = tcp_once(1, 4);
        let _ = tcp_once(2, 4);
    }
    for kind in 0..3u8 {
        let mut samples = Vec::new();
        for _ in 0..5 {
            samples.push(tcp_once(kind, chunks));
        }
        println!(
            "kind={kind} median_ns={} chunks={chunks}",
            median(samples)
        );
    }
}
