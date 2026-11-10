//! Loopback latency and throughput bench for the FASTLINK-RPC/1 transport.
//!
//! Plain `std::time` harness, so the dependency tree stays empty:
//!
//! ```text
//! cargo bench --bench rpc_latency
//! ```
//!
//! The bench binds a loopback listener and serves one connection per scenario.
//! It prints single-chunk round-trip latency, 8 MiB throughput and wire-byte
//! overhead versus the raw tensor. Latency and throughput exclude connection
//! setup; setup is measured and printed separately. `TCP_NODELAY` is enabled on
//! both sockets so small request/response turns are not delayed by Nagle.

use fastlink::transport::{receive_tensor, send_tensor};
use fastlink::CHUNK_BYTES;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

const ACK: u8 = 0;
const WARMUP_ROUNDS: usize = 20;
const THROUGHPUT_WARMUP_ROUNDS: usize = 3;
const LATENCY_ROUNDS: usize = 500;
const THROUGHPUT_ROUNDS: usize = 32;
const TENSOR_BYTES: usize = 8 * 1024 * 1024;

/// Deterministic, effectively incompressible bytes (splitmix64).
fn patterned_tensor(len: usize) -> Vec<u8> {
    let mut state = 0x9E37_79B9_7F4A_7C15_u64;
    (0..len)
        .map(|_| {
            state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            (z ^ (z >> 31)) as u8
        })
        .collect()
}

/// Serve one connection: receive one tensor, ACK it, repeat until the peer closes.
fn serve_connection(mut sock: TcpStream) {
    sock.set_nodelay(true).expect("server TCP_NODELAY");
    loop {
        match receive_tensor(&mut sock) {
            Ok(_) => {
                if sock.write_all(&[ACK]).is_err() {
                    return;
                }
            }
            Err(_) => return,
        }
    }
}

fn start_server() -> (std::net::SocketAddr, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    let handle = thread::spawn(move || {
        let (sock, _) = listener.accept().expect("accept");
        serve_connection(sock);
    });
    (addr, handle)
}

fn connect(addr: std::net::SocketAddr) -> TcpStream {
    let sock = TcpStream::connect(addr).expect("connect");
    sock.set_nodelay(true).expect("client TCP_NODELAY");
    sock
}

fn round_trip(sock: &mut TcpStream, tensor: &[u8]) -> std::io::Result<Duration> {
    let started = Instant::now();
    send_tensor(sock, tensor)?;
    let mut ack = [0u8; 1];
    sock.read_exact(&mut ack)?;
    if ack[0] != ACK {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "unexpected ack",
        ));
    }
    Ok(started.elapsed())
}

fn bench_latency() {
    let (addr, server) = start_server();
    let mut sock = connect(addr);
    let tensor = patterned_tensor(CHUNK_BYTES);

    for _ in 0..WARMUP_ROUNDS {
        round_trip(&mut sock, &tensor).expect("latency warmup");
    }
    let mut samples = Vec::with_capacity(LATENCY_ROUNDS);
    for _ in 0..LATENCY_ROUNDS {
        let elapsed = round_trip(&mut sock, &tensor).expect("latency round-trip");
        samples.push(elapsed.as_secs_f64() * 1000.0);
    }
    samples.sort_unstable_by(|a, b| a.total_cmp(b));
    let min = samples[0];
    let median = samples[samples.len() / 2];
    let mean = samples.iter().sum::<f64>() / samples.len() as f64;
    println!(
        "single-chunk RTT: payload={payload} B rounds={rounds} min={min:.3} ms median={median:.3} ms mean={mean:.3} ms",
        payload = tensor.len(),
        rounds = LATENCY_ROUNDS,
    );
    drop(sock);
    let _ = server.join();
}

fn bench_throughput(label: &str, tensor: &[u8]) {
    let (addr, server) = start_server();
    let mut sock = connect(addr);
    for _ in 0..THROUGHPUT_WARMUP_ROUNDS {
        round_trip(&mut sock, tensor).expect("throughput warmup");
    }

    let mut wire_bytes = 0u64;
    let mut ack = [0u8; 1];
    let started = Instant::now();
    for _ in 0..THROUGHPUT_ROUNDS {
        wire_bytes += send_tensor(&mut sock, tensor).expect("send tensor") as u64;
        sock.read_exact(&mut ack).expect("ack");
    }
    let elapsed = started.elapsed();
    let secs = elapsed.as_secs_f64();
    let raw_bytes = tensor.len() as u64 * THROUGHPUT_ROUNDS as u64;
    let per_tensor_wire = wire_bytes / THROUGHPUT_ROUNDS as u64;
    let delta_bytes = per_tensor_wire as i64 - tensor.len() as i64;
    println!(
        "{label}: rounds={THROUGHPUT_ROUNDS} elapsed={:.2} ms raw={:.1} MB/s wire={:.1} MB/s \
         wire_bytes_per_tensor={per_tensor_wire} raw_bytes_per_tensor={} delta_bytes={delta_bytes}",
        secs * 1000.0,
        raw_bytes as f64 / secs / 1e6,
        wire_bytes as f64 / secs / 1e6,
        tensor.len(),
    );
    drop(sock);
    let _ = server.join();
}

fn bench_connection_setup() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    let server = thread::spawn(move || {
        let (_sock, _) = listener.accept().expect("accept");
    });
    let started = Instant::now();
    let sock = TcpStream::connect(addr).expect("connect");
    let setup = started.elapsed();
    println!(
        "connection setup (client connect): {:.3} ms",
        setup.as_secs_f64() * 1000.0
    );
    drop(sock);
    let _ = server.join();
}

fn main() {
    println!("FASTLINK-RPC/1 loopback transport bench");
    println!("chunk_bytes={CHUNK_BYTES} tensor_bytes={TENSOR_BYTES}");
    bench_latency();
    bench_throughput(
        "throughput patterned (RAW)",
        &patterned_tensor(TENSOR_BYTES),
    );
    bench_throughput("throughput uniform   (RLE)", &vec![0xAB; TENSOR_BYTES]);
    bench_connection_setup();
}
