//! Tensor framing for the FASTLINK-RPC/1 llama.cpp adapter.
//!
//! This module bridges two related but deliberately distinct formats:
//!
//! * **FASTLINK v0.1** (this crate): 64 KiB chunks, BLAKE3 content addresses,
//!   RAW/RLE payloads.
//! * **FASTLINK-RPC/1** (llama.cpp fork): FLR1/FLR3 frames whose per-chunk
//!   wire payload is `codec:u8`, `encoded_size:u32` (little-endian), then the
//!   encoded bytes. The adapter authenticates a frame with SHA-256 and rejects
//!   frames smaller than 128 KiB, so tensors below that threshold are sent with
//!   the stock `SET_TENSOR` path instead.
//!
//! [`frame_tensor`] keeps the v0.1 chunking and BLAKE3 identities on the Rust
//! side and renders each chunk exactly as the adapter expects it inside an
//! FLR1/FLR3 frame. It does **not** emit the outer FLR1/FLR3 envelope: that
//! envelope needs SHA-256 over the original RPC header plus tensor bytes, and
//! the adapter has no BLAKE3 field. A full-size outer frame belongs to the
//! transport layer, not to this content-addressing layer.
//!
//! The codec choice matches the adapter's own v1 encoder exactly:
//! RLE is used only when `encoded_len + 32 < raw_len`; otherwise RAW wins.

use std::io;

use crate::{
    chunk_count, decode_chunk, digest, encode_chunk, expected_chunk_len, Digest, CHUNK_BYTES,
};

/// Bytes in the adapter's FLR1 per-chunk header: codec (1) + encoded size (4).
pub const ADAPTER_HEADER_BYTES: usize = 5;

/// One tensor chunk after v0.1 codec selection, with both its content address
/// and the bytes that belong inside an FLR1/FLR3 frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FramedChunk {
    /// Zero-based chunk position inside the tensor.
    pub index: u32,
    /// BLAKE3 content address of the uncompressed chunk (FASTLINK v0.1).
    pub chunk_id: Digest,
    /// Uncompressed chunk length in bytes (1..=65536).
    pub raw_len: usize,
    /// FASTLINK codec ID: 0 = RAW, 1 = RLE.
    pub codec: u8,
    /// Codec payload without any framing header.
    pub encoded: Vec<u8>,
}

impl FramedChunk {
    /// Render the exact per-chunk payload consumed by the adapter:
    /// `[codec: u8][encoded_size: u32 LE][encoded bytes]`.
    pub fn adapter_payload(&self) -> Vec<u8> {
        let encoded_len = u32::try_from(self.encoded.len())
            .expect("chunk payload cannot exceed the adapter limit");
        let mut payload = Vec::with_capacity(ADAPTER_HEADER_BYTES + self.encoded.len());
        payload.push(self.codec);
        payload.extend_from_slice(&encoded_len.to_le_bytes());
        payload.extend_from_slice(&self.encoded);
        payload
    }

    /// Length of [`FramedChunk::adapter_payload`] in bytes.
    pub fn adapter_wire_len(&self) -> usize {
        ADAPTER_HEADER_BYTES + self.encoded.len()
    }
}

/// Frame one already-sliced chunk. `chunk` must be 1..=64 KiB; the adapter
/// rejects zero-length chunks and chunks larger than 64 KiB.
pub fn frame_chunk(index: u32, chunk: &[u8]) -> io::Result<FramedChunk> {
    if chunk.is_empty() || chunk.len() > CHUNK_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "chunk length must be between 1 and 65536 bytes",
        ));
    }
    let (codec, encoded) = encode_chunk(chunk);
    Ok(FramedChunk {
        index,
        chunk_id: digest(chunk),
        raw_len: chunk.len(),
        codec,
        encoded,
    })
}

/// Split a tensor into FASTLINK v0.1 chunks and frame each one for the
/// adapter's FLR1/FLR3 per-chunk wire layout.
///
/// An empty tensor is rejected because the adapter requires `raw_len > 0`.
pub fn frame_tensor(tensor: &[u8]) -> io::Result<Vec<FramedChunk>> {
    if tensor.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "tensor must not be empty",
        ));
    }
    let total = tensor.len() as u64;
    let count = chunk_count(total)?;
    let mut framed = Vec::with_capacity(count as usize);
    for index in 0..count {
        let len = expected_chunk_len(total, index)?;
        let start = index as usize * CHUNK_BYTES;
        framed.push(frame_chunk(index, &tensor[start..start + len])?);
    }
    Ok(framed)
}

/// Decode one adapter per-chunk payload, enforcing the declared encoded length
/// and the FASTLINK codec bounds. The caller remains responsible for checking
/// the BLAKE3 identity of the decoded bytes.
pub fn decode_adapter_chunk(payload: &[u8], expected_raw_len: usize) -> io::Result<Vec<u8>> {
    if payload.len() < ADAPTER_HEADER_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "adapter chunk payload is shorter than its header",
        ));
    }
    let codec = payload[0];
    let encoded_len = u32::from_le_bytes(
        payload[1..ADAPTER_HEADER_BYTES]
            .try_into()
            .expect("header length is checked"),
    ) as usize;
    if encoded_len == 0 || payload.len() != ADAPTER_HEADER_BYTES + encoded_len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "adapter chunk payload length does not match its header",
        ));
    }
    decode_chunk(codec, &payload[ADAPTER_HEADER_BYTES..], expected_raw_len)
}
