//! High-performance GPU tensor transfer benchmark:
//! Memory-Mapped (mmap / POSIX shm) vs Raw TCP vs FASTLINK-RPC/1 Transport.
//!
//! Measures:
//! 1. Single-chunk (64 KiB) round-trip latency (min, p50, p95, p99, mean)
//! 2. 8 MiB tensor transfer throughput (Patterned incompressible vs Uniform compressible)
//! 3. Wire / memory overhead and zero-copy bandwidth
//!
//! Run with:
//! ```text
//! cargo bench --bench mmap_transfer
//! ```

use fastlink::transport::{receive_tensor, send_tensor};
use fastlink::CHUNK_BYTES;
use std::ffi::CString;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
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

/// Helper struct for POSIX shared memory mapped region.
struct MmapShm {
    name: CString,
    ptr: *mut u8,
    len: usize,
    fd: i32,
}

unsafe impl Send for MmapShm {}
unsafe impl Sync for MmapShm {}

#[cfg(unix)]
impl MmapShm {
    fn create(name_str: &str, len: usize) -> std::io::Result<Self> {
        let name = CString::new(name_str).unwrap();
        unsafe {
            libc::shm_unlink(name.as_ptr());
            let fd = libc::shm_open(
                name.as_ptr(),
                libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
                0o600,
            );
            if fd < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::ftruncate(fd, len as libc::off_t) < 0 {
                libc::close(fd);
                libc::shm_unlink(name.as_ptr());
                return Err(std::io::Error::last_os_error());
            }
            let ptr = libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            );
            if ptr == libc::MAP_FAILED {
                libc::close(fd);
                libc::shm_unlink(name.as_ptr());
                return Err(std::io::Error::last_os_error());
            }
            Ok(Self {
                name,
                ptr: ptr as *mut u8,
                len,
                fd,
            })
        }
    }

    fn open(name_str: &str, len: usize) -> std::io::Result<Self> {
        let name = CString::new(name_str).unwrap();
        unsafe {
            let fd = libc::shm_open(name.as_ptr(), libc::O_RDWR, 0o600);
            if fd < 0 {
                return Err(std::io::Error::last_os_error());
            }
            let ptr = libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            );
            if ptr == libc::MAP_FAILED {
                libc::close(fd);
                return Err(std::io::Error::last_os_error());
            }
            Ok(Self {
                name,
                ptr: ptr as *mut u8,
                len,
                fd,
            })
        }
    }
}

#[cfg(windows)]
impl MmapShm {
    fn create(_name_str: &str, _len: usize) -> std::io::Result<Self> {
        let ptr = Box::into_ptr(Box::new([0u8; 1]));
        Ok(Self {
            name: CString::new("").unwrap(),
            ptr,
            len: 0,
            fd: 0,
        })
    }

    fn open(_name_str: &str, _len: usize) -> std::io::Result<Self> {
        let ptr = Box::into_ptr(Box::new([0u8; 1]));
        Ok(Self {
            name: CString::new("").unwrap(),
            ptr,
            len: 0,
            fd: 0,
        })
    }
}

#[cfg(unix)]
impl Drop for MmapShm {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.ptr as *mut libc::c_void, self.len);
            libc::close(self.fd);
            libc::shm_unlink(self.name.as_ptr());
        }
    }
}

#[cfg(windows)]
impl Drop for MmapShm {
    fn drop(&mut self) {}
}

// -----------------------------------------------------------------------------
// Percentile calculation
// -----------------------------------------------------------------------------
struct LatencyPercentiles {
    min: f64,
    p50: f64,
    p95: f64,
    p99: f64,
    mean: f64,
}

fn calculate_percentiles(mut samples: Vec<f64>) -> LatencyPercentiles {
    samples.sort_unstable_by(|a, b| a.total_cmp(b));
    let len = samples.len();
    let min = samples[0];
    let p50 = samples[len * 50 / 100];
    let p95 = samples[len * 95 / 100];
    let p99 = samples[len * 99 / 100];
    let mean = samples.iter().sum::<f64>() / len as f64;
    LatencyPercentiles {
        min,
        p50,
        p95,
        p99,
        mean,
    }
}

