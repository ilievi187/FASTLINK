//! FASTLINK CPU-only proof of concept: content-addressed, lossless delta sync.
//! This does NOT yet accelerate llama.cpp RPC or GPU execution.
use fastlink::{
    codec_name, select_v4_adaptive_plan, send_file_resumable, serve, synthetic_dense_numeric_bytes,
    synthetic_incompressible_bytes, synthetic_sparse_bytes, synthetic_structured_bytes,
    v3_always_send_wire_bytes, CodecCostPolicy, PersistentTensorRegistry, RemoteWorkerStore,
    SendOptions, SyncDecision, V4Cluster,
};
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

fn run_v4_bench(json_mode: bool) -> io::Result<()> {
    let policy = CodecCostPolicy::default();
    let mut reg = PersistentTensorRegistry::new(policy);
    let mut worker = RemoteWorkerStore::new("worker-0");

    // 1. Residency benchmark: 4 representative 1 MiB tensors synced once (cold) + 9 times (warm REUSE)
    let incomp_1m = synthetic_incompressible_bytes(1024 * 1024);
    let sparse_1m = synthetic_sparse_bytes(1024 * 1024);
    let struct_1m = synthetic_structured_bytes(1024 * 1024);
    let numeric_1m = synthetic_dense_numeric_bytes(1024 * 1024);

    let plan_incomp = select_v4_adaptive_plan(&incomp_1m, policy);
    let plan_sparse = select_v4_adaptive_plan(&sparse_1m, policy);
    let plan_struct = select_v4_adaptive_plan(&struct_1m, policy);
    let plan_numeric = select_v4_adaptive_plan(&numeric_1m, policy);

    let cold_decisions = reg.sync_tensors_batched(
        "bench-session",
        &[
            ("blk.0.attn_q.weight", &incomp_1m),
            ("blk.0.attn_mask", &sparse_1m),
            ("blk.0.ssm_conv1d", &struct_1m),
            ("blk.0.ffn_gate", &numeric_1m),
        ],
        &mut worker,
        1,
    )?;
    let cold_wire_bytes: u64 = cold_decisions.iter().map(|d| d.wire_bytes()).sum();

    let mut warm_wire_bytes = 0u64;
    for _ in 0..9 {
        let warm = reg.sync_tensors_batched(
            "bench-session",
            &[
                ("blk.0.attn_q.weight", &incomp_1m),
                ("blk.0.attn_mask", &sparse_1m),
                ("blk.0.ssm_conv1d", &struct_1m),
                ("blk.0.ffn_gate", &numeric_1m),
            ],
            &mut worker,
            1,
        )?;
        warm_wire_bytes += warm.iter().map(|d| d.wire_bytes()).sum::<u64>();
    }

    // 2. Delta benchmark: 6 chunks of 16 KiB (96 KiB), mutate 1 chunk of 6
    let mut delta_tensor = synthetic_dense_numeric_bytes(6 * 16 * 1024);
    let d_delta_cold = reg.sync_tensor(
        "bench-session",
        "blk.1.ssm_state",
        &delta_tensor,
        &mut worker,
    )?;
    let delta_full_wire = d_delta_cold.wire_bytes();
    for (i, b) in delta_tensor[2 * 16 * 1024..3 * 16 * 1024]
        .iter_mut()
        .enumerate()
    {
        *b ^= ((i / 2) % 3) as u8 ^ 0x04;
    }
    let d_delta_step = reg.sync_tensor(
        "bench-session",
        "blk.1.ssm_state",
        &delta_tensor,
        &mut worker,
    )?;
    let (delta_wire, delta_reused, delta_transferred) = match &d_delta_step {
        SyncDecision::Delta {
            wire_bytes,
            reused_chunks,
            changed_chunks,
            ..
        } => (*wire_bytes, *reused_chunks, changed_chunks.len() as u32),
        _ => (d_delta_step.wire_bytes(), 0, 0),
    };

    // 3. Worker restart benchmark
    let cluster = V4Cluster::new(policy);
    let _ = cluster.sync_tensor("s1", "worker-a", "t.a", &struct_1m)?;
    let _ = cluster.sync_tensor("s1", "worker-b", "t.b", &numeric_1m)?;
    let invalidated_on_restart = cluster.restart_worker("worker-a");
    let after_a = cluster.sync_tensor("s1", "worker-a", "t.a", &struct_1m)?;
    let after_b = cluster.sync_tensor("s1", "worker-b", "t.b", &numeric_1m)?;

    let acc = reg.accounting();
    let v3_single_pass_4m = v3_always_send_wire_bytes("blk.0.attn_q.weight", incomp_1m.len())
        + v3_always_send_wire_bytes("blk.0.attn_mask", sparse_1m.len())
        + v3_always_send_wire_bytes("blk.0.ssm_conv1d", struct_1m.len())
        + v3_always_send_wire_bytes("blk.0.ffn_gate", numeric_1m.len());

    if json_mode {
        println!(
            concat!(
                "{{",
                "\"cold_wire_bytes\":{},",
                "\"warm_9_passes_wire_bytes\":{},",
                "\"v3_single_pass_4m_wire_bytes\":{},",
                "\"v3_ten_passes_wire_bytes\":{},",
                "\"v4_total_wire_bytes_sent\":{},",
                "\"reuse_bytes_saved\":{},",
                "\"bytes_per_token\":{:.3},",
                "\"reused_chunks\":{},",
                "\"transferred_chunks\":{},",
                "\"sync_events\":{},",
                "\"reuse_events\":{},",
                "\"full_events\":{},",
                "\"delta_events\":{},",
                "\"batches_sent\":{},",
                "\"encode_ms\":{:.4},",
                "\"decode_ms\":{:.4},",
                "\"delta_full_wire_bytes\":{},",
                "\"delta_1_of_6_wire_bytes\":{},",
                "\"delta_reused_chunks\":{},",
                "\"delta_transferred_chunks\":{},",
                "\"restart_invalidated\":{},",
                "\"restart_worker_a_resync_full\":{},",
                "\"restart_worker_b_kept_reuse\":{},",
                "\"plans\":{{",
                "\"incompressible\":{{\"codec\":\"{}\",\"chunk_bytes\":{}}},",
                "\"sparse\":{{\"codec\":\"{}\",\"chunk_bytes\":{}}},",
                "\"structured\":{{\"codec\":\"{}\",\"chunk_bytes\":{}}},",
                "\"dense_numeric\":{{\"codec\":\"{}\",\"chunk_bytes\":{}}}",
                "}}",
                "}}"
            ),
            cold_wire_bytes,
            warm_wire_bytes,
            v3_single_pass_4m,
            acc.v3_always_send_wire_bytes,
            acc.wire_bytes_sent,
            acc.reuse_bytes_saved,
            acc.bytes_per_token,
            acc.reused_chunks,
            acc.transferred_chunks,
            acc.sync_events,
            acc.reuse_events,
            acc.full_events,
            acc.delta_events,
            acc.batches_sent,
            acc.encode_ms,
            acc.decode_ms,
            delta_full_wire,
            delta_wire,
            delta_reused,
            delta_transferred,
            invalidated_on_restart,
            after_a.is_full(),
            after_b.is_reuse(),
            codec_name(plan_incomp.codec),
            plan_incomp.chunk_bytes,
            codec_name(plan_sparse.codec),
            plan_sparse.chunk_bytes,
            codec_name(plan_struct.codec),
            plan_struct.chunk_bytes,
            codec_name(plan_numeric.codec),
            plan_numeric.chunk_bytes,
        );
    } else {
        println!("FASTLINK v4 residency + adaptive transfer benchmark:");
        println!(
            "  v3_always_send_wire_bytes: {}",
            acc.v3_always_send_wire_bytes
        );
        println!("  v4_wire_bytes_sent: {}", acc.wire_bytes_sent);
        println!("  reuse_bytes_saved: {}", acc.reuse_bytes_saved);
        println!("  warm_9_passes_wire_bytes: {}", warm_wire_bytes);
        println!("  bytes_per_token: {:.3}", acc.bytes_per_token);
        println!("  reused_chunks: {}", acc.reused_chunks);
        println!("  transferred_chunks: {}", acc.transferred_chunks);
        println!("  sync_events: {}", acc.sync_events);
        println!("  encode_ms: {:.4}", acc.encode_ms);
        println!("  decode_ms: {:.4}", acc.decode_ms);
    }
    Ok(())
}

fn usage() {
    eprintln!("FASTLINK (CPU proof of concept)\n  fastlink serve [127.0.0.1:18777] [./.fastlink-data]\n  fastlink send FILE [127.0.0.1:18777]\n  fastlink v4-bench [--json]\nNo remote GPU scheduling yet.");
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
        Some("v4-bench") => {
            let json_mode = args.iter().any(|a| a == "--json");
            run_v4_bench(json_mode)
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
