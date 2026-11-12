//! FASTLINK: conservative, lossless content-addressed transfer primitives.
//! This is an independent experimental protocol, not a GPU driver.

pub mod cli_plan;
pub mod cluster_orchestrator;
pub mod kv_delta;
pub mod manifest;
pub mod node_runtime;
pub mod pairing;
pub mod pipeline;
pub mod residency;
pub mod scheduler;
pub mod topology;

use std::io::{self, Read, Write};

pub const MAGIC: &[u8; 4] = b"FLNK";
pub const VERSION: u16 = 1;
pub const CHUNK_BYTES: usize = 64 * 1024;
pub const MAX_CHUNKS: u32 = 1_000_000;
pub const MAX_FILE_BYTES: u64 = CHUNK_BYTES as u64 * MAX_CHUNKS as u64;
pub const MAX_ENCODED_BYTES: usize = CHUNK_BYTES * 2;
pub type Digest = [u8; 32];

pub fn digest(data: &[u8]) -> Digest {
    *blake3::hash(data).as_bytes()
}
pub fn hex(hash: &Digest) -> String {
    blake3::Hash::from(*hash).to_hex().to_string()
}
pub fn expected_chunk_len(total_bytes: u64, index: u32) -> io::Result<usize> {
    let start = index as u64 * CHUNK_BYTES as u64;
    if start >= total_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid chunk index",
        ));
    }
    Ok((total_bytes - start).min(CHUNK_BYTES as u64) as usize)
}
pub fn chunk_count(total_bytes: u64) -> io::Result<u32> {
    if total_bytes > MAX_FILE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "file exceeds transfer limit",
        ));
    }
    let count = total_bytes.div_ceil(CHUNK_BYTES as u64);
    u32::try_from(count).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "too many chunks"))
}

pub const RAW: u8 = 0;
pub const RLE: u8 = 1;

pub fn encode_chunk(chunk: &[u8]) -> (u8, Vec<u8>) {
    // A tiny lossless codec for easy-to-test sparse/uniform data.
    // Future milestone: benchmark LZ4/Zstd and choose by measured wall-clock time.
    let mut encoded = Vec::new();
    let mut idx = 0;
    while idx < chunk.len() {
        let byte = chunk[idx];
        let mut count = 1usize;
        while idx + count < chunk.len() && count < u8::MAX as usize && chunk[idx + count] == byte {
            count += 1;
        }
        encoded.push(count as u8);
        encoded.push(byte);
        idx += count;
    }
    if encoded.len().saturating_add(32) < chunk.len() {
        (RLE, encoded)
    } else {
        (RAW, chunk.to_vec())
    }
}

pub fn decode_chunk(codec: u8, bytes: &[u8], expected: usize) -> io::Result<Vec<u8>> {
    if expected > CHUNK_BYTES || bytes.len() > MAX_ENCODED_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "chunk exceeds limit",
        ));
    }
    let decoded = match codec {
        RAW => bytes.to_vec(),
        RLE => {
            if !bytes.len().is_multiple_of(2) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid RLE payload",
                ));
            }
            let mut output = Vec::with_capacity(expected);
            for pair in bytes.as_chunks::<2>().0 {
                let run = pair[0] as usize;
                if run == 0 || output.len() + run > expected {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "RLE exceeds expected size",
                    ));
                }
                output.extend(std::iter::repeat_n(pair[1], run));
            }
            output
        }
        _ => return Err(io::Error::new(io::ErrorKind::InvalidData, "unknown codec")),
    };
    if decoded.len() != expected {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "decoded size mismatch",
        ));
    }
    Ok(decoded)
}

pub fn read_u8(r: &mut impl Read) -> io::Result<u8> {
    let mut bytes = [0u8; 1];
    r.read_exact(&mut bytes)?;
    Ok(bytes[0])
}
pub fn read_u16(r: &mut impl Read) -> io::Result<u16> {
    let mut bytes = [0u8; 2];
    r.read_exact(&mut bytes)?;
    Ok(u16::from_le_bytes(bytes))
}
pub fn read_u32(r: &mut impl Read) -> io::Result<u32> {
    let mut bytes = [0u8; 4];
    r.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}
pub fn read_u64(r: &mut impl Read) -> io::Result<u64> {
    let mut bytes = [0u8; 8];
    r.read_exact(&mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}
pub fn write_u8(w: &mut impl Write, value: u8) -> io::Result<()> {
    w.write_all(&[value])
}
pub fn write_u16(w: &mut impl Write, value: u16) -> io::Result<()> {
    w.write_all(&value.to_le_bytes())
}
pub fn write_u32(w: &mut impl Write, value: u32) -> io::Result<()> {
    w.write_all(&value.to_le_bytes())
}
pub fn write_u64(w: &mut impl Write, value: u64) -> io::Result<()> {
    w.write_all(&value.to_le_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rle_sparse_roundtrip() {
        let bytes = vec![0u8; CHUNK_BYTES];
        let (codec, compressed) = encode_chunk(&bytes);
        assert_eq!(codec, RLE);
        assert!(compressed.len() < 1024);
        assert_eq!(
            decode_chunk(codec, &compressed, bytes.len()).unwrap(),
            bytes
        );
    }
    #[test]
    fn incompressible_roundtrip() {
        let bytes: Vec<u8> = (0..CHUNK_BYTES)
            .map(|i| (i.wrapping_mul(13) % 251) as u8)
            .collect();
        let (codec, compressed) = encode_chunk(&bytes);
        assert_eq!(
            decode_chunk(codec, &compressed, bytes.len()).unwrap(),
            bytes
        );
    }
    #[test]
    fn reject_rle_bomb() {
        let bad = vec![255, 42, 255, 42];
        assert!(decode_chunk(RLE, &bad, 100).is_err());
    }
    #[test]
    fn checksums_change_when_content_changes() {
        assert_ne!(digest(b"abc"), digest(b"abd"));
        assert_eq!(digest(b"abc"), digest(b"abc"));
    }
    #[test]
    fn chunk_count_boundaries() {
        assert_eq!(chunk_count(0).unwrap(), 0);
        assert_eq!(chunk_count(1).unwrap(), 1);
        assert_eq!(chunk_count(CHUNK_BYTES as u64).unwrap(), 1);
        assert_eq!(chunk_count(CHUNK_BYTES as u64 + 1).unwrap(), 2);
    }
}
