use fastlink::digest;
use fastlink::pipeline::{
    decode_activation_frame, encode_activation_frame, summarize_pipeline, ActivationError,
    BoundarySample, TelemetrySource, ACTIVATION_HEADER_BYTES,
};
use fastlink::residency::{ResidencyError, ResidencyTable, StageStateKind, StateHandleId};

#[test]
fn residency_enforces_budget_evicts_unpinned_and_invalidates_stale_epoch() {
    let mut table = ResidencyTable::new(1024, 1);
    let weights_handle = StateHandleId {
        stage_index: 0,
        kind: StageStateKind::Weights,
        slot: 0,
    };
    let kv_old = StateHandleId {
        stage_index: 0,
        kind: StageStateKind::KvCache,
        slot: 1,
    };
    let kv_new = StateHandleId {
        stage_index: 0,
        kind: StageStateKind::KvCache,
        slot: 2,
    };

    let d_w = digest(b"stage0-weights");
    let d_kv1 = digest(b"kv-slot-1");
    let d_kv2 = digest(b"kv-slot-2");

    table
        .register(weights_handle.clone(), 1, 600, d_w, true)
        .expect("pin weights");
    table
        .register(kv_old.clone(), 1, 300, d_kv1, false)
        .expect("kv1");
    assert_eq!(table.used_bytes(), 900);

    // Registering 300B more must evict unpinned kv_old, keeping pinned weights.
    table
        .register(kv_new.clone(), 1, 300, d_kv2, false)
        .expect("kv2");
    assert_eq!(table.used_bytes(), 900);
    assert!(matches!(
        table.verify_resident(&kv_old, &d_kv1),
        Err(ResidencyError::MissingHandle)
    ));
    table
        .verify_resident(&weights_handle, &d_w)
        .expect("weights still resident");

    // Epoch bump invalidates all epoch-1 entries.
    let purged = table.advance_epoch(2);
    assert_eq!(purged, 2);
    assert_eq!(table.used_bytes(), 0);
}

#[test]
fn activation_frame_roundtrip_and_malformed_rejection() {
    let raw = vec![0xABu8; 4096];
    let frame = encode_activation_frame(2, 99, 12, 0, 1, &raw).expect("encode");
    assert!(frame.len() >= ACTIVATION_HEADER_BYTES);

    let (decoded_hdr, decoded_raw) = decode_activation_frame(&frame, 2).expect("decode");
    assert_eq!(decoded_hdr.epoch, 2);
    assert_eq!(decoded_hdr.seq_id, 99);
    assert_eq!(decoded_hdr.token_pos, 12);
    assert_eq!(decoded_raw, raw);

    // Stale epoch rejected.
    assert!(matches!(
        decode_activation_frame(&frame, 3),
        Err(ActivationError::StaleEpoch {
            frame_epoch: 2,
            active_epoch: 3
        })
    ));

    // Corrupted payload rejected via BLAKE3 digest.
    let mut corrupt = frame.clone();
    let last = corrupt.len() - 1;
    corrupt[last] ^= 0xFF;
    assert_eq!(
        decode_activation_frame(&corrupt, 2),
        Err(ActivationError::DigestMismatch)
    );
}

#[test]
fn pipeline_telemetry_computes_percentiles_and_byte_accounting() {
    let samples = vec![
        BoundarySample {
            boundary_index: 0,
            raw_bytes: 8192,
            wire_bytes: 8261,
            link_rtt_us: 120,
            stage_compute_us: 1800,
        },
        BoundarySample {
            boundary_index: 1,
            raw_bytes: 8192,
            wire_bytes: 8261,
            link_rtt_us: 240,
            stage_compute_us: 2200,
        },
    ];
    let summary = summarize_pipeline(TelemetrySource::Simulated, &samples, 1);
    assert_eq!(summary.source, TelemetrySource::Simulated);
    assert_eq!(summary.total_raw_bytes, 16384);
    assert_eq!(summary.total_wire_bytes, 16522);
    assert!(summary.mean_decode_tok_per_sec > 0.0);
}
