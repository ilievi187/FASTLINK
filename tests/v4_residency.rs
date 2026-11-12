use fastlink::{
    digest, select_v4_adaptive_plan, send_batch_tcp, sync_decision, synthetic_dense_numeric_bytes,
    synthetic_incompressible_bytes, synthetic_sparse_bytes, synthetic_structured_bytes,
    v3_always_send_wire_bytes, BatchedSyncFrame, CodecCostPolicy, PersistentTensorRegistry,
    RemoteWorkerStore, SyncDecision, V4Cluster, V4LoopbackServer, WorkloadClass, LZ4, RAW, RLE,
    ZSTD,
};
use std::sync::{Arc, Barrier};
use std::thread;

#[test]
fn v4_residency_sync_twice_second_is_zero_bytes_reuse_and_byte_exact() {
    let mut registry = PersistentTensorRegistry::default();
    let mut worker = RemoteWorkerStore::new("worker-detti-0");

    // 1 MiB weight tensor (e.g. blk.0.attn_q.weight)
    let weight_bytes = synthetic_incompressible_bytes(1024 * 1024);
    let expected_hash = digest(&weight_bytes);

    // 1st sync (cold): must transfer full tensor
    let d1 = registry
        .sync_tensor(
            "session-main",
            "blk.0.attn_q.weight",
            &weight_bytes,
            &mut worker,
        )
        .unwrap();
    assert!(d1.is_full(), "first sync must be FULL");
    assert_eq!(d1.version(), 1);
    assert_eq!(d1.content_hash(), expected_hash);
    assert!(d1.wire_bytes() > 1024 * 1024);

    let resident_1 = worker
        .get_tensor_bytes("session-main", "blk.0.attn_q.weight")
        .expect("tensor must be resident on worker");
    assert_eq!(digest(resident_1), expected_hash);
    assert_eq!(resident_1, weight_bytes.as_slice());

    let wire_after_first = registry.accounting().wire_bytes_sent;

    // 2nd sync (warm): must return REUSE with 0 wire bytes transferred!
    let d2 = registry
        .sync_tensor(
            "session-main",
            "blk.0.attn_q.weight",
            &weight_bytes,
            &mut worker,
        )
        .unwrap();
    assert!(
        d2.is_reuse(),
        "second sync of unchanged tensor must be REUSE"
    );
    assert_eq!(d2.wire_bytes(), 0, "REUSE must transfer 0 wire bytes");
    assert_eq!(d2.version(), 1);
    match d2 {
        SyncDecision::Reuse {
            total_chunks,
            bytes_avoided,
            ..
        } => {
            assert!(total_chunks >= 4);
            assert_eq!(
                bytes_avoided,
                v3_always_send_wire_bytes("blk.0.attn_q.weight", weight_bytes.len())
            );
        }
        _ => panic!("expected SyncDecision::Reuse"),
    }

    assert_eq!(
        registry.accounting().wire_bytes_sent,
        wire_after_first,
        "warm REUSE sync must add 0 bytes to wire_bytes_sent"
    );
    assert_eq!(registry.accounting().reuse_events, 1);

    // Verify resident tensor remains byte-exact
    let resident_2 = worker
        .get_tensor_bytes("session-main", "blk.0.attn_q.weight")
        .unwrap();
    assert_eq!(digest(resident_2), expected_hash);
    assert_eq!(resident_2, weight_bytes.as_slice());
    assert_eq!(worker.verify_integrity().unwrap(), 1);
}

