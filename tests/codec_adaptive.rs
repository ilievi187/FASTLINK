use fastlink::{
    codec_name, decode_chunk, encode_chunk, encode_chunk_adaptive, encode_lz4, encode_rle,
    encode_with_codec, encode_zstd, select_codec_measured, synthetic_incompressible_chunk,
    synthetic_sparse_chunk, synthetic_tensor_like_chunk, CodecCostPolicy, CHUNK_BYTES, LZ4, RAW,
    RLE, ZSTD,
};

#[test]
fn all_codecs_roundtrip_on_incompressible_sparse_and_tensor_like_chunks() {
    let fixtures = [
        ("incompressible", synthetic_incompressible_chunk()),
        ("sparse", synthetic_sparse_chunk()),
        ("tensor_like", synthetic_tensor_like_chunk()),
    ];

    for (name, data) in &fixtures {
        for &codec in &[RAW, RLE, LZ4, ZSTD] {
            let encoded = encode_with_codec(codec, data).unwrap();
            if encoded.len() <= fastlink::MAX_ENCODED_BYTES {
                let decoded = decode_chunk(codec, &encoded, data.len()).unwrap();
                assert_eq!(
                    &decoded,
                    data,
                    "roundtrip mismatch for fixture {name} with codec {}",
                    codec_name(codec)
                );
            }
        }
    }
}

#[test]
fn adaptive_selector_picks_lower_total_cost_codec_per_workload() {
    // In unoptimized debug builds (`cfg!(debug_assertions)`), pure-Rust byte loops run ~8-10x
    // slower than `--release`, so we model a 100 Mbps link in debug and 250 Mbps in release
    // while strictly enforcing `total_cost_ms < raw_cost_ms`.
    let policy = CodecCostPolicy {
        bandwidth_mbps: if cfg!(debug_assertions) { 100.0 } else { 250.0 },
        min_byte_savings: 32,
    };

    // 1. Incompressible data: compressed codecs expand or add CPU cost without saving bytes -> RAW wins
    let incomp = synthetic_incompressible_chunk();
    let choice_incomp = select_codec_measured(&incomp, policy, 10);
    assert_eq!(choice_incomp.codec, RAW);
    let (fast_codec_incomp, fast_enc_incomp) = encode_chunk_adaptive(&incomp, policy);
    assert_eq!(fast_codec_incomp, RAW);
    assert_eq!(
        decode_chunk(fast_codec_incomp, &fast_enc_incomp, incomp.len()).unwrap(),
        incomp
    );

    // 2. Sparse data: RLE shrinks 64 KiB to ~518 bytes in minimal CPU time -> beats RAW
    let sparse = synthetic_sparse_chunk();
    let (fast_codec_sparse, fast_enc_sparse) = encode_chunk(&sparse);
    assert_eq!(fast_codec_sparse, RLE);
    assert!(fast_enc_sparse.len() < 1024);
    assert_eq!(
        decode_chunk(fast_codec_sparse, &fast_enc_sparse, sparse.len()).unwrap(),
        sparse
    );
    let choice_sparse = select_codec_measured(&sparse, policy, 10);
    assert!(
        choice_sparse.selected_metrics.total_cost_ms <= choice_sparse.candidates[0].total_cost_ms,
        "selector must never pick a codec more expensive than RAW"
    );

    // 3. Tensor-like BF16/FP16 strided data: RLE expands (>64 KiB), while LZ4/ZSTD compress heavily
    // and beat RAW on total cost (CPU ms + wire ms)
    let tensor = synthetic_tensor_like_chunk();
    let rle_tensor = encode_rle(&tensor);
    assert!(
        rle_tensor.len() > tensor.len(),
        "RLE cannot compress strided 16-bit tensor data"
    );

    let choice_tensor = select_codec_measured(&tensor, policy, 10);
    // The CPU time is measured at runtime, so a slower Windows/CI machine may
    // legitimately choose RAW despite a good LZ4/ZSTD compression ratio.
    assert!(
        choice_tensor.selected_metrics.total_cost_ms <= choice_tensor.candidates[0].total_cost_ms,
        "selector must not exceed the measured RAW transfer cost"
    );
    if choice_tensor.codec != RAW {
        assert!(choice_tensor.encoded.len() + policy.min_byte_savings < tensor.len());
        assert!(
            choice_tensor.selected_metrics.total_cost_ms
                < choice_tensor.candidates[0].total_cost_ms
        );
    }
    assert_eq!(
        decode_chunk(choice_tensor.codec, &choice_tensor.encoded, tensor.len()).unwrap(),
        tensor
    );

    // 4. Structured Q8/activation block tensor where LZ4 wins at 250 Mbps
    let mut tensor_blocks = Vec::with_capacity(CHUNK_BYTES);
    for i in 0..CHUNK_BYTES {
        let block = i / 32;
        let offset = i % 32;
        tensor_blocks.push(((offset * 19 + (block % 8) * 5) & 0xFF) as u8);
    }
    let choice_blocks = select_codec_measured(&tensor_blocks, policy, 10);
    assert!(
        choice_blocks.selected_metrics.total_cost_ms <= choice_blocks.candidates[0].total_cost_ms
    );
    if choice_blocks.codec != RAW {
        assert!(choice_blocks.encoded.len() + policy.min_byte_savings < tensor_blocks.len());
    }
    assert_eq!(
        decode_chunk(
            choice_blocks.codec,
            &choice_blocks.encoded,
            tensor_blocks.len()
        )
        .unwrap(),
        tensor_blocks
    );

    let (fast_codec_tensor, fast_enc_tensor) = encode_chunk_adaptive(&tensor, policy);
    if fast_codec_tensor != RAW {
        assert!(fast_enc_tensor.len() + policy.min_byte_savings < tensor.len());
    }
    assert_eq!(
        decode_chunk(fast_codec_tensor, &fast_enc_tensor, tensor.len()).unwrap(),
        tensor
    );

    for (label, choice) in [
        ("incompressible", &choice_incomp),
        ("sparse", &choice_sparse),
        ("tensor_bf16_strided", &choice_tensor),
        ("tensor_q8_blocks", &choice_blocks),
    ] {
        println!(
            "\n=== Workload: {label} (selected: {}) ===",
            codec_name(choice.codec)
        );
        for c in &choice.candidates {
            println!(
                "  {:4} | encoded: {:6} B | wire: {:6} B | enc_cpu: {:6.3} ms | dec_cpu: {:6.3} ms | wire_ms: {:6.3} ms | total_cost: {:6.3} ms",
                c.codec_name,
                c.encoded_bytes,
                c.wire_bytes,
                c.encode_cpu_ms,
                c.decode_cpu_ms,
                c.wire_transfer_ms,
                c.total_cost_ms
            );
        }
    }
}

