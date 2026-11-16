//! Synthetic framing-overhead arithmetic projection for FASTLINK-RPC/1 batches.
//! Computes in-memory wire-byte sizes with `frame_tensor` and arithmetic
//! connect-count reduction only; does not open sockets, use `Pool` or
//! `AsyncClient`, or measure async I/O, TCP keep-alive, latency, or throughput.
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
    println!("FASTLINK-RPC/1 synthetic batch-framing projection (unmeasured; no sockets opened)");
    println!(
        "tensor_bytes = {} B ({} chunks of {} B each)",
        TENSOR_BYTES, CHUNKS_PER_TENSOR, CHUNK_BYTES
    );
    println!();

    println!("Batch size    batched_wire_B    per_tensor_wire_B    connect_count_reduction");
    println!("------------  --------------    -----------------    -----------------------");
    for &n in BATCH_SIZES.iter() {
        let batched_wire = n * per_tensor_wire_sync(&tensor);
        let single_wire = per_tensor_wire_sync(&tensor);
        let connect_reduction_pct = if n > 1 {
            (n as f64 - 1.0) / (n as f64) * 100.0
        } else {
            0.0
        };

        println!(
            "{:>11}   {:>14}    {:>17}    {:>22.1}%",
            n, batched_wire, single_wire, connect_reduction_pct,
        );
    }

    println!();
    println!("Design notes (in-memory arithmetic only):");
    println!("  * Pool is mutex-serialized around blocking AsyncClient TCP streams (legacy name)");
    println!("  * Single write_all coalesces framed records for a batch, followed by one ACK byte");
    println!("  * Each tensor still retains its 12-byte header and 5-byte per-chunk headers");
    println!("  * Not measured: no TCP sockets opened, no async I/O, and no TCP keep-alive");
}