#[test]
fn v4_invalidation_and_memory_reuse_bump_version_and_resync_byte_exact() {
    let mut registry = PersistentTensorRegistry::default();
    let mut worker = RemoteWorkerStore::new("worker-0");

    let orig = synthetic_structured_bytes(64 * 1024);
    let d1 = registry
        .sync_tensor("sess-1", "blk.3.ssm_out", &orig, &mut worker)
        .unwrap();
    assert!(d1.is_full());
    assert_eq!(d1.version(), 1);
    assert_eq!(
        worker.get_tensor_bytes("sess-1", "blk.3.ssm_out").unwrap(),
        orig.as_slice()
    );

    // Case A: Explicit invalidation + content mutation -> version bumped, FULL resync
    assert!(registry.invalidate_tensor("sess-1", "blk.3.ssm_out"));
    let mutated_a: Vec<u8> = orig.iter().map(|b| b.wrapping_add(17)).collect();
    let d2 = registry
        .sync_tensor("sess-1", "blk.3.ssm_out", &mutated_a, &mut worker)
        .unwrap();
    assert!(
        d2.is_full(),
        "invalidated tensor with all-new content must FULL resync"
    );
    assert!(
        d2.version() >= 2,
        "version must bump on invalidation + resync"
    );
    assert_eq!(
        worker.get_tensor_bytes("sess-1", "blk.3.ssm_out").unwrap(),
        mutated_a.as_slice()
    );

    // Case B: Memory reuse (same tensor_id, new buffer contents without prior explicit invalidate)
    // Must re-validate by BLAKE3 hash, detect mismatch, bump version, and FULL resync!
    let prev_ver = d2.version();
    let reused_mem_new_data: Vec<u8> = mutated_a
        .iter()
        .enumerate()
        .map(|(i, b)| b.wrapping_mul(3).wrapping_add((i % 251) as u8))
        .collect();
    let d3 = registry
        .sync_tensor("sess-1", "blk.3.ssm_out", &reused_mem_new_data, &mut worker)
        .unwrap();
    assert!(
        d3.is_full(),
        "memory reuse with completely new content must never stale-REUSE"
    );
    assert_eq!(d3.version(), prev_ver + 1);
    assert_eq!(
        worker.get_tensor_bytes("sess-1", "blk.3.ssm_out").unwrap(),
        reused_mem_new_data.as_slice()
    );
    assert_eq!(worker.verify_integrity().unwrap(), 1);
}

#[test]
fn v4_delta_encoding_one_chunk_of_six_sends_one_sixth_bytes_and_reconstructs_byte_exact() {
    let mut registry = PersistentTensorRegistry::default();
    let mut worker = RemoteWorkerStore::new("worker-0");

    // 6 chunks of 16 KiB = 96 KiB dense numeric tensor (16 KiB chunk plan)
    let chunk_size = 16 * 1024;
    let total_chunks = 6usize;
    let mut tensor = synthetic_dense_numeric_bytes(chunk_size * total_chunks);

    // Initial sync: FULL (6 chunks)
    let d_full = registry
        .sync_tensor("sess-delta", "blk.1.ssm_state", &tensor, &mut worker)
        .unwrap();
    assert!(d_full.is_full());
    let full_wire = d_full.wire_bytes();
    assert_eq!(d_full.version(), 1);

    // Mutate ONLY chunk 2 (1 chunk out of 6) with incompressible numeric delta so size ratio is exact
    let mutated_chunk = synthetic_dense_numeric_bytes(chunk_size)
        .into_iter()
        .enumerate()
        .map(|(i, b)| b ^ (((i / 2) % 3) as u8))
        .collect::<Vec<u8>>();
    tensor[2 * chunk_size..3 * chunk_size].copy_from_slice(&mutated_chunk);

    // Sync mutated tensor: must choose DELTA, reusing 5 chunks and transferring 1 chunk
    let d_delta = registry
        .sync_tensor("sess-delta", "blk.1.ssm_state", &tensor, &mut worker)
        .unwrap();
    assert!(
        d_delta.is_delta(),
        "changing 1 chunk of 6 must select DELTA"
    );
    match &d_delta {
        SyncDecision::Delta {
            version,
            total_chunks: tot,
            reused_chunks,
            changed_chunks,
            wire_bytes,
            full_wire_bytes,
            total_cost_ms,
            full_cost_ms,
            ..
        } => {
            assert_eq!(*version, 2);
            assert_eq!(*tot, 6);
            assert_eq!(*reused_chunks, 5);
            assert_eq!(changed_chunks.len(), 1);
            assert_eq!(changed_chunks[0].chunk_index, 2);
            assert!(
                *wire_bytes < *full_wire_bytes / 3,
                "delta wire_bytes ({wire_bytes}) must be ~1/6 of full_wire_bytes ({full_wire_bytes})"
            );
            assert!(*total_cost_ms < *full_cost_ms);
        }
        _ => panic!("expected SyncDecision::Delta"),
    }
    assert!(d_delta.wire_bytes() < full_wire / 3);

    // Verify byte-exact reconstruction on worker
    let reconstructed = worker
        .get_tensor_bytes("sess-delta", "blk.1.ssm_state")
        .unwrap();
    assert_eq!(digest(reconstructed), digest(&tensor));
    assert_eq!(reconstructed, tensor.as_slice());

    // Subsequent sync without changes -> REUSE (0 wire bytes)
    let d_reuse = registry
        .sync_tensor("sess-delta", "blk.1.ssm_state", &tensor, &mut worker)
        .unwrap();
    assert!(d_reuse.is_reuse());
    assert_eq!(d_reuse.wire_bytes(), 0);
}

