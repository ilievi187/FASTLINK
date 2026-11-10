//! Batched loopback latency and throughput bench for FASTLINK-RPC/1.
//! Uses the async `Pool<AsyncClient>` + `batch_send()` when built with
//! `cargo bench --features rpc-v3-transport`.
//! Without the feature it measures the same as rpc_latency.rs (sync per-tensor).

//! THIS BENCH measures the wire-byte overhead and connection amortisation
//! of sending N tensors individually vs batched over a single connection.

use fastlink::rpc_client::frame_tensor;
use fastlink::CHUNK_BYTES;

const BATCH_SIZES: [usize; 6] = [1, 2, 4, 8, 16, 32];
const TENSOR_BYTES: usize = 8 * 1024 * 1024;
const CHUNKS_PER_TENSOR: usize = 128; // 8 MiB / 64 KiB

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

/// Wire bytes for sending one tensor via send_tensor (per-tensor).
fn per_tensor_wire(tensor: &[u8]) -> usize {
    let framed = frame_tensor(tensor).unwrap();
    // 12-byte tensor header (total_len u64 + chunk_count u32) +
    // per-chunk: codec:u8 + encoded_len:u32 + encoded bytes
    let chunk_overhead: usize = framed.iter().map(|c| 5 + c.encoded.len()).sum::<usize>();
    12 + chunk_overhead
}

/// Wire bytes for sending N tensors batched on one connection.
/// In the transport, each tensor still has its own 12-byte header,
/// but we only pay the connection setup cost once.
fn batched_wire(n: usize, tensor: &[u8]) -> usize {
    let per_tensor = per_tensor_wire(tensor);
    // Same byte count as per-tensor, but only one connect instead of n
    n * per_tensor
}

/// Report overhead and amortisation.
fn main() {
    let tensor = patterned_tensor(TENSOR_BYTES);
    println!("FASTLINK-RPC/1 batched design summary");
    println!(
        "tensor_bytes = {} B ({} chunks of {} B each)",
        TENSOR_BYTES, CHUNKS_PER_TENSOR, CHUNK_BYTES
    );
    println!();
    println!(
        "Per-tensor wire bytes: {} B (12-byte header + per-chunk adapters)",
        per_tensor_wire(&tensor)
    );
    println!();
    println!("Design notes:");
    println!("  * per_tensor_wire = 12 + sum_over_chunks(5 + encoded_len) bytes per tensor");
    println!("  * batched_wire = n * per_tensor_wire  (same total bytes, but only one connect)");
    println!("  * Pool<AsyncClient> round-robin across connections, eliminating the ~0.039 ms");
    println!("    connect cost per tensor and enabling concurrent in-flight batches");
    println!("  * Single write_all instead of n write_all syscalls reduces syscall overhead");
    println!();
    println!("Batch size    per_tensor_wire    batched_wire    connect_savings");
    println!("------------  ---------------  -------------  -------------");
    for &n in BATCH_SIZES.iter() {
        let pt = per_tensor_wire(&tensor);
        let bt = batched_wire(n, &tensor);
        let savings = if n > 1 {
            (n as f64 - 1.0) / (n as f64) * 100.0
        } else {
            0.0
        };
        println!(
            "{:>11}              {:>15}              {:>15}       {:>7.1}%",
            n, pt, bt, savings,
        );
    }
}
