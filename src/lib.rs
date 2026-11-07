//! FASTLINK: conservative, lossless content-addressed transfer primitives.
//! This is an independent experimental protocol, not a GPU driver.

use std::io::{self, Read, Write};

pub mod codec;
pub use codec::{
    codec_name, decode_chunk, encode_chunk_adaptive, encode_lz4, encode_rle, encode_with_codec,
    encode_zstd, evaluate_codec_candidate, select_codec_measured, synthetic_incompressible_chunk,
    synthetic_sparse_chunk, synthetic_tensor_like_chunk, AdaptiveCodecChoice,
    CodecCandidateMetrics, CodecCostPolicy, LZ4, RAW, RLE, ZSTD,
};

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

pub fn encode_chunk(chunk: &[u8]) -> (u8, Vec<u8>) {
    encode_chunk_adaptive(chunk, CodecCostPolicy::default())
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

/// Stream wrapper that counts exact on-wire bytes read and written at the application socket layer.
#[derive(Debug)]
pub struct CountingStream<S> {
    inner: S,
    bytes_rx: u64,
    bytes_tx: u64,
}

impl<S> CountingStream<S> {
    pub fn new(inner: S) -> Self {
        Self {
            inner,
            bytes_rx: 0,
            bytes_tx: 0,
        }
    }

    pub fn bytes_rx(&self) -> u64 {
        self.bytes_rx
    }

    pub fn bytes_tx(&self) -> u64 {
        self.bytes_tx
    }

    pub fn bytes_total(&self) -> u64 {
        self.bytes_rx.saturating_add(self.bytes_tx)
    }

    pub fn inner(&self) -> &S {
        &self.inner
    }

    pub fn inner_mut(&mut self) -> &mut S {
        &mut self.inner
    }

    pub fn into_inner(self) -> S {
        self.inner
    }
}

impl<S: Read> Read for CountingStream<S> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.bytes_rx = self.bytes_rx.saturating_add(n as u64);
        Ok(n)
    }
}

impl<S: Write> Write for CountingStream<S> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.bytes_tx = self.bytes_tx.saturating_add(n as u64);
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Exact protocol v0.1 wire-byte breakdown (excluding kernel TCP/IP/Ethernet headers).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WireBreakdown {
    pub handshake_wire_bytes: u64,
    pub manifest_wire_bytes: u64,
    pub need_wire_bytes: u64,
    pub chunk_framing_wire_bytes: u64,
    pub encoded_payload_bytes: u64,
    pub status_wire_bytes: u64,
    pub headers_and_framing_wire_bytes: u64,
    pub expected_wire_bytes_tx: u64,
    pub expected_wire_bytes_rx: u64,
    pub expected_wire_bytes_total: u64,
}

impl WireBreakdown {
    pub const HANDSHAKE_TX_BYTES: u64 = 6;
    pub const HANDSHAKE_RX_BYTES: u64 = 6;
    pub const HANDSHAKE_TOTAL_BYTES: u64 = Self::HANDSHAKE_TX_BYTES + Self::HANDSHAKE_RX_BYTES;
    pub const MANIFEST_FIXED_BYTES: u64 = 1 + 2 + 8 + 4 + 4; // cmd(1) + name_len(2) + size(8) + chunk_bytes(4) + count(4)
    pub const CHUNK_FRAME_HEADER_BYTES: u64 = 4 + 1 + 4; // index(4) + codec(1) + encoded_len(4)
    pub const STATUS_BYTES: u64 = 1;

    pub fn compute(
        filename_len: usize,
        chunks_total: u32,
        chunks_transferred: u32,
        encoded_payload_bytes: u64,
    ) -> Self {
        let handshake_wire_bytes = Self::HANDSHAKE_TOTAL_BYTES;
        let manifest_wire_bytes =
            Self::MANIFEST_FIXED_BYTES + filename_len as u64 + (chunks_total as u64 * 32);
        let need_wire_bytes = 4 + (chunks_transferred as u64 * 4);
        let chunk_framing_wire_bytes = chunks_transferred as u64 * Self::CHUNK_FRAME_HEADER_BYTES;
        let status_wire_bytes = Self::STATUS_BYTES;
        let headers_and_framing_wire_bytes = handshake_wire_bytes
            + manifest_wire_bytes
            + need_wire_bytes
            + chunk_framing_wire_bytes
            + status_wire_bytes;
        let expected_wire_bytes_tx = Self::HANDSHAKE_TX_BYTES
            + manifest_wire_bytes
            + chunk_framing_wire_bytes
            + encoded_payload_bytes;
        let expected_wire_bytes_rx = Self::HANDSHAKE_RX_BYTES + need_wire_bytes + status_wire_bytes;
        let expected_wire_bytes_total = expected_wire_bytes_tx + expected_wire_bytes_rx;
        Self {
            handshake_wire_bytes,
            manifest_wire_bytes,
            need_wire_bytes,
            chunk_framing_wire_bytes,
            encoded_payload_bytes,
            status_wire_bytes,
            headers_and_framing_wire_bytes,
            expected_wire_bytes_tx,
            expected_wire_bytes_rx,
            expected_wire_bytes_total,
        }
    }
}