// -----------------------------------------------------------------------------
// MMAP Benchmarks
// -----------------------------------------------------------------------------
fn bench_mmap_latency() -> LatencyPercentiles {
    let shm_name = "/fastlink_bench_lat_64k";
    let shm = MmapShm::create(shm_name, CHUNK_BYTES).expect("create shm");
    let ready_flag = Arc::new(AtomicBool::new(false));
    let ack_flag = Arc::new(AtomicBool::new(false));
    let stop_flag = Arc::new(AtomicBool::new(false));

    let ready_c = ready_flag.clone();
    let ack_c = ack_flag.clone();
    let stop_c = stop_flag.clone();

    // Consumer thread simulates GPU worker ingesting tensor from mmap region
    let consumer = thread::spawn(move || {
        let consumer_shm = MmapShm::open(shm_name, CHUNK_BYTES).expect("open shm in consumer");
        let mut dest_buf = vec![0u8; CHUNK_BYTES];
        while !stop_c.load(Ordering::Acquire) {
            if ready_c.load(Ordering::Acquire) {
                unsafe {
                    ptr::copy_nonoverlapping(consumer_shm.ptr, dest_buf.as_mut_ptr(), CHUNK_BYTES);
                }
                ready_c.store(false, Ordering::Release);
                ack_c.store(true, Ordering::Release);
            }
            std::hint::spin_loop();
        }
    });

    let tensor = patterned_tensor(CHUNK_BYTES);

    // Warmup
    for _ in 0..WARMUP_ROUNDS {
        unsafe {
            ptr::copy_nonoverlapping(tensor.as_ptr(), shm.ptr, CHUNK_BYTES);
        }
        ready_flag.store(true, Ordering::Release);
        while !ack_flag.load(Ordering::Acquire) {
            std::hint::spin_loop();
        }
        ack_flag.store(false, Ordering::Release);
    }

    let mut samples = Vec::with_capacity(LATENCY_ROUNDS);
    for _ in 0..LATENCY_ROUNDS {
        let t0 = Instant::now();
        unsafe {
            ptr::copy_nonoverlapping(tensor.as_ptr(), shm.ptr, CHUNK_BYTES);
        }
        ready_flag.store(true, Ordering::Release);
        while !ack_flag.load(Ordering::Acquire) {
            std::hint::spin_loop();
        }
        ack_flag.store(false, Ordering::Release);
        samples.push(t0.elapsed().as_secs_f64() * 1000.0);
    }

    stop_flag.store(true, Ordering::Release);
    let _ = consumer.join();

    let p = calculate_percentiles(samples);
    println!(
        "single-chunk RTT (mmap shm): payload={CHUNK_BYTES} B rounds={LATENCY_ROUNDS} min={:.3} ms p50={:.3} ms p95={:.3} ms p99={:.3} ms mean={:.3} ms",
        p.min, p.p50, p.p95, p.p99, p.mean
    );
    p
}