#[test]
fn v4_worker_restart_resyncs_only_restarted_worker_tensors_keeping_others_resident() {
    let cluster = V4Cluster::default();

    let t_w0_a = synthetic_incompressible_bytes(64 * 1024);
    let t_w0_b = synthetic_sparse_bytes(64 * 1024);
    let t_w1_a = synthetic_structured_bytes(64 * 1024);
    let t_w1_b = synthetic_dense_numeric_bytes(64 * 1024);

    // Initial sync across worker-0 and worker-1
    assert!(cluster
        .sync_tensor("sess-ha", "worker-0", "w0.weight.a", &t_w0_a)
        .unwrap()
        .is_full());
    assert!(cluster
        .sync_tensor("sess-ha", "worker-0", "w0.weight.b", &t_w0_b)
        .unwrap()
        .is_full());
    assert!(cluster
        .sync_tensor("sess-ha", "worker-1", "w1.weight.a", &t_w1_a)
        .unwrap()
        .is_full());
    assert!(cluster
        .sync_tensor("sess-ha", "worker-1", "w1.weight.b", &t_w1_b)
        .unwrap()
        .is_full());

    // Warm check before restart: all 4 reuse with 0 bytes
    assert!(cluster
        .sync_tensor("sess-ha", "worker-0", "w0.weight.a", &t_w0_a)
        .unwrap()
        .is_reuse());
    assert!(cluster
        .sync_tensor("sess-ha", "worker-1", "w1.weight.a", &t_w1_a)
        .unwrap()
        .is_reuse());

    // Kill and restart ONLY worker-0
    let invalidated = cluster.restart_worker("worker-0");
    assert_eq!(
        invalidated, 2,
        "only worker-0's 2 tensors should be invalidated"
    );

    // Reconnect & sync all 4 tensors:
    // - worker-1's tensors MUST remain resident (REUSE, 0 wire bytes)
    let r_w1_a = cluster
        .sync_tensor("sess-ha", "worker-1", "w1.weight.a", &t_w1_a)
        .unwrap();
    let r_w1_b = cluster
        .sync_tensor("sess-ha", "worker-1", "w1.weight.b", &t_w1_b)
        .unwrap();
    assert!(r_w1_a.is_reuse(), "worker-1 tensor A must remain REUSE");
    assert!(r_w1_b.is_reuse(), "worker-1 tensor B must remain REUSE");
    assert_eq!(r_w1_a.wire_bytes(), 0);
    assert_eq!(r_w1_b.wire_bytes(), 0);

    // - worker-0's tensors MUST resync (FULL) and be restored byte-exact
    let r_w0_a = cluster
        .sync_tensor("sess-ha", "worker-0", "w0.weight.a", &t_w0_a)
        .unwrap();
    let r_w0_b = cluster
        .sync_tensor("sess-ha", "worker-0", "w0.weight.b", &t_w0_b)
        .unwrap();
    assert!(
        r_w0_a.is_full(),
        "restarted worker-0 tensor A must FULL resync"
    );
    assert!(
        r_w0_b.is_full(),
        "restarted worker-0 tensor B must FULL resync"
    );

    assert_eq!(
        cluster
            .read_worker_tensor("worker-0", "sess-ha", "w0.weight.a")
            .unwrap(),
        t_w0_a
    );
    assert_eq!(
        cluster
            .read_worker_tensor("worker-0", "sess-ha", "w0.weight.b")
            .unwrap(),
        t_w0_b
    );
    assert_eq!(
        cluster
            .read_worker_tensor("worker-1", "sess-ha", "w1.weight.a")
            .unwrap(),
        t_w1_a
    );
    assert_eq!(
        cluster
            .read_worker_tensor("worker-1", "sess-ha", "w1.weight.b")
            .unwrap(),
        t_w1_b
    );
    assert_eq!(cluster.verify_all_workers_integrity().unwrap(), 4);
}