/// Compute throughput in MiB/s (`bytes / 2^20 / seconds`). Returns 0.0 for zero bytes or zero elapsed time.
pub fn mib_per_sec(bytes: u64, elapsed: std::time::Duration) -> f64 {
    let secs = elapsed.as_secs_f64();
    if bytes == 0 || secs <= 0.0 || !secs.is_finite() {
        return 0.0;
    }
    (bytes as f64 / (1024.0 * 1024.0)) / secs
}

/// End-to-end transfer metrics captured by the sender.
#[derive(Debug, Clone, PartialEq)]
pub struct TransferMetrics {
    pub file_bytes: u64,
    pub chunks_total: u32,
    pub chunks_reused: u32,
    pub chunks_transferred: u32,
    pub raw_missing_bytes: u64,
    pub encoded_payload_bytes: u64,
    pub handshake_wire_bytes: u64,
    pub manifest_wire_bytes: u64,
    pub need_wire_bytes: u64,
    pub chunk_framing_wire_bytes: u64,
    pub status_wire_bytes: u64,
    pub headers_and_framing_wire_bytes: u64,
    pub wire_bytes_tx: u64,
    pub wire_bytes_rx: u64,
    pub wire_bytes_total: u64,
    pub handshake_us: u64,
    pub manifest_us: u64,
    pub transfer_us: u64,
    pub elapsed_us: u64,
    pub elapsed_ms: u64,
    pub wire_mib_per_s: f64,
    pub logical_mib_per_s: f64,
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

    #[test]
    fn counting_stream_tracks_read_and_write_bytes() {
        use std::io::Cursor;

        #[derive(Default)]
        struct MemoryDuplex {
            rx: Cursor<Vec<u8>>,
            tx: Vec<u8>,
        }
        impl Read for MemoryDuplex {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                self.rx.read(buf)
            }
        }
        impl Write for MemoryDuplex {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                self.tx.extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let duplex = MemoryDuplex {
            rx: Cursor::new(vec![b'F', b'L', b'N', b'K', 1, 0, 0]),
            tx: Vec::new(),
        };
        let mut counted = CountingStream::new(duplex);
        counted.write_all(MAGIC).unwrap();
        write_u16(&mut counted, VERSION).unwrap();
        write_u32(&mut counted, 42).unwrap();
        counted.flush().unwrap();

        let mut magic = [0u8; 4];
        counted.read_exact(&mut magic).unwrap();
        let ver = read_u16(&mut counted).unwrap();
        let status = read_u8(&mut counted).unwrap();

        assert_eq!(&magic, MAGIC);
        assert_eq!(ver, VERSION);
        assert_eq!(status, 0);
        assert_eq!(counted.bytes_tx(), 10);
        assert_eq!(counted.bytes_rx(), 7);
        assert_eq!(counted.bytes_total(), 17);
    }

    #[test]
    fn wire_breakdown_matches_cold_and_warm_protocol_frames() {
        // 1 MiB fixture ("tensor-fixture.bin" = 18 bytes), 16 chunks
        let cold = WireBreakdown::compute(18, 16, 16, 1_048_576);
        assert_eq!(cold.handshake_wire_bytes, 12);
        assert_eq!(cold.manifest_wire_bytes, 549);
        assert_eq!(cold.need_wire_bytes, 68);
        assert_eq!(cold.chunk_framing_wire_bytes, 144);
        assert_eq!(cold.status_wire_bytes, 1);
        assert_eq!(cold.headers_and_framing_wire_bytes, 774);
        assert_eq!(cold.expected_wire_bytes_tx, 1_049_275);
        assert_eq!(cold.expected_wire_bytes_rx, 75);
        assert_eq!(cold.expected_wire_bytes_total, 1_049_350);

        let warm = WireBreakdown::compute(18, 16, 0, 0);
        assert_eq!(warm.handshake_wire_bytes, 12);
        assert_eq!(warm.manifest_wire_bytes, 549);
        assert_eq!(warm.need_wire_bytes, 4);
        assert_eq!(warm.chunk_framing_wire_bytes, 0);
        assert_eq!(warm.status_wire_bytes, 1);
        assert_eq!(warm.headers_and_framing_wire_bytes, 566);
        assert_eq!(warm.expected_wire_bytes_tx, 555);
        assert_eq!(warm.expected_wire_bytes_rx, 11);
        assert_eq!(warm.expected_wire_bytes_total, 566);
    }

    #[test]
    fn throughput_mib_per_s_handles_normal_and_zero_durations() {
        use std::time::Duration;
        assert_eq!(mib_per_sec(0, Duration::from_millis(100)), 0.0);
        assert_eq!(mib_per_sec(1_048_576, Duration::ZERO), 0.0);
        let rate = mib_per_sec(10 * 1024 * 1024, Duration::from_millis(500));
        assert!((rate - 20.0).abs() < 1e-6);
    }
}
