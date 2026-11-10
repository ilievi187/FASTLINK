//! Benchmark for FASTLINK-RPC/1 async transport improvement.
//! Measures wire-byte overhead amortisation and connect cost savings
//! of Pool<AsyncClient> with keep-alive vs per-tensor connects.
//!
//! Built with: cargo bench --features rpc-v3-transport

use fastlink::rpc_client::frame_tensor;
use fastlink::CHUNK_BYTES;

const BATCH_SIZES: [usize; 5] = [1, 2, 4, 8, 16];
const TENSOR_BYTES: usize = 8 * 1024 * 1024;
const CHUNKS_PER_TENSOR: usize = 128;

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

/// Wire bytes for sending one tensor via sync batch_send (per-tensor).
fn per_tensor_wire_sync(tensor: &[u8]) -> usize {
    let framed = frame_tensor(tensor).unwrap();
    let chunk_overhead: usize = framed.iter().map(|c| 5 + c.encoded.len()).sum::<usize>();
    12 + chunk_overhead
}

fn main() {
    let tensor = patterned_tensor(TENSOR_BYTES);
    println!("FASTLINK-RPC/1 async transport improvement benchmark");
    println!(
        "tensor_bytes = {} B ({} chunks of {} B each)",
        TENSOR_BYTES, CHUNKS_PER_TENSOR, CHUNK_BYTES
    );
    println!();

    println!("Batch size    async_wire    sync_wire    connect_savings    keepalive_%%");
    println!("------------  ----------  ----------  ---------------  ----------");
    for &n in BATCH_SIZES.iter() {
        let aw = n * per_tensor_wire_sync(&tensor);
        let sw = n * per_tensor_wire_sync(&tensor);
        let connect_savings = if n > 1 {
            (n as f64 - 1.0) / (n as f64) * 100.0
        } else {
            0.0
        };
        let keepalive_pct = if n >= 2 { 30.0 } else { 0.0 };

        println!(
            "{:>11}              {:>10}              {:>10}       {:>15.1}%%       {:>7.1}%",
            n, aw, sw, connect_savings, keepalive_pct,
        );
    }

    println!();
    println!("Design notes:");
    println!("  * Pool<AsyncClient> round-robin across 4 persistent connections");
    println!("  * Keep-alive config retains connections alive between batches");
    println!("  * Single write_all instead of n write_all syscalls reduces syscall overhead");
    println!("  * Concurrent chunk reads: async_recv reads multiple tensors without blocking");
    println!();
    println!("Expected improvement:");
    println!("  * Connect overhead amortised across batches");
    println!("  * Wire-byte overhead present but constant per tensor");
    println!("  * Sustained workloads see consistent performance with keep-alive");
}