fn bench_mmap_throughput(label: &str, tensor: &[u8]) -> f64 {
    let shm_name = "/fastlink_bench_tp_8m";
    let len = tensor.len();
    let shm = MmapShm::create(shm_name, len).expect("create shm");
    let ready_flag = Arc::new(AtomicBool::new(false));
    let ack_flag = Arc::new(AtomicBool::new(false));
    let stop_flag = Arc::new(AtomicBool::new(false));

    let ready_c = ready_flag.clone();
    let ack_c = ack_flag.clone();
    let stop_c = stop_flag.clone();

    let consumer = thread::spawn(move || {
        let consumer_shm = MmapShm::open(shm_name, len).expect("open shm in consumer");
        let mut dest_buf = vec![0u8; len];
        while !stop_c.load(Ordering::Acquire) {
            if ready_c.load(Ordering::Acquire) {
                unsafe {
                    ptr::copy_nonoverlapping(consumer_shm.ptr, dest_buf.as_mut_ptr(), len);
                }
                ready_c.store(false, Ordering::Release);
                ack_c.store(true, Ordering::Release);
            }
            std::hint::spin_loop();
        }
    });

    // Warmup
    for _ in 0..THROUGHPUT_WARMUP_ROUNDS {
        unsafe {
            ptr::copy_nonoverlapping(tensor.as_ptr(), shm.ptr, len);
        }
        ready_flag.store(true, Ordering::Release);
        while !ack_flag.load(Ordering::Acquire) {
            std::hint::spin_loop();
        }
        ack_flag.store(false, Ordering::Release);
    }

    let started = Instant::now();
    for _ in 0..THROUGHPUT_ROUNDS {
        unsafe {
            ptr::copy_nonoverlapping(tensor.as_ptr(), shm.ptr, len);
        }
        ready_flag.store(true, Ordering::Release);
        while !ack_flag.load(Ordering::Acquire) {
            std::hint::spin_loop();
        }
        ack_flag.store(false, Ordering::Release);
    }
    let elapsed = started.elapsed();
    stop_flag.store(true, Ordering::Release);
    let _ = consumer.join();

    let secs = elapsed.as_secs_f64();
    let raw_bytes = len as u64 * THROUGHPUT_ROUNDS as u64;
    let throughput = raw_bytes as f64 / secs / 1e6;
    println!(
        "{label}: rounds={THROUGHPUT_ROUNDS} elapsed={:.2} ms throughput={:.1} MB/s bytes_per_tensor={}",
        secs * 1000.0,
        throughput,
        len
    );
    throughput
}

// -----------------------------------------------------------------------------
// Raw TCP Benchmarks
// -----------------------------------------------------------------------------
fn serve_raw_connection(mut sock: TcpStream, expected_len: usize) {
    sock.set_nodelay(true).expect("server TCP_NODELAY");
    let mut buf = vec![0u8; expected_len.min(64 * 1024)];
    loop {
        let mut remaining = expected_len;
        while remaining > 0 {
            let to_read = remaining.min(buf.len());
            match sock.read_exact(&mut buf[..to_read]) {
                Ok(_) => remaining -= to_read,
                Err(_) => return,
            }
        }
        if sock.write_all(&[ACK]).is_err() {
            return;
        }
    }
}

fn start_raw_server(expected_len: usize) -> (std::net::SocketAddr, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    let handle = thread::spawn(move || {
        let (sock, _) = listener.accept().expect("accept");
        serve_raw_connection(sock, expected_len);
    });
    (addr, handle)
}

fn connect(addr: std::net::SocketAddr) -> TcpStream {
    let sock = TcpStream::connect(addr).expect("connect");
    sock.set_nodelay(true).expect("client TCP_NODELAY");
    sock
}

