//! Lightweight loopback TCP transport for FASTLINK-RPC/1 framed tensors.
//!
//! This module adds no codec logic and no dependency. [`send_tensor`] frames a
//! tensor with the existing [`crate::rpc_client::frame_tensor`] and writes the
//! FASTLINK-RPC/1 adapter payloads over one TCP connection; [`receive_tensor`]
//! reads the same records back, reconstructs the tensor and verifies every
//! chunk while decoding it.
//!
//! Wire layout (all integers little-endian):
//!
//! ```text
//! total_len: u64
//! chunk_count: u32
//! chunk_count × [codec: u8][encoded_len: u32][encoded bytes]
//! ```
//!
//! The per-chunk record is exactly the adapter payload from
//! [`crate::rpc_client::FramedChunk::adapter_payload`]. The 12-byte tensor
//! header is the only transport overhead: every chunk keeps the adapter's own
//! 5-byte header, and all records are coalesced into one buffer so a tensor
//! costs a single `write_all` instead of one syscall per chunk.
//!
//! **Experimental and unauthenticated.** This is a loopback measurement
//! transport, not a secure channel: there is no encryption, authentication,
//! replay protection or admission control. Do not expose it beyond loopback;
//! use SSH forwarding or a future authenticated transport.

use std::io::{self, Read, Write};
use std::net::TcpStream;

use crate::rpc_client::frame_tensor;
use crate::{
    chunk_count, decode_chunk, expected_chunk_len, read_u32, read_u64, read_u8, MAX_ENCODED_BYTES,
};

/// Bytes in the transport tensor header: `total_len` (u64) + `chunk_count` (u32).
pub const TENSOR_HEADER_BYTES: usize = 12;

/// Frame `tensor` with [`frame_tensor`] and write every per-chunk adapter
/// payload to `sock`.
///
/// Returns the total number of wire bytes written, including the 12-byte tensor
/// header and every per-chunk adapter header. The result is at least the
/// encoded payload size; for incompressible tensors it is `>= tensor.len()`.
///
/// All records are concatenated into one buffer and written with a single
/// `write_all`; this keeps per-chunk network overhead to the adapter's own
/// 5-byte header and avoids one syscall per chunk. An empty tensor is rejected
/// by [`frame_tensor`].
pub fn send_tensor(sock: &mut TcpStream, tensor: &[u8]) -> io::Result<usize> {
    let framed = frame_tensor(tensor)?;
    let mut wire = Vec::with_capacity(
        TENSOR_HEADER_BYTES
            + framed
                .iter()
                .map(|chunk| chunk.adapter_wire_len())
                .sum::<usize>(),
    );
    wire.extend_from_slice(&(tensor.len() as u64).to_le_bytes());
    wire.extend_from_slice(&(framed.len() as u32).to_le_bytes());
    for chunk in &framed {
        wire.extend_from_slice(&chunk.adapter_payload());
    }
    sock.write_all(&wire)?;
    Ok(wire.len())
}

/// Read one framed tensor from `sock`, reconstruct it and verify every chunk.
///
/// Each chunk is decoded with [`decode_chunk`] against the raw length implied
/// by the tensor header, so an out-of-bounds encoded length, an unknown codec, a
/// truncated payload or a decoded size mismatch fails the whole read. Returns
/// the reconstructed tensor bytes.
pub fn receive_tensor(sock: &mut TcpStream) -> io::Result<Vec<u8>> {
    let total_len = read_u64(sock)?;
    let count = read_u32(sock)?;
    if total_len == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "tensor length must be positive",
        ));
    }
    if count != chunk_count(total_len)? {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "chunk count does not match tensor length",
        ));
    }

    let mut tensor = Vec::new();
    for index in 0..count {
        let codec = read_u8(sock)?;
        let encoded_len = read_u32(sock)? as usize;
        if encoded_len == 0 || encoded_len > MAX_ENCODED_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "encoded chunk length is out of bounds",
            ));
        }
        let mut encoded = vec![0u8; encoded_len];
        sock.read_exact(&mut encoded)?;
        let raw_len = expected_chunk_len(total_len, index)?;
        tensor.extend_from_slice(&decode_chunk(codec, &encoded, raw_len)?);
    }
    Ok(tensor)
}