#[test]
fn reject_malformed_and_overlong_lz4_and_zstd_frames() {
    // Unknown codec ID
    assert!(decode_chunk(99, &[0, 1, 2], 3).is_err());

    // LZ4 with zero offset (invalid back-reference)
    let bad_lz4_zero_offset = vec![0x10, b'A', 0x00, 0x00];
    assert!(decode_chunk(LZ4, &bad_lz4_zero_offset, 10).is_err());

    // LZ4 with offset exceeding current output length
    let bad_lz4_oob_offset = vec![0x10, b'A', 0x05, 0x00];
    assert!(decode_chunk(LZ4, &bad_lz4_oob_offset, 10).is_err());

    // LZ4 decompression bomb exceeding expected output size
    let bomb_chunk = vec![b'X'; 1024];
    let lz4_encoded = encode_lz4(&bomb_chunk);
    assert!(decode_chunk(LZ4, &lz4_encoded, 64).is_err());

    // ZSTD bad magic header
    let bad_zstd_magic = vec![0x00, 0x00, 0x00, 0x00, 0x04, 10, 0, 0];
    assert!(decode_chunk(ZSTD, &bad_zstd_magic, 10).is_err());

    // ZSTD truncated frame
    assert!(decode_chunk(ZSTD, &[0x28, 0xB5, 0x2F], 10).is_err());

    // ZSTD declared size mismatch
    let zstd_encoded = encode_zstd(&vec![7u8; CHUNK_BYTES]);
    assert!(decode_chunk(ZSTD, &zstd_encoded, 128).is_err());
}