fn raw_round_trip(sock: &mut TcpStream, tensor: &[u8]) -> std::io::Result<Duration> {
    let started = Instant::now();
    sock.write_all(tensor)?;
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

fn bench_raw_latency() -> LatencyPercentiles {
    let (addr, server) = start_raw_server(CHUNK_BYTES);
    let mut sock = connect(addr);
    let tensor = patterned_tensor(CHUNK_BYTES);

    for _ in 0..WARMUP_ROUNDS {
        raw_round_trip(&mut sock, &tensor).expect("raw latency warmup");
    }
    let mut samples = Vec::with_capacity(LATENCY_ROUNDS);
    for _ in 0..LATENCY_ROUNDS {
        let elapsed = raw_round_trip(&mut sock, &tensor).expect("raw latency round-trip");
        samples.push(elapsed.as_secs_f64() * 1000.0);
    }
    drop(sock);
    let _ = server.join();

    let p = calculate_percentiles(samples);
    println!(
        "single-chunk RTT (Raw TCP) : payload={CHUNK_BYTES} B rounds={LATENCY_ROUNDS} min={:.3} ms p50={:.3} ms p95={:.3} ms p99={:.3} ms mean={:.3} ms",
        p.min, p.p50, p.p95, p.p99, p.mean
    );
    p
}

fn bench_raw_throughput(label: &str, tensor: &[u8]) -> f64 {
    let (addr, server) = start_raw_server(tensor.len());
    let mut sock = connect(addr);
    for _ in 0..THROUGHPUT_WARMUP_ROUNDS {
        raw_round_trip(&mut sock, tensor).expect("raw throughput warmup");
    }

    let started = Instant::now();
    for _ in 0..THROUGHPUT_ROUNDS {
        sock.write_all(tensor).expect("send raw tensor");
        let mut ack = [0u8; 1];
        sock.read_exact(&mut ack).expect("ack");
    }
    let elapsed = started.elapsed();
    let secs = elapsed.as_secs_f64();
    let raw_bytes = tensor.len() as u64 * THROUGHPUT_ROUNDS as u64;
    let tp = raw_bytes as f64 / secs / 1e6;
    println!(
        "{label}: rounds={THROUGHPUT_ROUNDS} elapsed={:.2} ms throughput={:.1} MB/s bytes_per_tensor={}",
        secs * 1000.0,
        tp,
        tensor.len(),
    );
    drop(sock);
    let _ = server.join();
    tp
}

// -----------------------------------------------------------------------------
// FASTLINK-RPC/1 Transport Benchmarks
// -----------------------------------------------------------------------------
fn serve_fastlink_connection(mut sock: TcpStream) {
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

fn start_fastlink_server() -> (std::net::SocketAddr, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    let handle = thread::spawn(move || {
        let (sock, _) = listener.accept().expect("accept");
        serve_fastlink_connection(sock);
    });
    (addr, handle)
}

fn fastlink_round_trip(sock: &mut TcpStream, tensor: &[u8]) -> std::io::Result<Duration> {
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

fn bench_fastlink_latency() -> LatencyPercentiles {
    let (addr, server) = start_fastlink_server();
    let mut sock = connect(addr);
    let tensor = patterned_tensor(CHUNK_BYTES);

    for _ in 0..WARMUP_ROUNDS {
        fastlink_round_trip(&mut sock, &tensor).expect("fastlink latency warmup");
    }
    let mut samples = Vec::with_capacity(LATENCY_ROUNDS);
    for _ in 0..LATENCY_ROUNDS {
        let elapsed = fastlink_round_trip(&mut sock, &tensor).expect("fastlink round-trip");
        samples.push(elapsed.as_secs_f64() * 1000.0);
    }
    drop(sock);
    let _ = server.join();

    let p = calculate_percentiles(samples);
    println!(
        "single-chunk RTT (FASTLINK): payload={CHUNK_BYTES} B rounds={LATENCY_ROUNDS} min={:.3} ms p50={:.3} ms p95={:.3} ms p99={:.3} ms mean={:.3} ms",
        p.min, p.p50, p.p95, p.p99, p.mean
    );
    p
}

fn bench_fastlink_throughput(label: &str, tensor: &[u8]) -> (f64, f64, u64, i64) {
    let (addr, server) = start_fastlink_server();
    let mut sock = connect(addr);
    for _ in 0..THROUGHPUT_WARMUP_ROUNDS {
        fastlink_round_trip(&mut sock, tensor).expect("fastlink throughput warmup");
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
    let raw_tp = raw_bytes as f64 / secs / 1e6;
    let wire_tp = wire_bytes as f64 / secs / 1e6;
    let per_tensor_wire = wire_bytes / THROUGHPUT_ROUNDS as u64;
    let delta_bytes = per_tensor_wire as i64 - tensor.len() as i64;
    println!(
        "{label}: rounds={THROUGHPUT_ROUNDS} elapsed={:.2} ms raw={:.1} MB/s wire={:.1} MB/s \
         wire_bytes={per_tensor_wire} delta={delta_bytes} B",
        secs * 1000.0,
        raw_tp,
        wire_tp,
    );
    drop(sock);
    let _ = server.join();
    (raw_tp, wire_tp, per_tensor_wire, delta_bytes)
}

fn main() {
    println!("================================================================================");
    println!("FASTLINK A-3: GPU Tensor Transfer Harness (mmap vs Raw TCP vs FASTLINK-RPC/1)");
    println!("chunk_bytes={CHUNK_BYTES} tensor_bytes={TENSOR_BYTES}");
    println!("================================================================================");

    println!("\n--- [1] Single-Chunk Round-Trip Latency (64 KiB, 500 rounds) ---");
    let mmap_lat = bench_mmap_latency();
    let raw_lat = bench_raw_latency();
    let fl_lat = bench_fastlink_latency();

    println!("\nLatency Summary (64 KiB RTT):");
    println!("  mmap shm     : min={:.3} ms | p50={:.3} ms | p95={:.3} ms | p99={:.3} ms | mean={:.3} ms", mmap_lat.min, mmap_lat.p50, mmap_lat.p95, mmap_lat.p99, mmap_lat.mean);
    println!("  Raw TCP      : min={:.3} ms | p50={:.3} ms | p95={:.3} ms | p99={:.3} ms | mean={:.3} ms", raw_lat.min, raw_lat.p50, raw_lat.p95, raw_lat.p99, raw_lat.mean);
    println!("  FASTLINK-RPC : min={:.3} ms | p50={:.3} ms | p95={:.3} ms | p99={:.3} ms | mean={:.3} ms", fl_lat.min, fl_lat.p50, fl_lat.p95, fl_lat.p99, fl_lat.mean);

    println!("\n--- [2] 8 MiB Patterned Incompressible Tensor Throughput ---");
    let pat = patterned_tensor(TENSOR_BYTES);
    let mmap_pat_tp = bench_mmap_throughput("mmap shm transfer (patterned) ", &pat);
    let raw_pat_tp = bench_raw_throughput("Raw TCP streaming (patterned) ", &pat);
    let (fl_pat_raw, _fl_pat_wire, _, _) =
        bench_fastlink_throughput("FASTLINK framed (RAW codec)   ", &pat);

    println!("\n--- [3] 8 MiB Uniform Compressible Tensor Throughput ---");
    let uni = vec![0xAB; TENSOR_BYTES];
    let mmap_uni_tp = bench_mmap_throughput("mmap shm transfer (uniform)   ", &uni);
    let raw_uni_tp = bench_raw_throughput("Raw TCP streaming (uniform)   ", &uni);
    let (fl_uni_raw, _fl_uni_wire, _, _) =
        bench_fastlink_throughput("FASTLINK framed (RLE codec)   ", &uni);

    println!("\n================================================================================");
    println!("Transfer Mechanism Comparison Matrix:");
    println!("  Method       | 64KB p50  | 64KB p95  | 64KB p99  | 8MB Patterned | 8MB Uniform");
    println!("  -------------+-----------+-----------+-----------+---------------+------------");
    #[cfg(unix)]
    println!(
        "  mmap shm     | {:>6.3} ms | {:>6.3} ms | {:>6.3} ms | {:>9.1} MB/s | {:>9.1} MB/s",
        mmap_lat.p50, mmap_lat.p95, mmap_lat.p99, mmap_pat_tp, mmap_uni_tp
    );
    println!(
        "  Raw TCP      | {:>6.3} ms | {:>6.3} ms | {:>6.3} ms | {:>9.1} MB/s | {:>9.1} MB/s",
        raw_lat.p50, raw_lat.p95, raw_lat.p99, raw_pat_tp, raw_uni_tp
    );
    println!(
        "  FASTLINK-RPC | {:>6.3} ms | {:>6.3} ms | {:>6.3} ms | {:>9.1} MB/s | {:>9.1} MB/s",
        fl_lat.p50, fl_lat.p95, fl_lat.p99, fl_pat_raw, fl_uni_raw
    );
    println!("================================================================================");
}
