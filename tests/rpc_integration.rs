//! Integration tests for the FASTLINK-RPC/1 adapter chunk framing.
//!
//! The golden vector below is intentionally hard-coded: it pins the wire bytes
//! and the BLAKE3 content address so an accidental change to the chunk size,
//! codec threshold, RLE run encoding, or header byte order fails loudly.

use fastlink::rpc_client::{
    decode_adapter_chunk, frame_chunk, frame_tensor, FramedChunk, ADAPTER_HEADER_BYTES,
};
use fastlink::{digest, hex, CHUNK_BYTES, RAW, RLE};

/// BLAKE3 of 65,536 bytes with value 0xAB.
const EXPECTED_CHUNK_ID: &str = "b01eb9000c0964964671d09cab928d484b6381cdd0bc1e78f5e788d560bb7984";
/// BLAKE3 of the full adapter payload for the same tensor.
const EXPECTED_PAYLOAD_ID: &str =
    "535a74d246f760d030f3a45227ee9e1c8d5a1dcbbd03924be169f4c60e99d7c0";

fn uniform_tensor(value: u8) -> Vec<u8> {
    vec![value; CHUNK_BYTES]
}

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

fn roundtrip(chunk: &FramedChunk, raw: &[u8]) {
    let payload = chunk.adapter_payload();
    assert_eq!(payload.len(), chunk.adapter_wire_len());
    assert_eq!(payload[0], chunk.codec);
    let declared = u32::from_le_bytes(payload[1..ADAPTER_HEADER_BYTES].try_into().unwrap());
    assert_eq!(declared as usize, chunk.encoded.len());
    let decoded = decode_adapter_chunk(&payload, chunk.raw_len).expect("payload decodes");
    assert_eq!(decoded, raw);
    assert_eq!(digest(&decoded), chunk.chunk_id);
}

#[test]
fn frames_64kib_tensor_byte_exact() {
    let tensor = uniform_tensor(0xAB);
    let framed = frame_tensor(&tensor).expect("tensor frames");
    assert_eq!(framed.len(), 1, "64 KiB is exactly one v0.1 chunk");

    let chunk = &framed[0];
    assert_eq!(chunk.index, 0);
    assert_eq!(chunk.raw_len, CHUNK_BYTES);
    assert_eq!(chunk.codec, RLE, "uniform data must use RLE");
    assert_eq!(chunk.chunk_id, digest(&tensor));
    assert_eq!(hex(&chunk.chunk_id), EXPECTED_CHUNK_ID);

    // v0.1 RLE is (run: u8, value: u8) with runs capped at 255:
    // 65,536 = 255 * 257 + 1, so 258 pairs = 516 encoded bytes.
    let mut expected = Vec::with_capacity(ADAPTER_HEADER_BYTES + 516);
    expected.push(RLE);
    expected.extend_from_slice(&516_u32.to_le_bytes());
    for _ in 0..257 {
        expected.push(255);
        expected.push(0xAB);
    }
    expected.push(1);
    expected.push(0xAB);

    let payload = chunk.adapter_payload();
    assert_eq!(chunk.encoded.len(), 516);
    assert_eq!(payload, expected, "adapter wire bytes must be exact");
    assert_eq!(hex(&digest(&payload)), EXPECTED_PAYLOAD_ID);
    roundtrip(chunk, &tensor);
}

#[test]
fn framing_is_deterministic() {
    let tensor = uniform_tensor(0x5A);
    let first = frame_tensor(&tensor).expect("first framing");
    let second = frame_tensor(&tensor).expect("second framing");
    assert_eq!(first, second);
    assert_eq!(
        first[0].adapter_payload(),
        second[0].adapter_payload(),
        "repeated framing must be byte-identical"
    );

    let mut mutated = tensor.clone();
    mutated[12_345] ^= 0xFF;
    let changed = frame_tensor(&mutated).expect("mutated framing");
    assert_ne!(first[0].chunk_id, changed[0].chunk_id);
    assert_ne!(first[0].adapter_payload(), changed[0].adapter_payload());
}

#[test]
fn splits_multi_chunk_tensor_in_order() {
    let len = CHUNK_BYTES * 2 + 1_000;
    let tensor = patterned_tensor(len);
    let framed = frame_tensor(&tensor).expect("tensor frames");
    assert_eq!(framed.len(), 3);

    let expected_lengths = [CHUNK_BYTES, CHUNK_BYTES, 1_000];
    let mut offset = 0;
    for (index, chunk) in framed.iter().enumerate() {
        let raw_len = expected_lengths[index];
        let raw = &tensor[offset..offset + raw_len];
        assert_eq!(chunk.index, index as u32);
        assert_eq!(chunk.raw_len, raw_len);
        assert_eq!(chunk.chunk_id, digest(raw));
        roundtrip(chunk, raw);
        offset += raw_len;
    }
    assert_eq!(offset, len);
}

#[test]
fn incompressible_tensor_uses_raw() {
    let tensor = patterned_tensor(CHUNK_BYTES);
    let framed = frame_tensor(&tensor).expect("tensor frames");
    assert_eq!(framed.len(), 1);
    let chunk = &framed[0];
    assert_eq!(chunk.codec, RAW);
    assert_eq!(chunk.encoded, tensor);
    roundtrip(chunk, &tensor);
}

#[test]
fn rejects_invalid_chunks_and_payloads() {
    assert!(frame_tensor(&[]).is_err(), "empty tensor must be rejected");
    assert!(frame_chunk(0, &[]).is_err(), "empty chunk must be rejected");
    assert!(
        frame_chunk(0, &vec![0u8; CHUNK_BYTES + 1]).is_err(),
        "chunk larger than 64 KiB must be rejected"
    );

    assert!(
        decode_adapter_chunk(&[], 1).is_err(),
        "short header must be rejected"
    );

    let zero_len = [RAW, 0, 0, 0, 0];
    assert!(
        decode_adapter_chunk(&zero_len, 1).is_err(),
        "zero encoded length must be rejected"
    );

    let truncated = [RAW, 3, 0, 0, 0, 1, 2];
    assert!(
        decode_adapter_chunk(&truncated, 3).is_err(),
        "declared length beyond payload must be rejected"
    );

    let unknown_codec = [9u8, 1, 0, 0, 0, 7];
    assert!(
        decode_adapter_chunk(&unknown_codec, 1).is_err(),
        "unknown codec must be rejected"
    );

    let valid = frame_chunk(0, &[7u8; 16]).expect("chunk frames");
    let payload = valid.adapter_payload();
    assert!(
        decode_adapter_chunk(&payload, 15).is_err(),
        "wrong expected size must be rejected"
    );
}
