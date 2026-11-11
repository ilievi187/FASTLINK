//! Malformed-input regressions for the experimental FLB4 batch protocol.
//! These tests must reject dangerous headers *before* reserving payload memory.
use fastlink::{sync_decision, BatchedSyncFrame, CodecCostPolicy, RemoteWorkerStore};

fn fake_op(total_bytes: u64, chunk_bytes: u32, total_chunks: u32, updates: u32) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"FLB4");
    bytes.extend_from_slice(&4u16.to_le_bytes()); // protocol version
    bytes.extend_from_slice(&1u64.to_le_bytes()); // batch id
    bytes.extend_from_slice(&1u32.to_le_bytes()); // op_count
    bytes.push(1); // OP_FULL
    for _ in 0..3 {
        bytes.extend_from_slice(&1u16.to_le_bytes()); // session, tensor, owner
        bytes.push(b'a');
    }
    bytes.extend_from_slice(&1u64.to_le_bytes()); // tensor version
    bytes.extend_from_slice(&total_bytes.to_le_bytes());
    bytes.extend_from_slice(&chunk_bytes.to_le_bytes());
    bytes.extend_from_slice(&total_chunks.to_le_bytes());
    bytes.extend_from_slice(&[0u8; 32]); // content hash
    bytes.extend_from_slice(&updates.to_le_bytes());
    bytes
}

fn valid_frame() -> BatchedSyncFrame {
    let bytes = b"small but valid tensor";
    let decision = sync_decision("tensor.a", bytes, None, CodecCostPolicy::default());
    let mut frame = BatchedSyncFrame::new(1);
    frame.push_decision("session.a", "worker.a", bytes.len(), &decision);
    frame
}

#[test]
fn valid_batch_roundtrips_and_applies() {
    let frame = valid_frame();
    let wire = frame.encode().unwrap();
    let decoded = BatchedSyncFrame::decode(&wire).unwrap();
    assert_eq!(decoded, frame);
    let mut worker = RemoteWorkerStore::new("worker.a");
    assert_eq!(worker.apply_batch(&decoded).unwrap().0, 1);
}

#[test]
fn rejects_unbounded_update_count_without_allocation() {
    let malformed = fake_op(1, 1, 1, u32::MAX);
    assert!(BatchedSyncFrame::decode(&malformed).is_err());
}

#[test]
fn rejects_zero_chunk_geometry_without_allocation() {
    let malformed = fake_op(1, 1, 0, u32::MAX);
    assert!(BatchedSyncFrame::decode(&malformed).is_err());
}

#[test]
fn rejects_oversize_tensor_before_payload_allocation() {
    let size = 512u64 * 1024 * 1024 + 1;
    let chunks = size.div_ceil(256 * 1024) as u32;
    let malformed = fake_op(size, 256 * 1024, chunks, 0);
    assert!(BatchedSyncFrame::decode(&malformed).is_err());
}

#[test]
fn rejects_oversize_op_count_before_allocation() {
    let mut bytes = b"FLB4".to_vec();
    bytes.extend_from_slice(&4u16.to_le_bytes());
    bytes.extend_from_slice(&1u64.to_le_bytes());
    bytes.extend_from_slice(&u32::MAX.to_le_bytes());
    assert!(BatchedSyncFrame::decode(&bytes).is_err());
}

#[test]
fn rejects_trailing_bytes_in_buffer_decoder() {
    let mut wire = valid_frame().encode().unwrap();
    wire.push(0xAA);
    assert!(BatchedSyncFrame::decode(&wire).is_err());
}

#[test]
fn encoder_rejects_identifier_that_decoder_would_reject() {
    let mut frame = valid_frame();
    frame.ops[0].tensor_id = "x".repeat(4_097);
    assert!(frame.encode().is_err());
}

#[test]
fn encoder_and_direct_worker_reject_oversize_tensor() {
    let mut frame = valid_frame();
    frame.ops[0].total_bytes = 512 * 1024 * 1024 + 1;
    frame.ops[0].chunk_bytes = 256 * 1024;
    frame.ops[0].total_chunks = frame.ops[0].total_bytes.div_ceil(256 * 1024) as u32;
    assert!(frame.encode().is_err());
    let mut worker = RemoteWorkerStore::new("worker.a");
    assert!(worker.apply_batch(&frame).is_err());
}

#[test]
fn direct_worker_rejects_zero_chunk_size_without_panicking() {
    let mut frame = valid_frame();
    frame.ops[0].chunk_bytes = 0;
    let mut worker = RemoteWorkerStore::new("worker.a");
    assert!(worker.apply_batch(&frame).is_err());
}