#[test]
fn v4_four_concurrent_sessions_isolated_overlapping_and_unique_tensors_zero_corruption() {
    let cluster = Arc::new(V4Cluster::default());
    let shared_weight = Arc::new(synthetic_dense_numeric_bytes(64 * 1024));

    let barrier = Arc::new(Barrier::new(4));
    let mut handles = Vec::new();

    for sid in 0..4usize {
        let c = Arc::clone(&cluster);
        let shared = Arc::clone(&shared_weight);
        let b = Arc::clone(&barrier);

        handles.push(thread::spawn(move || {
            let session_id = format!("session-{sid}");
            // Unique per-session tensor (even with an overlapping tensor_id name!)
            let mut same_name_per_session = synthetic_structured_bytes(64 * 1024);
            for byte in &mut same_name_per_session[0..1024] {
                *byte = byte.wrapping_add((sid as u8 + 1) * 29);
            }
            let unique_id = format!("unique.tensor.{sid}");
            let unique_data: Vec<u8> = synthetic_incompressible_bytes(64 * 1024)
                .into_iter()
                .map(|x| x ^ (sid as u8 * 41))
                .collect();

            b.wait();

            // 1. Batch sync shared + overlapping-name + unique tensors
            let decisions = c
                .sync_batch(
                    &session_id,
                    "worker-shared",
                    &[
                        ("model.embed.weight", shared.as_slice()),
                        ("session.ssm.state", same_name_per_session.as_slice()),
                        (&unique_id, unique_data.as_slice()),
                    ],
                    16,
                )
                .unwrap();
            assert_eq!(decisions.len(), 3);
            assert!(decisions.iter().all(|d| d.is_full()));

            // 2. Warm sync -> all 3 must be REUSE (0 wire bytes)
            let warm = c
                .sync_batch(
                    &session_id,
                    "worker-shared",
                    &[
                        ("model.embed.weight", shared.as_slice()),
                        ("session.ssm.state", same_name_per_session.as_slice()),
                        (&unique_id, unique_data.as_slice()),
                    ],
                    16,
                )
                .unwrap();
            assert!(warm.iter().all(|d| d.is_reuse()));

            // 3. Session 0 invalidates its "session.ssm.state"; must NOT affect sessions 1..=3!
            if sid == 0 {
                assert!(c.invalidate_tensor(&session_id, "session.ssm.state"));
                let resync = c
                    .sync_tensor(
                        &session_id,
                        "worker-shared",
                        "session.ssm.state",
                        &same_name_per_session,
                    )
                    .unwrap();
                assert!(resync.is_full());
            } else {
                let still_reuse = c
                    .sync_tensor(
                        &session_id,
                        "worker-shared",
                        "session.ssm.state",
                        &same_name_per_session,
                    )
                    .unwrap();
                assert!(
                    still_reuse.is_reuse(),
                    "session 0 invalidation must not invalidate session {sid}"
                );
            }

            (session_id, unique_id, same_name_per_session, unique_data)
        }));
    }

    for h in handles {
        let (session_id, unique_id, expected_ssm, expected_unique) = h.join().unwrap();
        let actual_shared = cluster
            .read_worker_tensor("worker-shared", &session_id, "model.embed.weight")
            .unwrap();
        let actual_ssm = cluster
            .read_worker_tensor("worker-shared", &session_id, "session.ssm.state")
            .unwrap();
        let actual_unique = cluster
            .read_worker_tensor("worker-shared", &session_id, &unique_id)
            .unwrap();

        assert_eq!(digest(&actual_shared), digest(&shared_weight));
        assert_eq!(digest(&actual_ssm), digest(&expected_ssm));
        assert_eq!(digest(&actual_unique), digest(&expected_unique));
        assert_eq!(actual_shared, *shared_weight);
        assert_eq!(actual_ssm, expected_ssm);
        assert_eq!(actual_unique, expected_unique);
    }

    // 4 sessions * 3 tensors each = 12 verified resident tensors, 0 corruption
    assert_eq!(cluster.verify_all_workers_integrity().unwrap(), 12);

    let acc = cluster.accounting();
    assert!(acc.reuse_events >= 15);
    assert!(acc.reuse_bytes_saved > 0);
    assert!(acc.bytes_per_token > 0.0);
    assert!(acc.encode_ms > 0.0);
    assert!(acc.decode_ms > 0.0);
}

