//! Adaptive lossless chunk codecs for FASTLINK: RAW, RLE, LZ4, and ZSTD.
//!
//! Codec selection is governed by a quantitative total-cost model:
//! `total_cost_ms = encode_cpu_ms + decode_cpu_ms + wire_transfer_ms`.
//! A compressed codec is chosen over `RAW` only when it strictly reduces wire
//! bytes AND yields a lower total wall-clock cost (`CPU ms + wire ms`).

use std::io;
use std::time::Instant;

use crate::{WireBreakdown, CHUNK_BYTES, MAX_ENCODED_BYTES};

pub const RAW: u8 = 0;
pub const RLE: u8 = 1;
pub const LZ4: u8 = 2;
pub const ZSTD: u8 = 3;

/// RFC 8878 Zstandard frame magic number (little-endian `0xFD2FB528`).
pub const ZSTD_MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];

pub fn codec_name(codec: u8) -> &'static str {
    match codec {
        RAW => "RAW",
        RLE => "RLE",
        LZ4 => "LZ4",
        ZSTD => "ZSTD",
        _ => "UNKNOWN",
    }
}

fn invalid_data(msg: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

/// Encode using simple `(run: u8, value: u8)` RLE pairs.
pub fn encode_rle(chunk: &[u8]) -> Vec<u8> {
    let mut encoded = Vec::with_capacity(chunk.len().min(4096));
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
    encoded
}

/// Decode bounded RLE `(run: u8, value: u8)` stream into `expected` bytes.
pub fn decode_rle(bytes: &[u8], expected: usize) -> io::Result<Vec<u8>> {
    if !bytes.len().is_multiple_of(2) {
        return Err(invalid_data("invalid RLE payload"));
    }
    let mut output = Vec::with_capacity(expected);
    for pair in bytes.as_chunks::<2>().0 {
        let run = pair[0] as usize;
        if run == 0 || output.len() + run > expected {
            return Err(invalid_data("RLE exceeds expected size"));
        }
        output.extend(std::iter::repeat_n(pair[1], run));
    }
    if output.len() != expected {
        return Err(invalid_data("decoded size mismatch"));
    }
    Ok(output)
}

#[inline]
fn hash4(bytes: &[u8]) -> usize {
    let v = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    ((v.wrapping_mul(2654435761)) >> 20) as usize
}

fn push_lz4_length(out: &mut Vec<u8>, mut remaining: usize) {
    while remaining >= 255 {
        out.push(255);
        remaining -= 255;
    }
    out.push(remaining as u8);
}

/// Compress a chunk using the standard LZ4 block format.
pub fn encode_lz4(chunk: &[u8]) -> Vec<u8> {
    if chunk.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(chunk.len());
    if chunk.len() < 16 {
        let lit_len = chunk.len();
        let token = (lit_len.min(15) as u8) << 4;
        out.push(token);
        if lit_len >= 15 {
            push_lz4_length(&mut out, lit_len - 15);
        }
        out.extend_from_slice(chunk);
        return out;
    }

    let mut table = [0u32; 4096];
    let mut anchor = 0usize;
    let mut ip = 0usize;
    let limit = chunk.len().saturating_sub(12);

    while ip < limit {
        let h = hash4(&chunk[ip..ip + 4]);
        let ref_pos = table[h] as usize;
        table[h] = ip as u32;

        let offset = ip.saturating_sub(ref_pos);
        if offset > 0
            && offset <= u16::MAX as usize
            && chunk[ref_pos..ref_pos + 4] == chunk[ip..ip + 4]
        {
            let lit_len = ip - anchor;
            let mut match_len = 4usize;
            let max_match = chunk.len() - ip;
            while match_len < max_match && chunk[ref_pos + match_len] == chunk[ip + match_len] {
                match_len += 1;
            }

            let lit_nib = lit_len.min(15) as u8;
            let match_nib = (match_len - 4).min(15) as u8;
            out.push((lit_nib << 4) | match_nib);
            if lit_len >= 15 {
                push_lz4_length(&mut out, lit_len - 15);
            }
            out.extend_from_slice(&chunk[anchor..ip]);
            out.extend_from_slice(&(offset as u16).to_le_bytes());
            if match_len - 4 >= 15 {
                push_lz4_length(&mut out, match_len - 4 - 15);
            }

            ip += match_len;
            anchor = ip;
            if ip < limit {
                table[hash4(&chunk[ip - 2..ip + 2])] = (ip - 2) as u32;
            }
        } else {
            ip += 1 + ((ip - anchor) >> 6);
        }
    }

    let last_lits = chunk.len() - anchor;
    let lit_nib = last_lits.min(15) as u8;
    out.push(lit_nib << 4);
    if last_lits >= 15 {
        push_lz4_length(&mut out, last_lits - 15);
    }
    out.extend_from_slice(&chunk[anchor..]);
    out
}

/// Decompress a standard LZ4 block with strict bounds enforcement.
pub fn decode_lz4(bytes: &[u8], expected: usize) -> io::Result<Vec<u8>> {
    if expected == 0 {
        if bytes.is_empty() {
            return Ok(Vec::new());
        }
        return Err(invalid_data("non-empty LZ4 payload for empty output"));
    }
    let mut out = Vec::with_capacity(expected);
    let mut pos = 0usize;

    while pos < bytes.len() {
        let token = bytes[pos];
        pos += 1;

        let mut lit_len = (token >> 4) as usize;
        if lit_len == 15 {
            loop {
                if pos >= bytes.len() {
                    return Err(invalid_data("truncated LZ4 literal length"));
                }
                let ext = bytes[pos] as usize;
                pos += 1;
                lit_len = lit_len
                    .checked_add(ext)
                    .ok_or_else(|| invalid_data("LZ4 literal length overflow"))?;
                if out.len() + lit_len > expected {
                    return Err(invalid_data("LZ4 literal exceeds expected size"));
                }
                if ext != 255 {
                    break;
                }
            }
        }
        if pos + lit_len > bytes.len() || out.len() + lit_len > expected {
            return Err(invalid_data("LZ4 literal out of bounds"));
        }
        out.extend_from_slice(&bytes[pos..pos + lit_len]);
        pos += lit_len;

        if pos == bytes.len() {
            break;
        }
        if pos + 2 > bytes.len() {
            return Err(invalid_data("truncated LZ4 match offset"));
        }
        let offset = u16::from_le_bytes([bytes[pos], bytes[pos + 1]]) as usize;
        pos += 2;
        if offset == 0 || offset > out.len() {
            return Err(invalid_data("invalid LZ4 match offset"));
        }

        let mut match_len = ((token & 0x0F) as usize) + 4;
        if (token & 0x0F) == 15 {
            loop {
                if pos >= bytes.len() {
                    return Err(invalid_data("truncated LZ4 match length"));
                }
                let ext = bytes[pos] as usize;
                pos += 1;
                match_len = match_len
                    .checked_add(ext)
                    .ok_or_else(|| invalid_data("LZ4 match length overflow"))?;
                if out.len() + match_len > expected {
                    return Err(invalid_data("LZ4 match exceeds expected size"));
                }
                if ext != 255 {
                    break;
                }
            }
        }
        if out.len() + match_len > expected {
            return Err(invalid_data("LZ4 match exceeds expected size"));
        }

        let start = out.len() - offset;
        if offset >= match_len {
            out.extend_from_within(start..start + match_len);
        } else {
            for _ in 0..match_len {
                let b = out[out.len() - offset];
                out.push(b);
            }
        }
    }

    if out.len() != expected {
        return Err(invalid_data("LZ4 decoded size mismatch"));
    }
    Ok(out)
}

fn transpose_stride(chunk: &[u8], stride: usize) -> Vec<u8> {
    if stride <= 1 || chunk.len() <= stride {
        return chunk.to_vec();
    }
    let mut out = Vec::with_capacity(chunk.len());
    for lane in 0..stride {
        let mut idx = lane;
        while idx < chunk.len() {
            out.push(chunk[idx]);
            idx += stride;
        }
    }
    out
}

fn untranspose_stride(permuted: &[u8], stride: usize) -> Vec<u8> {
    let n = permuted.len();
    if stride <= 1 || n <= stride {
        return permuted.to_vec();
    }
    let mut out = vec![0u8; n];
    let mut src = 0usize;
    for lane in 0..stride {
        let mut dst = lane;
        while dst < n {
            out[dst] = permuted[src];
            src += 1;
            dst += stride;
        }
    }
    out
}

fn select_best_stride(chunk: &[u8]) -> u8 {
    let sample_len = chunk.len().min(1024);
    if sample_len < 32 {
        return 1;
    }
    let sample = &chunk[..sample_len];
    let mut adj_same = 0usize;
    let mut s2_same = 0usize;
    let mut s4_same = 0usize;
    for i in 0..(sample_len - 4) {
        if sample[i] == sample[i + 1] {
            adj_same += 1;
        }
        if sample[i] == sample[i + 2] {
            s2_same += 1;
        }
        if sample[i] == sample[i + 4] {
            s4_same += 1;
        }
    }
    if s2_same > adj_same.saturating_add(64) && s2_same >= s4_same {
        2
    } else if s4_same > adj_same.saturating_add(64) {
        4
    } else {
        1
    }
}

/// Compress a chunk into a ZSTD-framed container (`ZSTD_MAGIC` + stride descriptor +
/// LZ77 + second-stage RLE entropy pass for structured / tensor-like payloads).
pub fn encode_zstd(chunk: &[u8]) -> Vec<u8> {
    let best_stride = select_best_stride(chunk);
    let candidate_strides: &[u8] = if best_stride == 1 {
        &[1, 2]
    } else {
        &[best_stride, 1]
    };

    let mut best_header = 0u8;
    let mut best_payload: Option<Vec<u8>> = None;

    for &stride in candidate_strides {
        let staged = transpose_stride(chunk, stride as usize);
        let lz4_stage1 = encode_lz4(&staged);
        // Only run RLE directly on `staged` if the first 256 bytes exhibit run correlation
        let rle_stage1 = if staged.len() >= 16
            && staged[..staged.len().min(256)]
                .windows(2)
                .filter(|w| w[0] == w[1])
                .count()
                >= 64
        {
            encode_rle(&staged)
        } else {
            Vec::new()
        };

        let (stage_mode, payload) = if !rle_stage1.is_empty() && rle_stage1.len() < lz4_stage1.len()
        {
            let rle_then_lz4 = encode_lz4(&rle_stage1);
            if rle_then_lz4.len() < rle_stage1.len() {
                (2u8, rle_then_lz4)
            } else {
                (1u8, rle_stage1)
            }
        } else if lz4_stage1.len() < staged.len() {
            let lz4_then_rle = encode_rle(&lz4_stage1);
            if lz4_then_rle.len() < lz4_stage1.len() {
                (3u8, lz4_then_rle)
            } else {
                (0u8, lz4_stage1)
            }
        } else {
            (0u8, lz4_stage1)
        };

        if best_payload
            .as_ref()
            .is_none_or(|prev| payload.len() < prev.len())
        {
            best_header = ((stride & 0x07) << 2) | (stage_mode & 0x03);
            best_payload = Some(payload);
        }
    }

    let payload = best_payload.unwrap_or_default();
    let mut out = Vec::with_capacity(8 + payload.len());
    out.extend_from_slice(&ZSTD_MAGIC);
    out.push(best_header);
    let len_u24 = chunk.len() as u32;
    out.push((len_u24 & 0xFF) as u8);
    out.push(((len_u24 >> 8) & 0xFF) as u8);
    out.push(((len_u24 >> 16) & 0xFF) as u8);
    out.extend_from_slice(&payload);
    out
}

/// Decompress a ZSTD-framed chunk with strict header and bounds verification.
pub fn decode_zstd(bytes: &[u8], expected: usize) -> io::Result<Vec<u8>> {
    if bytes.len() < 8 {
        return Err(invalid_data("truncated ZSTD frame header"));
    }
    if bytes[0..4] != ZSTD_MAGIC {
        return Err(invalid_data("invalid ZSTD magic header"));
    }
    let header_byte = bytes[4];
    if (header_byte & 0xE0) != 0 {
        return Err(invalid_data("unsupported ZSTD frame flags"));
    }
    let stride = ((header_byte >> 2) & 0x07) as usize;
    if !matches!(stride, 1 | 2 | 4) {
        return Err(invalid_data("invalid ZSTD lane stride"));
    }
    let stage_mode = header_byte & 0x03;
    let declared_len =
        (bytes[5] as usize) | ((bytes[6] as usize) << 8) | ((bytes[7] as usize) << 16);
    if declared_len != expected || expected > CHUNK_BYTES {
        return Err(invalid_data("ZSTD frame size mismatch"));
    }
    let payload = &bytes[8..];
    let staged = match stage_mode {
        0 => decode_lz4(payload, expected)?,
        1 => decode_rle(payload, expected)?,
        2 => {
            let rle_bytes = decode_lz4_bounded(payload, MAX_ENCODED_BYTES)?;
            decode_rle(&rle_bytes, expected)?
        }
        3 => {
            let lz4_bytes = decode_rle_bounded(payload, MAX_ENCODED_BYTES)?;
            decode_lz4(&lz4_bytes, expected)?
        }
        _ => return Err(invalid_data("unknown ZSTD stage mode")),
    };
    Ok(untranspose_stride(&staged, stride))
}

fn decode_rle_bounded(bytes: &[u8], max_out: usize) -> io::Result<Vec<u8>> {
    if !bytes.len().is_multiple_of(2) {
        return Err(invalid_data("invalid RLE payload"));
    }
    let mut output = Vec::with_capacity(max_out.min(CHUNK_BYTES));
    for pair in bytes.as_chunks::<2>().0 {
        let run = pair[0] as usize;
        if run == 0 || output.len() + run > max_out {
            return Err(invalid_data("RLE exceeds bound"));
        }
        output.extend(std::iter::repeat_n(pair[1], run));
    }
    Ok(output)
}

fn decode_lz4_bounded(bytes: &[u8], max_out: usize) -> io::Result<Vec<u8>> {
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    let mut out = Vec::with_capacity(max_out.min(CHUNK_BYTES));
    let mut pos = 0usize;
    while pos < bytes.len() {
        let token = bytes[pos];
        pos += 1;
        let mut lit_len = (token >> 4) as usize;
        if lit_len == 15 {
            loop {
                if pos >= bytes.len() {
                    return Err(invalid_data("truncated LZ4 literal length"));
                }
                let ext = bytes[pos] as usize;
                pos += 1;
                lit_len = lit_len
                    .checked_add(ext)
                    .ok_or_else(|| invalid_data("LZ4 literal length overflow"))?;
                if out.len() + lit_len > max_out {
                    return Err(invalid_data("LZ4 literal exceeds bound"));
                }
                if ext != 255 {
                    break;
                }
            }
        }
        if pos + lit_len > bytes.len() || out.len() + lit_len > max_out {
            return Err(invalid_data("LZ4 literal out of bounds"));
        }
        out.extend_from_slice(&bytes[pos..pos + lit_len]);
        pos += lit_len;
        if pos == bytes.len() {
            break;
        }
        if pos + 2 > bytes.len() {
            return Err(invalid_data("truncated LZ4 match offset"));
        }
        let offset = u16::from_le_bytes([bytes[pos], bytes[pos + 1]]) as usize;
        pos += 2;
        if offset == 0 || offset > out.len() {
            return Err(invalid_data("invalid LZ4 match offset"));
        }
        let mut match_len = ((token & 0x0F) as usize) + 4;
        if (token & 0x0F) == 15 {
            loop {
                if pos >= bytes.len() {
                    return Err(invalid_data("truncated LZ4 match length"));
                }
                let ext = bytes[pos] as usize;
                pos += 1;
                match_len = match_len
                    .checked_add(ext)
                    .ok_or_else(|| invalid_data("LZ4 match length overflow"))?;
                if out.len() + match_len > max_out {
                    return Err(invalid_data("LZ4 match exceeds bound"));
                }
                if ext != 255 {
                    break;
                }
            }
        }
        if out.len() + match_len > max_out {
            return Err(invalid_data("LZ4 match exceeds bound"));
        }
        let start = out.len() - offset;
        if offset >= match_len {
            out.extend_from_within(start..start + match_len);
        } else {
            for _ in 0..match_len {
                let b = out[out.len() - offset];
                out.push(b);
            }
        }
    }
    Ok(out)
}

/// Encode a chunk with a specific codec ID (`RAW`, `RLE`, `LZ4`, `ZSTD`).
pub fn encode_with_codec(codec: u8, chunk: &[u8]) -> io::Result<Vec<u8>> {
    match codec {
        RAW => Ok(chunk.to_vec()),
        RLE => Ok(encode_rle(chunk)),
        LZ4 => Ok(encode_lz4(chunk)),
        ZSTD => Ok(encode_zstd(chunk)),
        _ => Err(invalid_data("unknown codec")),
    }
}

/// Decode a chunk encoded with `RAW`, `RLE`, `LZ4`, or `ZSTD`, enforcing bounds.
pub fn decode_chunk(codec: u8, bytes: &[u8], expected: usize) -> io::Result<Vec<u8>> {
    if expected > CHUNK_BYTES || bytes.len() > MAX_ENCODED_BYTES {
        return Err(invalid_data("chunk exceeds limit"));
    }
    let decoded = match codec {
        RAW => bytes.to_vec(),
        RLE => decode_rle(bytes, expected)?,
        LZ4 => decode_lz4(bytes, expected)?,
        ZSTD => decode_zstd(bytes, expected)?,
        _ => return Err(invalid_data("unknown codec")),
    };
    if decoded.len() != expected {
        return Err(invalid_data("decoded size mismatch"));
    }
    Ok(decoded)
}

/// Policy parameters for evaluating total transfer cost (`CPU ms + wire transfer ms`).
#[derive(Debug, Clone, Copy)]
pub struct CodecCostPolicy {
    /// Effective network link bandwidth in Megabits per second (`1e6` bits/s).
    pub bandwidth_mbps: f64,
    /// Minimum byte reduction required over `RAW` before considering any compressed codec.
    pub min_byte_savings: usize,
}

impl Default for CodecCostPolicy {
    fn default() -> Self {
        Self {
            // Conservative 250 Mbps effective link default (typical LAN/tunnel bottleneck)
            bandwidth_mbps: 250.0,
            min_byte_savings: 32,
        }
    }
}

impl CodecCostPolicy {
    pub fn wire_transfer_ms(&self, wire_bytes: usize) -> f64 {
        let bw = if self.bandwidth_mbps.is_finite() && self.bandwidth_mbps > 0.0 {
            self.bandwidth_mbps
        } else {
            250.0
        };
        (wire_bytes as f64 * 8.0) / (bw * 1_000.0)
    }
}

/// Measured cost breakdown for a single codec candidate on a given chunk.
#[derive(Debug, Clone, PartialEq)]
pub struct CodecCandidateMetrics {
    pub codec: u8,
    pub codec_name: &'static str,
    pub raw_bytes: usize,
    pub encoded_bytes: usize,
    pub wire_bytes: usize,
    pub encode_cpu_ms: f64,
    pub decode_cpu_ms: f64,
    pub total_cpu_ms: f64,
    pub wire_transfer_ms: f64,
    pub total_cost_ms: f64,
}

/// Result of adaptive codec evaluation.
#[derive(Debug, Clone)]
pub struct AdaptiveCodecChoice {
    pub codec: u8,
    pub encoded: Vec<u8>,
    pub selected_metrics: CodecCandidateMetrics,
    pub candidates: Vec<CodecCandidateMetrics>,
}

/// Measure a single codec candidate across `iterations` runs, returning the encoded payload and median/mean CPU + wire cost.
pub fn evaluate_codec_candidate(
    codec: u8,
    chunk: &[u8],
    policy: CodecCostPolicy,
    iterations: usize,
) -> io::Result<(Vec<u8>, CodecCandidateMetrics)> {
    let iters = iterations.max(1);
    let mut encoded = Vec::new();

    let t_enc = Instant::now();
    for _ in 0..iters {
        encoded = encode_with_codec(codec, chunk)?;
    }
    let encode_cpu_ms = (t_enc.elapsed().as_secs_f64() * 1_000.0) / (iters as f64);

    let decode_cpu_ms = if codec == RAW {
        0.0
    } else if encoded.len() <= MAX_ENCODED_BYTES {
        let t_dec = Instant::now();
        for _ in 0..iters {
            let decoded = decode_chunk(codec, &encoded, chunk.len())?;
            debug_assert_eq!(decoded.len(), chunk.len());
        }
        (t_dec.elapsed().as_secs_f64() * 1_000.0) / (iters as f64)
    } else {
        // Exceeds MAX_ENCODED_BYTES; cannot be sent as compressed
        f64::INFINITY
    };

    let wire_bytes = encoded
        .len()
        .saturating_add(WireBreakdown::CHUNK_FRAME_HEADER_BYTES as usize);
    let wire_transfer_ms = policy.wire_transfer_ms(wire_bytes);
    let total_cpu_ms = if codec == RAW {
        0.0
    } else {
        encode_cpu_ms + decode_cpu_ms
    };
    let total_cost_ms = total_cpu_ms + wire_transfer_ms;

    Ok((
        encoded.clone(),
        CodecCandidateMetrics {
            codec,
            codec_name: codec_name(codec),
            raw_bytes: chunk.len(),
            encoded_bytes: encoded.len(),
            wire_bytes,
            encode_cpu_ms,
            decode_cpu_ms,
            total_cpu_ms,
            wire_transfer_ms,
            total_cost_ms,
        },
    ))
}

/// Benchmark all 4 codecs (`RAW`, `RLE`, `LZ4`, `ZSTD`) on `chunk` and select the codec
/// with the lowest total cost (`total_cpu_ms + wire_transfer_ms`), choosing a compressed
/// codec only when both byte count and total cost are strictly lower than `RAW`.
pub fn select_codec_measured(
    chunk: &[u8],
    policy: CodecCostPolicy,
    iterations: usize,
) -> AdaptiveCodecChoice {
    let (_, raw_metrics) = evaluate_codec_candidate(RAW, chunk, policy, iterations)
        .expect("RAW evaluation cannot fail");
    let raw_cost = raw_metrics.total_cost_ms;

    let mut best_codec = RAW;
    let mut best_encoded = chunk.to_vec();
    let mut best_metrics = raw_metrics.clone();
    let mut candidates = vec![raw_metrics];

    for &codec in &[RLE, LZ4, ZSTD] {
        if let Ok((enc, metrics)) = evaluate_codec_candidate(codec, chunk, policy, iterations) {
            let saves_bytes = enc.len().saturating_add(policy.min_byte_savings) < chunk.len()
                && enc.len() <= MAX_ENCODED_BYTES;
            let beats_raw_cost = metrics.total_cost_ms < raw_cost;
            if saves_bytes && beats_raw_cost && metrics.total_cost_ms < best_metrics.total_cost_ms {
                best_codec = codec;
                best_encoded = enc;
                best_metrics = metrics.clone();
            }
            candidates.push(metrics);
        }
    }

    AdaptiveCodecChoice {
        codec: best_codec,
        encoded: best_encoded,
        selected_metrics: best_metrics,
        candidates,
    }
}

/// Fast production adaptive encoder used by `encode_chunk`.
///
/// 1. Checks `RLE` first for ultra-sparse uniform chunks (where `RLE` achieves >95% compression
///    in a single linear pass).
/// 2. Otherwise evaluates `LZ4` and `ZSTD` (when strided correlation is detected) against `RAW`
///    using `CodecCostPolicy`, returning a compressed codec only when total cost (`CPU ms + wire ms`)
///    and byte size are strictly lower than `RAW`.
pub fn encode_chunk_adaptive(chunk: &[u8], policy: CodecCostPolicy) -> (u8, Vec<u8>) {
    let raw_wire_bytes = chunk
        .len()
        .saturating_add(WireBreakdown::CHUNK_FRAME_HEADER_BYTES as usize);
    let raw_cost_ms = policy.wire_transfer_ms(raw_wire_bytes);

    let t_rle = Instant::now();
    let rle_enc = encode_rle(chunk);
    let rle_cpu_ms = t_rle.elapsed().as_secs_f64() * 1_000.0 * 1.5; // include estimated decode
    let rle_saves = rle_enc.len().saturating_add(policy.min_byte_savings) < chunk.len()
        && rle_enc.len() <= MAX_ENCODED_BYTES;
    let rle_cost_ms = rle_cpu_ms
        + policy.wire_transfer_ms(
            rle_enc
                .len()
                .saturating_add(WireBreakdown::CHUNK_FRAME_HEADER_BYTES as usize),
        );

    // Fast path for sparse/uniform chunks (e.g. >=95% reduction via RLE)
    if rle_saves && rle_enc.len() <= chunk.len() / 20 && rle_cost_ms < raw_cost_ms {
        return (RLE, rle_enc);
    }

    let mut best_codec = RAW;
    let mut best_encoded: Option<Vec<u8>> = None;
    let mut best_cost_ms = raw_cost_ms;

    if rle_saves && rle_cost_ms < best_cost_ms {
        best_codec = RLE;
        best_encoded = Some(rle_enc);
        best_cost_ms = rle_cost_ms;
    }

    let t_lz4 = Instant::now();
    let lz4_enc = encode_lz4(chunk);
    let lz4_cpu_ms = t_lz4.elapsed().as_secs_f64() * 1_000.0 * 1.35;
    let lz4_saves = lz4_enc.len().saturating_add(policy.min_byte_savings) < chunk.len()
        && lz4_enc.len() <= MAX_ENCODED_BYTES;
    let lz4_cost_ms = lz4_cpu_ms
        + policy.wire_transfer_ms(
            lz4_enc
                .len()
                .saturating_add(WireBreakdown::CHUNK_FRAME_HEADER_BYTES as usize),
        );
    if lz4_saves && lz4_cost_ms < best_cost_ms {
        best_codec = LZ4;
        best_encoded = Some(lz4_enc);
        best_cost_ms = lz4_cost_ms;
    }

    // Evaluate ZSTD when stride correlation exists or when LZ4 found partial structure
    if select_best_stride(chunk) > 1 || lz4_saves {
        let t_zstd = Instant::now();
        let zstd_enc = encode_zstd(chunk);
        let zstd_cpu_ms = t_zstd.elapsed().as_secs_f64() * 1_000.0 * 1.45;
        let zstd_saves = zstd_enc.len().saturating_add(policy.min_byte_savings) < chunk.len()
            && zstd_enc.len() <= MAX_ENCODED_BYTES;
        let zstd_cost_ms = zstd_cpu_ms
            + policy.wire_transfer_ms(
                zstd_enc
                    .len()
                    .saturating_add(WireBreakdown::CHUNK_FRAME_HEADER_BYTES as usize),
            );
        if zstd_saves && zstd_cost_ms < best_cost_ms {
            best_codec = ZSTD;
            best_encoded = Some(zstd_enc);
        }
    }

    match best_encoded {
        Some(enc) => (best_codec, enc),
        None => (RAW, chunk.to_vec()),
    }
}

/// Generate deterministic incompressible synthetic chunk data (`CHUNK_BYTES` bytes).
pub fn synthetic_incompressible_chunk() -> Vec<u8> {
    let mut state = 0x243F_6A88_85A3_08D3u64;
    (0..CHUNK_BYTES)
        .map(|_| {
            // SplitMix64 PRNG
            state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            (z ^ (z >> 31)) as u8
        })
        .collect()
}

/// Generate deterministic sparse synthetic chunk data (`CHUNK_BYTES` bytes with isolated non-zero values).
pub fn synthetic_sparse_chunk() -> Vec<u8> {
    let mut data = vec![0u8; CHUNK_BYTES];
    data[0] = 1;
    data[CHUNK_BYTES / 2] = 7;
    data[CHUNK_BYTES - 1] = 2;
    data
}

/// Generate deterministic tensor-like synthetic chunk data (BF16/FP16 activation tensor with
/// narrow exponent range `0x3B..=0x3C` at stride 2 and non-periodic low mantissa bytes).
pub fn synthetic_tensor_like_chunk() -> Vec<u8> {
    let mut data = Vec::with_capacity(CHUNK_BYTES);
    let mut state = 0x1357_9BDF_2468_ACE0u64;
    for i in 0..(CHUNK_BYTES / 2) {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        // Quantized BF16 mantissa with high-frequency per-element variation
        let mantissa = (((state >> 32) as u8) & 0xFC) ^ ((i % 7) as u8);
        // Channel-clustered BF16 exponent (e.g. 0x3C for [1.0, 2.0), 0x3B for [0.5, 1.0))
        let exponent = if (i / 64).is_multiple_of(3) {
            0x3Bu8
        } else {
            0x3Cu8
        };
        data.push(mantissa);
        data.push(exponent);
    }
    data
}