#[test]
fn v4_adaptive_codec_and_chunk_selection_matches_measured_policy_per_workload() {
    let policy = CodecCostPolicy::default();

    // 1. incompressible-random -> RAW (64K-256K chunks; framing dominates, big chunks minimize overhead)
    let incomp_256k = synthetic_incompressible_bytes(512 * 1024);
    let plan_incomp = select_v4_adaptive_plan(&incomp_256k, policy);
    assert_eq!(
        plan_incomp.workload_class,
        WorkloadClass::IncompressibleRandom
    );
    assert_eq!(plan_incomp.codec, RAW);
    assert!(
        (64 * 1024..=256 * 1024).contains(&plan_incomp.chunk_bytes),
        "incompressible chunk_bytes ({}) must be in 64K..=256K",
        plan_incomp.chunk_bytes
    );
    assert_eq!(plan_incomp.chunk_bytes, 256 * 1024);

    // 2. sparse/RLE-friendly -> RLE
    let sparse = synthetic_sparse_bytes(128 * 1024);
    let plan_sparse = select_v4_adaptive_plan(&sparse, policy);
    assert_eq!(plan_sparse.workload_class, WorkloadClass::SparseRle);
    assert_eq!(plan_sparse.codec, RLE);
    assert_eq!(plan_sparse.chunk_bytes, 64 * 1024);
    assert!(plan_sparse.total_cost_ms < plan_sparse.raw_total_cost_ms);

    // 3. structured -> LZ4 (16K chunks)
    let structured = synthetic_structured_bytes(128 * 1024);
    let plan_struct = select_v4_adaptive_plan(&structured, policy);
    assert_eq!(plan_struct.workload_class, WorkloadClass::Structured);
    assert_eq!(plan_struct.codec, LZ4);
    assert_eq!(plan_struct.chunk_bytes, 16 * 1024);
    assert!(plan_struct.total_cost_ms < plan_struct.raw_total_cost_ms);

    // 4. dense numeric tensor -> ZSTD (4K-16K chunks)
    let numeric_16k = synthetic_dense_numeric_bytes(128 * 1024);
    let plan_num_16k = select_v4_adaptive_plan(&numeric_16k, policy);
    assert_eq!(
        plan_num_16k.workload_class,
        WorkloadClass::DenseNumericTensor
    );
    assert_eq!(plan_num_16k.codec, ZSTD);
    assert_eq!(plan_num_16k.chunk_bytes, 16 * 1024);
    assert!(plan_num_16k.total_cost_ms < plan_num_16k.raw_total_cost_ms);

    let numeric_small = synthetic_dense_numeric_bytes(16 * 1024);
    let plan_num_4k = select_v4_adaptive_plan(&numeric_small, policy);
    assert_eq!(
        plan_num_4k.workload_class,
        WorkloadClass::DenseNumericTensor
    );
    assert_eq!(plan_num_4k.codec, ZSTD);
    assert_eq!(plan_num_4k.chunk_bytes, 4 * 1024);
    assert!(plan_num_4k.total_cost_ms < plan_num_4k.raw_total_cost_ms);
}

#[test]
fn v4_coalesced_batch_frame_sends_multiple_updates_in_one_tcp_round_trip() {
    let server = V4LoopbackServer::spawn("127.0.0.1:0", "worker-tcp-0").unwrap();
    let addr = server.addr_string();
    let policy = CodecCostPolicy::default();

    let t1 = synthetic_sparse_bytes(64 * 1024);
    let t2 = synthetic_structured_bytes(32 * 1024);
    let t3 = synthetic_dense_numeric_bytes(32 * 1024);

    let d1 = sync_decision("blk.0.norm", &t1, None, policy);
    let d2 = sync_decision("blk.0.ssm_a", &t2, None, policy);
    let d3 = sync_decision("blk.0.attn_k", &t3, None, policy);

    let mut batch = BatchedSyncFrame::new(1);
    batch.push_decision("sess-tcp", "worker-tcp-0", t1.len(), &d1);
    batch.push_decision("sess-tcp", "worker-tcp-0", t2.len(), &d2);
    batch.push_decision("sess-tcp", "worker-tcp-0", t3.len(), &d3);

    assert_eq!(batch.ops.len(), 3);
    assert!(batch.total_chunk_updates() >= 5);

    // Send all 3 tensors (>5 chunks) in a SINGLE TCP batch round-trip!
    let (wire_bytes, applied) = send_batch_tcp(&addr, &batch).unwrap();
    assert_eq!(applied, 3);
    assert!(wire_bytes > 0);
    assert_eq!(
        server.batches_received(),
        1,
        "all chunk updates must be coalesced into 1 network batch round-trip"
    );

    {
        let store = server.store().lock().unwrap();
        assert_eq!(
            store.get_tensor_bytes("sess-tcp", "blk.0.norm").unwrap(),
            t1.as_slice()
        );
        assert_eq!(
            store.get_tensor_bytes("sess-tcp", "blk.0.ssm_a").unwrap(),
            t2.as_slice()
        );
        assert_eq!(
            store.get_tensor_bytes("sess-tcp", "blk.0.attn_k").unwrap(),
            t3.as_slice()
        );
        assert_eq!(store.verify_integrity().unwrap(), 3);
    }

    server.shutdown();
}
