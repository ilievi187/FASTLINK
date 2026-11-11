//! FASTLINK v4 Core (`FL4-IMPL`): Persistent remote tensor residency, adaptive codec + chunk
//! selection, per-chunk BLAKE3 delta encoding, coalesced network batching, strict invalidation,
//! and wire-byte accounting.
//!
//! Core v4 principle:
//! - Keep reusable tensors resident on the owning worker.
//! - Do NOT re-send a tensor that the receiver already holds in a valid state (`SyncDecision::Reuse` -> 0 wire bytes).
//! - Transfer only what changed (`SyncDecision::Delta`) or is genuinely new (`SyncDecision::Full`),
//!   coalesced into a single transport batch frame (`BatchedSyncFrame`) per RPC step.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::codec::{
    codec_name, decode_v4_chunk, encode_lz4, encode_rle, encode_with_codec, encode_zstd,
    select_best_stride, CodecCostPolicy, LZ4, RAW, RLE, V4_MAX_CHUNK_BYTES, V4_MAX_ENCODED_BYTES,
    ZSTD,
};
use crate::{
    digest, read_u16, read_u32, read_u64, read_u8, validate_connect_address,
    validate_listen_address, write_u16, write_u32, write_u64, write_u8, Digest, WireBreakdown,
    CHUNK_BYTES,
};

/// Magic header for coalesced v4 batched transport frames (`"FLB4"`).
pub const V4_BATCH_MAGIC: &[u8; 4] = b"FLB4";
/// Wire version for v4 batched transport frames.
pub const V4_BATCH_VERSION: u16 = 4;

/// Mode tag inside a `BatchedTensorOp`.
pub const OP_REUSE: u8 = 0;
pub const OP_FULL: u8 = 1;
pub const OP_DELTA: u8 = 2;

/// Measured per-MiB CPU encode/decode costs from `docs/v4/TRANSFER_ANALYSIS.md` (`-O3` on host CPU).
/// Used alongside live timing so adaptive selection is deterministic and grounded in measured data.
pub const MEASURED_RAW_ENCODE_MS_PER_MIB: f64 = 0.020;
pub const MEASURED_RAW_DECODE_MS_PER_MIB: f64 = 0.019;
pub const MEASURED_RLE_ENCODE_MS_PER_MIB: f64 = 0.383;
pub const MEASURED_RLE_DECODE_MS_PER_MIB: f64 = 0.016;
pub const MEASURED_LZ4_ENCODE_MS_PER_MIB: f64 = 0.256;
pub const MEASURED_LZ4_DECODE_MS_PER_MIB: f64 = 0.451;
pub const MEASURED_ZSTD_ENCODE_MS_PER_MIB: f64 = 0.785;
pub const MEASURED_ZSTD_DECODE_MS_PER_MIB: f64 = 1.180;

/// Per-chunk framing overhead in `FLR3` (`codec:1 + flags:1 + orig_len:4 + enc_len:4 = 10 B`).
pub const V4_CHUNK_HEADER_BYTES: usize = 10;
/// Additional per-chunk index + BLAKE3 digest overhead in `DELTA` / `BatchedTensorOp` (`idx:4 + hash:32 = 36 B`).
pub const V4_DELTA_INDEX_HASH_BYTES: usize = 36;
/// Outer frame header for a tensor op (`mode:1 + ver:8 + total:8 + chunk_bytes:4 + count:4 + hash:32 = 57 B`).
pub const V4_TENSOR_OP_FIXED_BYTES: usize = 57;
/// Per-batch transport header (`magic:4 + ver:2 + batch_id:8 + op_count:4 = 18 B`) + ACK (`status:1 + applied:4 = 5 B`).
pub const V4_BATCH_FRAME_OVERHEAD_BYTES: usize = 23;

// Experimental transport admission limits. A peer must never be able to reserve
// unbounded memory just by declaring a large batch or tensor in its header.
const MAX_BATCH_OPS: usize = 4_096;
const MAX_BATCH_UPDATES: usize = 65_536;
const MAX_BATCH_TENSOR_BYTES: u64 = 512 * 1024 * 1024;
const MAX_BATCH_IDENTIFIER_BYTES: usize = 4_096;

fn invalid(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

fn now_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Workload classification derived from `docs/v4/TRANSFER_ANALYSIS.md`:
/// - `IncompressibleRandom` -> `RAW` (`64K-256K` chunks; framing dominates, big chunks minimize overhead)
/// - `SparseRle` -> `RLE` (`64K` chunks)
/// - `Structured` -> `LZ4` (`16K` chunks)
/// - `DenseNumericTensor` -> `ZSTD` (`4K-16K` chunks)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkloadClass {
    IncompressibleRandom,
    SparseRle,
    Structured,
    DenseNumericTensor,
}

impl WorkloadClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::IncompressibleRandom => "incompressible-random",
            Self::SparseRle => "sparse/RLE-friendly",
            Self::Structured => "structured",
            Self::DenseNumericTensor => "dense-numeric-tensor",
        }
    }

    /// Target logical chunk size in bytes per `docs/v4/TRANSFER_ANALYSIS.md` Task 2 sweep.
    pub fn recommended_chunk_bytes(self, total_bytes: usize) -> usize {
        match self {
            Self::IncompressibleRandom => {
                if total_bytes >= 256 * 1024 {
                    256 * 1024
                } else {
                    64 * 1024
                }
            }
            Self::SparseRle => 64 * 1024,
            Self::Structured => 16 * 1024,
            Self::DenseNumericTensor => {
                if total_bytes <= 16 * 1024 {
                    4 * 1024
                } else {
                    16 * 1024
                }
            }
        }
    }
}

/// Classify a tensor buffer into one of the four `TRANSFER_ANALYSIS.md` workload classes
/// by inspecting a bounded sample (`<= 16 KiB`).
pub fn classify_tensor_workload(bytes: &[u8]) -> WorkloadClass {
    if bytes.is_empty() {
        return WorkloadClass::SparseRle;
    }
    let sample_len = bytes.len().min(16 * 1024);
    let sample = &bytes[..sample_len];

    // 1. Check sparse / RLE-friendly (high adjacent run compression)
    let rle_sample = encode_rle(sample);
    if rle_sample.len().saturating_add(32) <= sample.len() / 4 {
        return WorkloadClass::SparseRle;
    }

    // 2. Check strided numeric tensor correlation (BF16/FP16 stride 2 or FP32 stride 4)
    let stride = select_best_stride(sample);
    if stride > 1 {
        let zstd_sample = encode_zstd(sample);
        if zstd_sample.len().saturating_add(32) < sample.len() {
            return WorkloadClass::DenseNumericTensor;
        }
    }

    // 3. Check structured (non-strided LZ4-compressible patterns)
    let lz4_sample = encode_lz4(sample);
    if lz4_sample.len().saturating_add(32) < (sample.len() * 4) / 5 {
        return WorkloadClass::Structured;
    }

    // 4. Default: incompressible / pseudo-random quantized blocks -> RAW with 64K-256K chunks
    WorkloadClass::IncompressibleRandom
}

/// Result of the v4 adaptive codec + chunk-size evaluation.
#[derive(Debug, Clone, PartialEq)]
pub struct V4AdaptivePlan {
    pub workload_class: WorkloadClass,
    pub codec: u8,
    pub codec_name: &'static str,
    pub chunk_bytes: usize,
    pub total_chunks: u32,
    pub raw_bytes: usize,
    pub encoded_payload_bytes: usize,
    pub wire_bytes: usize,
    pub encode_ms: f64,
    pub decode_ms: f64,
    pub wire_ms: f64,
    pub sync_overhead_ms: f64,
    pub total_cost_ms: f64,
    pub raw_total_cost_ms: f64,
}

fn calibrated_cpu_ms(codec: u8, raw_bytes: usize) -> (f64, f64) {
    let mib = (raw_bytes as f64) / (1024.0 * 1024.0);
    match codec {
        RAW => (
            mib * MEASURED_RAW_ENCODE_MS_PER_MIB,
            mib * MEASURED_RAW_DECODE_MS_PER_MIB,
        ),
        RLE => (
            mib * MEASURED_RLE_ENCODE_MS_PER_MIB,
            mib * MEASURED_RLE_DECODE_MS_PER_MIB,
        ),
        LZ4 => (
            mib * MEASURED_LZ4_ENCODE_MS_PER_MIB,
            mib * MEASURED_LZ4_DECODE_MS_PER_MIB,
        ),
        ZSTD => (
            mib * MEASURED_ZSTD_ENCODE_MS_PER_MIB,
            mib * MEASURED_ZSTD_DECODE_MS_PER_MIB,
        ),
        _ => (f64::INFINITY, f64::INFINITY),
    }
}

/// Select the optimal `(codec, chunk_bytes)` plan for `bytes` using the measured cost model
/// from `docs/v4/TRANSFER_ANALYSIS.md`:
/// `total_cost_ms = encode_ms + wire_ms + decode_ms + sync_overhead_ms`.
pub fn select_v4_adaptive_plan(bytes: &[u8], policy: CodecCostPolicy) -> V4AdaptivePlan {
    let workload_class = classify_tensor_workload(bytes);
    let chunk_bytes = workload_class
        .recommended_chunk_bytes(bytes.len())
        .clamp(512, V4_MAX_CHUNK_BYTES);
    let total_chunks = if bytes.is_empty() {
        0
    } else {
        bytes.len().div_ceil(chunk_bytes) as u32
    };

    // Probe up to 64 KiB to measure actual compression ratios across candidate codecs
    let probe_len = bytes.len().min(64 * 1024);
    let probe = &bytes[..probe_len];
    let scale = if probe_len > 0 {
        (bytes.len() as f64) / (probe_len as f64)
    } else {
        1.0
    };

    let raw_framing = V4_TENSOR_OP_FIXED_BYTES
        + (total_chunks as usize) * (V4_CHUNK_HEADER_BYTES + V4_DELTA_INDEX_HASH_BYTES);
    let raw_wire_bytes = bytes.len().saturating_add(raw_framing);
    let (raw_enc_ms, raw_dec_ms) = calibrated_cpu_ms(RAW, bytes.len());
    let raw_wire_ms = policy.wire_transfer_ms(raw_wire_bytes);
    // Per-chunk framing/dispatch sync overhead (~0.0005 ms per chunk header)
    let sync_overhead_ms = (total_chunks as f64) * 0.0005;
    let raw_total_cost_ms = raw_enc_ms + raw_dec_ms + raw_wire_ms + sync_overhead_ms;

    // Candidate order guided by workload class while strictly gated by total_cost_ms < raw_total_cost_ms
    let candidates: &[u8] = match workload_class {
        WorkloadClass::IncompressibleRandom => &[RAW],
        WorkloadClass::SparseRle => &[RLE, LZ4],
        WorkloadClass::Structured => &[LZ4, RLE],
        WorkloadClass::DenseNumericTensor => &[ZSTD, LZ4],
    };

    let mut best_codec = RAW;
    let mut best_encoded_bytes = bytes.len();
    let mut best_wire_bytes = raw_wire_bytes;
    let mut best_enc_ms = raw_enc_ms;
    let mut best_dec_ms = raw_dec_ms;
    let mut best_wire_ms = raw_wire_ms;
    let mut best_total_cost = raw_total_cost_ms;

    for &codec in candidates {
        if codec == RAW || probe.is_empty() {
            continue;
        }
        // Encode probe in chunks of `chunk_bytes` to capture exact chunk-size compression behavior
        let mut probe_encoded = 0usize;
        for sub in probe.chunks(chunk_bytes) {
            if let Ok(enc) = encode_with_codec(codec, sub) {
                probe_encoded += enc.len().min(sub.len());
            } else {
                probe_encoded += sub.len();
            }
        }
        let est_encoded_bytes = ((probe_encoded as f64) * scale).round() as usize;
        if est_encoded_bytes.saturating_add(policy.min_byte_savings) >= bytes.len() {
            continue;
        }
        let wire_bytes = est_encoded_bytes.saturating_add(raw_framing);
        let (mut enc_ms, dec_ms) = calibrated_cpu_ms(codec, bytes.len());
        // On strided dense numeric tensors (BF16/FP16), ZSTD's byte-lane transpose unlocks
        // 2-stage entropy coding; on non-strided structured data, LZ4 avoids transpose overhead.
        if workload_class == WorkloadClass::DenseNumericTensor && codec == LZ4 {
            enc_ms += 0.35 * ((bytes.len() as f64) / (1024.0 * 1024.0));
        }
        let wire_ms = policy.wire_transfer_ms(wire_bytes);
        let total_cost = enc_ms + dec_ms + wire_ms + sync_overhead_ms;

        if total_cost < raw_total_cost_ms && total_cost < best_total_cost {
            best_codec = codec;
            best_encoded_bytes = est_encoded_bytes;
            best_wire_bytes = wire_bytes;
            best_enc_ms = enc_ms;
            best_dec_ms = dec_ms;
            best_wire_ms = wire_ms;
            best_total_cost = total_cost;
        }
    }

    V4AdaptivePlan {
        workload_class,
        codec: best_codec,
        codec_name: codec_name(best_codec),
        chunk_bytes,
        total_chunks,
        raw_bytes: bytes.len(),
        encoded_payload_bytes: best_encoded_bytes,
        wire_bytes: best_wire_bytes,
        encode_ms: best_enc_ms,
        decode_ms: best_dec_ms,
        wire_ms: best_wire_ms,
        sync_overhead_ms,
        total_cost_ms: best_total_cost,
        raw_total_cost_ms,
    }
}

/// Compute the v3 always-send wire byte footprint for a tensor of `total_bytes` (using 64 KiB chunks + manifest).
pub fn v3_always_send_wire_bytes(id: &str, total_bytes: usize) -> u64 {
    let chunks = (total_bytes as u64).div_ceil(CHUNK_BYTES as u64) as u32;
    WireBreakdown::compute(id.len(), chunks, chunks, total_bytes as u64).expected_wire_bytes_total
}

/// Persistent tensor metadata tracked by the sender registry and receiver worker state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TensorRegistryEntry {
    pub tensor_id: String,
    pub session_id: String,
    pub content_hash: Digest,
    pub version: u64,
    pub owner: String,
    pub valid: bool,
    pub last_sync_ms: u64,
    pub chunk_bytes: usize,
    pub chunk_hashes: Vec<Digest>,
    pub total_bytes: usize,
}

/// A single encoded logical chunk update within a `Full` or `Delta` sync decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedChunkUpdate {
    pub chunk_index: u32,
    pub orig_len: u32,
    pub codec: u8,
    pub chunk_hash: Digest,
    pub encoded_bytes: Vec<u8>,
}

impl EncodedChunkUpdate {
    pub fn wire_bytes(&self) -> usize {
        V4_CHUNK_HEADER_BYTES + V4_DELTA_INDEX_HASH_BYTES + self.encoded_bytes.len()
    }
}

/// Decision returned by `sync_decision(id, bytes, known_remote_state)`.
#[derive(Debug, Clone, PartialEq)]
pub enum SyncDecision {
    /// Remote worker already holds `tensor_id` with matching `content_hash + version` and `valid == true`.
    /// Zero wire bytes are transferred!
    Reuse {
        tensor_id: String,
        content_hash: Digest,
        version: u64,
        total_chunks: u32,
        bytes_avoided: u64,
    },
    /// Tensor is new, invalidated, resized, or cheaper to replace in full than via delta.
    Full {
        tensor_id: String,
        content_hash: Digest,
        version: u64,
        workload_class: WorkloadClass,
        codec: u8,
        chunk_bytes: usize,
        total_chunks: u32,
        chunks: Vec<EncodedChunkUpdate>,
        wire_bytes: u64,
        v3_always_send_wire_bytes: u64,
        encode_ms: f64,
        decode_ms: f64,
        total_cost_ms: f64,
    },
    /// Tensor is known and valid on remote worker, but a subset of logical chunks changed
    /// and sending only the changed chunks + indices is cheaper than `Full`.
    Delta {
        tensor_id: String,
        content_hash: Digest,
        version: u64,
        workload_class: WorkloadClass,
        codec: u8,
        chunk_bytes: usize,
        total_chunks: u32,
        reused_chunks: u32,
        changed_chunks: Vec<EncodedChunkUpdate>,
        wire_bytes: u64,
        full_wire_bytes: u64,
        v3_always_send_wire_bytes: u64,
        encode_ms: f64,
        decode_ms: f64,
        total_cost_ms: f64,
        full_cost_ms: f64,
    },
}

impl SyncDecision {
    pub fn is_reuse(&self) -> bool {
        matches!(self, Self::Reuse { .. })
    }

    pub fn is_full(&self) -> bool {
        matches!(self, Self::Full { .. })
    }

    pub fn is_delta(&self) -> bool {
        matches!(self, Self::Delta { .. })
    }

    pub fn wire_bytes(&self) -> u64 {
        match self {
            Self::Reuse { .. } => 0,
            Self::Full { wire_bytes, .. } => *wire_bytes,
            Self::Delta { wire_bytes, .. } => *wire_bytes,
        }
    }

    pub fn version(&self) -> u64 {
        match self {
            Self::Reuse { version, .. }
            | Self::Full { version, .. }
            | Self::Delta { version, .. } => *version,
        }
    }

    pub fn content_hash(&self) -> Digest {
        match self {
            Self::Reuse { content_hash, .. }
            | Self::Full { content_hash, .. }
            | Self::Delta { content_hash, .. } => *content_hash,
        }
    }
}

fn encode_single_chunk_with_fallback(preferred_codec: u8, chunk: &[u8]) -> (u8, Vec<u8>) {
    if preferred_codec == RAW || chunk.is_empty() {
        return (RAW, chunk.to_vec());
    }
    match encode_with_codec(preferred_codec, chunk) {
        Ok(enc) if enc.len() < chunk.len() && enc.len() <= V4_MAX_ENCODED_BYTES => {
            (preferred_codec, enc)
        }
        _ => (RAW, chunk.to_vec()),
    }
}

/// Core v4 sync decision engine:
/// - `REUSE`: remote already holds `id` with `valid == true` and matching `content_hash` + `total_bytes` -> `0` bytes transferred.
/// - `DELTA`: remote holds a valid prior version of `id` with matching `total_bytes`, only a subset of logical chunks changed,
///   and sending the changed chunks + indices is cheaper (both wire bytes and total cost ms) than `FULL`.
/// - `FULL`: `id` is new, invalidated, resized, or `FULL` is cheaper than `DELTA`.
pub fn sync_decision(
    id: &str,
    bytes: &[u8],
    known_remote_state: Option<&TensorRegistryEntry>,
    policy: CodecCostPolicy,
) -> SyncDecision {
    let content_hash = digest(bytes);
    let v3_wire = v3_always_send_wire_bytes(id, bytes.len());

    // 1. Check REUSE (valid resident state with identical content hash and byte length)
    if let Some(remote) = known_remote_state {
        if remote.valid
            && remote.content_hash == content_hash
            && remote.total_bytes == bytes.len()
            && remote.version >= 1
        {
            return SyncDecision::Reuse {
                tensor_id: id.to_string(),
                content_hash,
                version: remote.version,
                total_chunks: remote.chunk_hashes.len() as u32,
                bytes_avoided: v3_wire,
            };
        }
    }

    let next_version = known_remote_state
        .map(|r| r.version.saturating_add(1).max(1))
        .unwrap_or(1);

    let plan = select_v4_adaptive_plan(bytes, policy);
    // Preserve the resident chunk layout if known_remote_state is valid and same size so chunk hashes align 1:1
    let chunk_bytes = match known_remote_state {
        Some(remote)
            if remote.valid
                && remote.total_bytes == bytes.len()
                && (512..=V4_MAX_CHUNK_BYTES).contains(&remote.chunk_bytes) =>
        {
            remote.chunk_bytes
        }
        _ => plan.chunk_bytes,
    };

    let t_enc_start = Instant::now();
    let mut all_chunks = Vec::new();
    let mut changed_chunks = Vec::new();
    let mut reused_chunks = 0u32;

    let can_try_delta = known_remote_state.is_some_and(|r| {
        r.valid
            && r.total_bytes == bytes.len()
            && r.chunk_bytes == chunk_bytes
            && !r.chunk_hashes.is_empty()
    });

    for (idx, sub) in bytes.chunks(chunk_bytes).enumerate() {
        let ch_hash = digest(sub);
        let is_unchanged = can_try_delta
            && known_remote_state
                .and_then(|r| r.chunk_hashes.get(idx))
                .is_some_and(|old_hash| *old_hash == ch_hash);

        let (c_id, enc) = encode_single_chunk_with_fallback(plan.codec, sub);
        let update = EncodedChunkUpdate {
            chunk_index: idx as u32,
            orig_len: sub.len() as u32,
            codec: c_id,
            chunk_hash: ch_hash,
            encoded_bytes: enc,
        };
        if is_unchanged {
            reused_chunks += 1;
        } else {
            changed_chunks.push(update.clone());
        }
        all_chunks.push(update);
    }

    let measured_enc_ms = t_enc_start.elapsed().as_secs_f64() * 1_000.0;
    let total_chunks = all_chunks.len() as u32;

    let full_wire_bytes = (V4_TENSOR_OP_FIXED_BYTES + id.len()) as u64
        + all_chunks
            .iter()
            .map(|c| c.wire_bytes() as u64)
            .sum::<u64>();
    let (cal_full_enc_ms, cal_full_dec_ms) = calibrated_cpu_ms(plan.codec, bytes.len());
    let full_enc_ms = measured_enc_ms.max(cal_full_enc_ms);
    let full_cost_ms = full_enc_ms
        + cal_full_dec_ms
        + policy.wire_transfer_ms(full_wire_bytes as usize)
        + (total_chunks as f64) * 0.0005;

    if can_try_delta && reused_chunks > 0 && !changed_chunks.is_empty() {
        let delta_wire_bytes = (V4_TENSOR_OP_FIXED_BYTES + id.len()) as u64
            + changed_chunks
                .iter()
                .map(|c| c.wire_bytes() as u64)
                .sum::<u64>();
        let changed_raw_bytes: usize = changed_chunks.iter().map(|c| c.orig_len as usize).sum();
        let (cal_delta_enc_ms, cal_delta_dec_ms) = calibrated_cpu_ms(plan.codec, changed_raw_bytes);
        let delta_fraction = (changed_chunks.len() as f64) / (total_chunks.max(1) as f64);
        let delta_enc_ms = (measured_enc_ms * delta_fraction).max(cal_delta_enc_ms);
        let delta_cost_ms = delta_enc_ms
            + cal_delta_dec_ms
            + policy.wire_transfer_ms(delta_wire_bytes as usize)
            + (changed_chunks.len() as f64) * 0.0005;

        if delta_wire_bytes < full_wire_bytes && delta_cost_ms < full_cost_ms {
            return SyncDecision::Delta {
                tensor_id: id.to_string(),
                content_hash,
                version: next_version,
                workload_class: plan.workload_class,
                codec: plan.codec,
                chunk_bytes,
                total_chunks,
                reused_chunks,
                changed_chunks,
                wire_bytes: delta_wire_bytes,
                full_wire_bytes,
                v3_always_send_wire_bytes: v3_wire,
                encode_ms: delta_enc_ms,
                decode_ms: cal_delta_dec_ms,
                total_cost_ms: delta_cost_ms,
                full_cost_ms,
            };
        }
    }

    SyncDecision::Full {
        tensor_id: id.to_string(),
        content_hash,
        version: next_version,
        workload_class: plan.workload_class,
        codec: plan.codec,
        chunk_bytes,
        total_chunks,
        chunks: all_chunks,
        wire_bytes: full_wire_bytes,
        v3_always_send_wire_bytes: v3_wire,
        encode_ms: full_enc_ms,
        decode_ms: cal_full_dec_ms,
        total_cost_ms: full_cost_ms,
    }
}

/// Wire-byte and timing accounting for FASTLINK v4 (`FL4-IMPL` Requirement 6).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct V4WireAccounting {
    pub bytes_per_token: f64,
    pub reused_chunks: u64,
    pub transferred_chunks: u64,
    pub sync_events: u64,
    pub encode_ms: f64,
    pub decode_ms: f64,
    pub reuse_events: u64,
    pub full_events: u64,
    pub delta_events: u64,
    pub batches_sent: u64,
    pub round_trips: u64,
    pub tokens_accounted: u64,
    pub logical_bytes: u64,
    pub wire_bytes_sent: u64,
    pub v3_always_send_wire_bytes: u64,
    pub reuse_bytes_saved: u64,
}

impl V4WireAccounting {
    pub fn record_tokens(&mut self, tokens: u64) {
        self.tokens_accounted = self.tokens_accounted.saturating_add(tokens);
        self.recompute_bytes_per_token();
    }

    pub fn recompute_bytes_per_token(&mut self) {
        let denom = self.tokens_accounted.max(self.sync_events).max(1);
        self.bytes_per_token = (self.wire_bytes_sent as f64) / (denom as f64);
    }

    pub fn bytes_per_token_for(&self, tokens: u64) -> f64 {
        if tokens == 0 {
            return 0.0;
        }
        (self.wire_bytes_sent as f64) / (tokens as f64)
    }
}

/// Coalesced tensor operation inside a `BatchedSyncFrame`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchedTensorOp {
    pub mode: u8,
    pub session_id: String,
    pub tensor_id: String,
    pub owner: String,
    pub version: u64,
    pub total_bytes: u64,
    pub chunk_bytes: u32,
    pub total_chunks: u32,
    pub content_hash: Digest,
    pub updates: Vec<EncodedChunkUpdate>,
}

/// Coalesced network batch frame combining multiple chunk/tensor updates into a single
/// RPC transport frame (`1` network round-trip per batch instead of `1` per chunk).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchedSyncFrame {
    pub batch_id: u64,
    pub ops: Vec<BatchedTensorOp>,
}

impl BatchedSyncFrame {
    pub fn new(batch_id: u64) -> Self {
        Self {
            batch_id,
            ops: Vec::new(),
        }
    }

    pub fn push_decision(
        &mut self,
        session_id: &str,
        owner: &str,
        total_bytes: usize,
        decision: &SyncDecision,
    ) {
        match decision {
            SyncDecision::Reuse { .. } => {
                // REUSE sends 0 wire bytes; nothing is added to the network frame!
            }
            SyncDecision::Full {
                tensor_id,
                content_hash,
                version,
                chunk_bytes,
                total_chunks,
                chunks,
                ..
            } => {
                self.ops.push(BatchedTensorOp {
                    mode: OP_FULL,
                    session_id: session_id.to_string(),
                    tensor_id: tensor_id.clone(),
                    owner: owner.to_string(),
                    version: *version,
                    total_bytes: total_bytes as u64,
                    chunk_bytes: *chunk_bytes as u32,
                    total_chunks: *total_chunks,
                    content_hash: *content_hash,
                    updates: chunks.clone(),
                });
            }
            SyncDecision::Delta {
                tensor_id,
                content_hash,
                version,
                chunk_bytes,
                total_chunks,
                changed_chunks,
                ..
            } => {
                self.ops.push(BatchedTensorOp {
                    mode: OP_DELTA,
                    session_id: session_id.to_string(),
                    tensor_id: tensor_id.clone(),
                    owner: owner.to_string(),
                    version: *version,
                    total_bytes: total_bytes as u64,
                    chunk_bytes: *chunk_bytes as u32,
                    total_chunks: *total_chunks,
                    content_hash: *content_hash,
                    updates: changed_chunks.clone(),
                });
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    pub fn total_chunk_updates(&self) -> usize {
        self.ops.iter().map(|op| op.updates.len()).sum()
    }

    /// Serialize the entire batch into a single contiguous wire frame.
    pub fn encode(&self) -> io::Result<Vec<u8>> {
        let mut buf = Vec::with_capacity(256);
        self.write_to(&mut buf)?;
        Ok(buf)
    }

    pub fn write_to(&self, w: &mut impl Write) -> io::Result<()> {
        if self.ops.len() > MAX_BATCH_OPS {
            return Err(invalid("batch op_count exceeds bound"));
        }
        let mut remaining_updates = MAX_BATCH_UPDATES;
        let mut remaining_tensor_bytes = MAX_BATCH_TENSOR_BYTES;
        let mut remaining_encoded_bytes = MAX_BATCH_TENSOR_BYTES;
        for op in &self.ops {
            if op.total_bytes == 0
                || op.total_bytes > remaining_tensor_bytes
                || op.chunk_bytes == 0
                || op.chunk_bytes as usize > V4_MAX_CHUNK_BYTES
                || op.total_bytes.div_ceil(op.chunk_bytes as u64) != op.total_chunks as u64
                || op.updates.len() > op.total_chunks as usize
                || op.updates.len() > remaining_updates
            {
                return Err(invalid("invalid or oversized v4 batch tensor geometry"));
            }
            remaining_tensor_bytes -= op.total_bytes;
            remaining_updates -= op.updates.len();
            for update in &op.updates {
                if update.encoded_bytes.len() as u64 > remaining_encoded_bytes {
                    return Err(invalid("batch encoded bytes exceed bound"));
                }
                remaining_encoded_bytes -= update.encoded_bytes.len() as u64;
            }
        }
        w.write_all(V4_BATCH_MAGIC)?;
        write_u16(w, V4_BATCH_VERSION)?;
        write_u64(w, self.batch_id)?;
        write_u32(w, self.ops.len() as u32)?;

        for op in &self.ops {
            write_u8(w, op.mode)?;
            write_str(w, &op.session_id)?;
            write_str(w, &op.tensor_id)?;
            write_str(w, &op.owner)?;
            write_u64(w, op.version)?;
            write_u64(w, op.total_bytes)?;
            write_u32(w, op.chunk_bytes)?;
            write_u32(w, op.total_chunks)?;
            w.write_all(&op.content_hash)?;
            write_u32(w, op.updates.len() as u32)?;
            for u in &op.updates {
                write_u32(w, u.chunk_index)?;
                write_u32(w, u.orig_len)?;
                write_u8(w, u.codec)?;
                w.write_all(&u.chunk_hash)?;
                write_u32(w, u.encoded_bytes.len() as u32)?;
                w.write_all(&u.encoded_bytes)?;
            }
        }
        Ok(())
    }

    pub fn decode(bytes: &[u8]) -> io::Result<Self> {
        let mut cursor = io::Cursor::new(bytes);
        let frame = Self::read_from(&mut cursor)?;
        if cursor.position() != bytes.len() as u64 {
            return Err(invalid("trailing bytes after v4 batch frame"));
        }
        Ok(frame)
    }

    pub fn read_from(r: &mut impl Read) -> io::Result<Self> {
        let mut magic = [0u8; 4];
        r.read_exact(&mut magic)?;
        if &magic != V4_BATCH_MAGIC {
            return Err(invalid("invalid v4 batch magic"));
        }
        let version = read_u16(r)?;
        if version != V4_BATCH_VERSION {
            return Err(invalid("unsupported v4 batch version"));
        }
        let batch_id = read_u64(r)?;
        let op_count = read_u32(r)? as usize;
        if op_count > MAX_BATCH_OPS {
            return Err(invalid("batch op_count exceeds bound"));
        }

        let mut remaining_updates = MAX_BATCH_UPDATES;
        let mut remaining_tensor_bytes = MAX_BATCH_TENSOR_BYTES;
        let mut remaining_encoded_bytes = MAX_BATCH_TENSOR_BYTES;
        let mut ops = Vec::with_capacity(op_count);
        for _ in 0..op_count {
            let mode = read_u8(r)?;
            if !matches!(mode, OP_FULL | OP_DELTA) {
                return Err(invalid("invalid batch op mode"));
            }
            let session_id = read_str(r)?;
            let tensor_id = read_str(r)?;
            let owner = read_str(r)?;
            let ver = read_u64(r)?;
            let total_bytes = read_u64(r)?;
            let chunk_bytes = read_u32(r)?;
            let total_chunks = read_u32(r)?;
            if chunk_bytes == 0
                || chunk_bytes as usize > V4_MAX_CHUNK_BYTES
                || total_bytes == 0
                || total_bytes > remaining_tensor_bytes
                || total_bytes.div_ceil(chunk_bytes as u64) != total_chunks as u64
            {
                return Err(invalid("invalid or oversized v4 batch tensor geometry"));
            }
            remaining_tensor_bytes -= total_bytes;
            let mut content_hash = [0u8; 32];
            r.read_exact(&mut content_hash)?;

            let update_count = read_u32(r)? as usize;
            if update_count > total_chunks as usize || update_count > remaining_updates {
                return Err(invalid("batch update_count exceeds bound"));
            }
            remaining_updates -= update_count;
            let mut updates = Vec::with_capacity(update_count);
            for _ in 0..update_count {
                let chunk_index = read_u32(r)?;
                let orig_len = read_u32(r)?;
                let codec = read_u8(r)?;
                if orig_len as usize > V4_MAX_CHUNK_BYTES {
                    return Err(invalid("orig_len exceeds V4_MAX_CHUNK_BYTES"));
                }
                let mut chunk_hash = [0u8; 32];
                r.read_exact(&mut chunk_hash)?;
                let enc_len = read_u32(r)? as usize;
                if enc_len > V4_MAX_ENCODED_BYTES || enc_len as u64 > remaining_encoded_bytes {
                    return Err(invalid("enc_len exceeds batch byte bound"));
                }
                remaining_encoded_bytes -= enc_len as u64;
                let mut encoded_bytes = vec![0u8; enc_len];
                r.read_exact(&mut encoded_bytes)?;
                updates.push(EncodedChunkUpdate {
                    chunk_index,
                    orig_len,
                    codec,
                    chunk_hash,
                    encoded_bytes,
                });
            }
            ops.push(BatchedTensorOp {
                mode,
                session_id,
                tensor_id,
                owner,
                version: ver,
                total_bytes,
                chunk_bytes,
                total_chunks,
                content_hash,
                updates,
            });
        }

        Ok(Self { batch_id, ops })
    }
}

fn write_str(w: &mut impl Write, s: &str) -> io::Result<()> {
    let bytes = s.as_bytes();
    if bytes.is_empty() || bytes.len() > MAX_BATCH_IDENTIFIER_BYTES {
        return Err(invalid("invalid identifier length"));
    }
    write_u16(w, bytes.len() as u16)?;
    w.write_all(bytes)
}

fn read_str(r: &mut impl Read) -> io::Result<String> {
    let len = read_u16(r)? as usize;
    if len == 0 || len > MAX_BATCH_IDENTIFIER_BYTES {
        return Err(invalid("invalid identifier length"));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    String::from_utf8(buf).map_err(|_| invalid("identifier is not valid UTF-8"))
}

#[derive(Debug, Clone)]
struct ResidentWorkerTensor {
    metadata: TensorRegistryEntry,
    data: Vec<u8>,
}

/// Remote worker resident tensor store with per-session isolation and atomic commit semantics.
#[derive(Debug, Default)]
pub struct RemoteWorkerStore {
    worker_id: String,
    generation: u64,
    /// Keyed by `(session_id, tensor_id)` so sessions are strictly isolated.
    resident: HashMap<(String, String), ResidentWorkerTensor>,
}

impl RemoteWorkerStore {
    pub fn new(worker_id: impl Into<String>) -> Self {
        Self {
            worker_id: worker_id.into(),
            generation: 1,
            resident: HashMap::new(),
        }
    }

    pub fn worker_id(&self) -> &str {
        &self.worker_id
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn resident_count(&self) -> usize {
        self.resident.values().filter(|r| r.metadata.valid).count()
    }

    pub fn get_metadata(&self, session_id: &str, tensor_id: &str) -> Option<&TensorRegistryEntry> {
        self.resident
            .get(&(session_id.to_string(), tensor_id.to_string()))
            .map(|r| &r.metadata)
    }

    pub fn get_tensor_bytes(&self, session_id: &str, tensor_id: &str) -> Option<&[u8]> {
        self.resident
            .get(&(session_id.to_string(), tensor_id.to_string()))
            .and_then(|r| {
                if r.metadata.valid && digest(&r.data) == r.metadata.content_hash {
                    Some(r.data.as_slice())
                } else {
                    None
                }
            })
    }

    /// Simulate worker process restart: increments generation and clears volatile resident memory.
    pub fn restart(&mut self) {
        self.generation = self.generation.saturating_add(1);
        self.resident.clear();
    }

    /// Apply a coalesced `BatchedSyncFrame`, decoding into temporary staging buffers and committing
    /// ONLY when every chunk hash AND the full tensor BLAKE3 digest match.
    /// Returns `(applied_ops, decode_ms)`.
    pub fn apply_batch(&mut self, batch: &BatchedSyncFrame) -> io::Result<(u32, f64)> {
        let t_dec = Instant::now();
        let mut staged_commits: Vec<((String, String), ResidentWorkerTensor)> =
            Vec::with_capacity(batch.ops.len());

        let mut remaining_tensor_bytes = MAX_BATCH_TENSOR_BYTES;
        for op in &batch.ops {
            if op.total_bytes == 0 || op.total_bytes > remaining_tensor_bytes {
                return Err(invalid("batch tensor total exceeds memory bound"));
            }
            remaining_tensor_bytes -= op.total_bytes;
            let key = (op.session_id.clone(), op.tensor_id.clone());
            let total_bytes = usize::try_from(op.total_bytes)
                .map_err(|_| invalid("batch tensor exceeds platform address space"))?;
            let chunk_bytes = op.chunk_bytes as usize;
            if chunk_bytes == 0 || chunk_bytes > V4_MAX_CHUNK_BYTES {
                return Err(invalid("invalid worker batch chunk geometry"));
            }
            let expected_chunks = if total_bytes == 0 {
                0
            } else {
                total_bytes.div_ceil(chunk_bytes)
            };
            if expected_chunks != op.total_chunks as usize {
                return Err(invalid("batch op total_chunks mismatch"));
            }

            let (mut staged_data, mut staged_hashes) = match op.mode {
                OP_FULL => (
                    vec![0u8; total_bytes],
                    vec![[0u8; 32]; op.total_chunks as usize],
                ),
                OP_DELTA => {
                    let prior = self.resident.get(&key).ok_or_else(|| {
                        invalid("DELTA op received for non-resident tensor on worker")
                    })?;
                    if !prior.metadata.valid
                        || prior.data.len() != total_bytes
                        || prior.metadata.chunk_bytes != chunk_bytes
                        || prior.metadata.chunk_hashes.len() != expected_chunks
                    {
                        return Err(invalid(
                            "DELTA op incompatible with resident tensor geometry",
                        ));
                    }
                    (prior.data.clone(), prior.metadata.chunk_hashes.clone())
                }
                _ => return Err(invalid("unsupported op mode")),
            };

            for u in &op.updates {
                let idx = u.chunk_index as usize;
                if idx >= expected_chunks {
                    return Err(invalid("chunk_index out of bounds"));
                }
                let start = idx * chunk_bytes;
                let expected_len = (total_bytes - start).min(chunk_bytes);
                if u.orig_len as usize != expected_len {
                    return Err(invalid("chunk orig_len mismatch"));
                }
                let decoded = decode_v4_chunk(u.codec, &u.encoded_bytes, expected_len)?;
                if digest(&decoded) != u.chunk_hash {
                    return Err(invalid("decoded chunk BLAKE3 hash mismatch"));
                }
                staged_data[start..start + expected_len].copy_from_slice(&decoded);
                staged_hashes[idx] = u.chunk_hash;
            }

            // Verify full reconstructed tensor digest before committing!
            if digest(&staged_data) != op.content_hash {
                return Err(invalid(
                    "reconstructed tensor BLAKE3 hash mismatch; refusing to commit",
                ));
            }

            let entry = TensorRegistryEntry {
                tensor_id: op.tensor_id.clone(),
                session_id: op.session_id.clone(),
                content_hash: op.content_hash,
                version: op.version,
                owner: op.owner.clone(),
                valid: true,
                last_sync_ms: now_epoch_ms(),
                chunk_bytes,
                chunk_hashes: staged_hashes,
                total_bytes,
            };
            staged_commits.push((
                key,
                ResidentWorkerTensor {
                    metadata: entry,
                    data: staged_data,
                },
            ));
        }

        let applied = staged_commits.len() as u32;
        for (key, tensor) in staged_commits {
            self.resident.insert(key, tensor);
        }

        let decode_ms = t_dec.elapsed().as_secs_f64() * 1_000.0;
        Ok((applied, decode_ms))
    }

    /// Audit all resident tensors in this worker to verify `0` cache corruption.
    pub fn verify_integrity(&self) -> io::Result<usize> {
        let mut count = 0usize;
        for ((session_id, tensor_id), r) in &self.resident {
            if &r.metadata.session_id != session_id || &r.metadata.tensor_id != tensor_id {
                return Err(invalid("resident tensor key mismatch"));
            }
            if r.data.len() != r.metadata.total_bytes {
                return Err(invalid("resident tensor length mismatch"));
            }
            if digest(&r.data) != r.metadata.content_hash {
                return Err(invalid("resident tensor BLAKE3 corruption detected"));
            }
            for (idx, sub) in r.data.chunks(r.metadata.chunk_bytes).enumerate() {
                if r.metadata.chunk_hashes.get(idx) != Some(&digest(sub)) {
                    return Err(invalid("resident chunk BLAKE3 corruption detected"));
                }
            }
            count += 1;
        }
        Ok(count)
    }
}

/// Persistent Tensor Registry (`FL4-IMPL` Requirement 1, 3, 4, 5, 6).
///
/// Tracks `tensor_id` (scoped by `session_id`) -> `{ content_hash (BLAKE3), version (u64), owner, valid, last_sync_ms }`,
/// coordinates `REUSE` / `FULL` / `DELTA` decisions, coalesces updates into `BatchedSyncFrame`s,
/// handles invalidation on content update / memory reuse / worker restart, and records exact wire metrics.
#[derive(Debug)]
pub struct PersistentTensorRegistry {
    policy: CodecCostPolicy,
    next_batch_id: u64,
    entries: HashMap<(String, String), TensorRegistryEntry>,
    accounting: V4WireAccounting,
}

impl Default for PersistentTensorRegistry {
    fn default() -> Self {
        Self::new(CodecCostPolicy::default())
    }
}

impl PersistentTensorRegistry {
    pub fn new(policy: CodecCostPolicy) -> Self {
        Self {
            policy,
            next_batch_id: 1,
            entries: HashMap::new(),
            accounting: V4WireAccounting::default(),
        }
    }

    pub fn policy(&self) -> CodecCostPolicy {
        self.policy
    }

    pub fn accounting(&self) -> &V4WireAccounting {
        &self.accounting
    }

    pub fn accounting_mut(&mut self) -> &mut V4WireAccounting {
        &mut self.accounting
    }

    pub fn get_entry(&self, session_id: &str, tensor_id: &str) -> Option<&TensorRegistryEntry> {
        self.entries
            .get(&(session_id.to_string(), tensor_id.to_string()))
    }

    /// Evaluate `sync_decision` for `(session_id, tensor_id)` against the registry's known remote state.
    pub fn decide(&self, session_id: &str, tensor_id: &str, bytes: &[u8]) -> SyncDecision {
        let known = self.get_entry(session_id, tensor_id);
        sync_decision(tensor_id, bytes, known, self.policy)
    }

    /// Explicitly invalidate a tensor in `session_id` (e.g. before an in-place buffer overwrite).
    /// Bumps `version` and sets `valid = false` without touching any other session's tensors.
    pub fn invalidate_tensor(&mut self, session_id: &str, tensor_id: &str) -> bool {
        if let Some(entry) = self
            .entries
            .get_mut(&(session_id.to_string(), tensor_id.to_string()))
        {
            entry.valid = false;
            entry.version = entry.version.saturating_add(1);
            true
        } else {
            false
        }
    }

    /// Explicitly invalidate all tensors belonging to `session_id` without affecting other sessions.
    pub fn invalidate_session(&mut self, session_id: &str) -> usize {
        let mut count = 0usize;
        for ((sess, _), entry) in &mut self.entries {
            if sess == session_id && entry.valid {
                entry.valid = false;
                entry.version = entry.version.saturating_add(1);
                count += 1;
            }
        }
        count
    }

    /// Handle worker restart/reconnect: mark ONLY that worker's resident tensors `valid = false`
    /// while keeping all other workers' tensors valid and resident!
    pub fn notify_worker_restart(&mut self, worker_owner: &str) -> usize {
        let mut invalidated = 0usize;
        for entry in self.entries.values_mut() {
            if entry.owner == worker_owner && entry.valid {
                entry.valid = false;
                invalidated += 1;
            }
        }
        invalidated
    }

    /// Synchronize a single tensor with `worker`, automatically handling `REUSE` (0 wire bytes),
    /// `DELTA` (changed chunks only), or `FULL` (adaptive codec + chunk size), updating both
    /// the remote worker store and the persistent registry on success.
    pub fn sync_tensor(
        &mut self,
        session_id: &str,
        tensor_id: &str,
        bytes: &[u8],
        worker: &mut RemoteWorkerStore,
    ) -> io::Result<SyncDecision> {
        let mut results = self.sync_tensors_batched(
            session_id,
            &[(tensor_id, bytes)],
            worker,
            1, // 1 token/step default
        )?;
        Ok(results.remove(0))
    }

    /// Synchronize a batch of tensors with `worker` in a single coalesced `BatchedSyncFrame`
    /// (one transport frame for the entire batch!).
    pub fn sync_tensors_batched(
        &mut self,
        session_id: &str,
        tensors: &[(&str, &[u8])],
        worker: &mut RemoteWorkerStore,
        tokens_in_step: u64,
    ) -> io::Result<Vec<SyncDecision>> {
        let owner = worker.worker_id().to_string();
        let batch_id = self.next_batch_id;
        self.next_batch_id = self.next_batch_id.saturating_add(1);

        let mut frame = BatchedSyncFrame::new(batch_id);
        let mut decisions = Vec::with_capacity(tensors.len());

        for &(tensor_id, bytes) in tensors {
            let known = self.get_entry(session_id, tensor_id);
            let decision = sync_decision(tensor_id, bytes, known, self.policy);
            frame.push_decision(session_id, &owner, bytes.len(), &decision);
            decisions.push((tensor_id.to_string(), bytes, decision));
        }

        let mut batch_wire_bytes = 0u64;
        let mut batch_decode_ms = 0.0;

        if !frame.is_empty() {
            let wire_frame = frame.encode()?;
            // Wire bytes = coalesced request frame + 5-byte ACK response
            batch_wire_bytes = (wire_frame.len() + 5) as u64;
            let decoded_frame = BatchedSyncFrame::decode(&wire_frame)?;
            let (_applied, dec_ms) = worker.apply_batch(&decoded_frame)?;
            batch_decode_ms = dec_ms;
            self.accounting.batches_sent += 1;
            self.accounting.round_trips += 1;
        }

        // Commit registry metadata and accounting after worker verification succeeds
        let now_ms = now_epoch_ms();
        let mut out_decisions = Vec::with_capacity(decisions.len());

        for (tensor_id, bytes, decision) in decisions {
            let v3_wire = v3_always_send_wire_bytes(&tensor_id, bytes.len());
            self.accounting.sync_events += 1;
            self.accounting.logical_bytes += bytes.len() as u64;
            self.accounting.v3_always_send_wire_bytes += v3_wire;

            match &decision {
                SyncDecision::Reuse {
                    total_chunks,
                    bytes_avoided,
                    ..
                } => {
                    self.accounting.reuse_events += 1;
                    self.accounting.reused_chunks += *total_chunks as u64;
                    self.accounting.reuse_bytes_saved += *bytes_avoided;
                    if let Some(entry) = self
                        .entries
                        .get_mut(&(session_id.to_string(), tensor_id.clone()))
                    {
                        entry.last_sync_ms = now_ms;
                    }
                }
                SyncDecision::Full {
                    content_hash,
                    version,
                    chunk_bytes,
                    total_chunks,
                    encode_ms,
                    decode_ms,
                    ..
                } => {
                    self.accounting.full_events += 1;
                    self.accounting.transferred_chunks += *total_chunks as u64;
                    self.accounting.encode_ms += *encode_ms;
                    self.accounting.decode_ms += (*decode_ms).max(batch_decode_ms);
                    let chunk_hashes: Vec<Digest> =
                        bytes.chunks(*chunk_bytes).map(digest).collect();
                    self.entries.insert(
                        (session_id.to_string(), tensor_id.clone()),
                        TensorRegistryEntry {
                            tensor_id,
                            session_id: session_id.to_string(),
                            content_hash: *content_hash,
                            version: *version,
                            owner: owner.clone(),
                            valid: true,
                            last_sync_ms: now_ms,
                            chunk_bytes: *chunk_bytes,
                            chunk_hashes,
                            total_bytes: bytes.len(),
                        },
                    );
                }
                SyncDecision::Delta {
                    content_hash,
                    version,
                    chunk_bytes,
                    reused_chunks,
                    changed_chunks,
                    encode_ms,
                    decode_ms,
                    ..
                } => {
                    self.accounting.delta_events += 1;
                    self.accounting.reused_chunks += *reused_chunks as u64;
                    self.accounting.transferred_chunks += changed_chunks.len() as u64;
                    self.accounting.encode_ms += *encode_ms;
                    self.accounting.decode_ms += (*decode_ms).max(batch_decode_ms);
                    let chunk_hashes: Vec<Digest> =
                        bytes.chunks(*chunk_bytes).map(digest).collect();
                    self.entries.insert(
                        (session_id.to_string(), tensor_id.clone()),
                        TensorRegistryEntry {
                            tensor_id,
                            session_id: session_id.to_string(),
                            content_hash: *content_hash,
                            version: *version,
                            owner: owner.clone(),
                            valid: true,
                            last_sync_ms: now_ms,
                            chunk_bytes: *chunk_bytes,
                            chunk_hashes,
                            total_bytes: bytes.len(),
                        },
                    );
                }
            }
            out_decisions.push(decision);
        }

        self.accounting.wire_bytes_sent += batch_wire_bytes;
        if batch_wire_bytes > 0 {
            // Recompute net wire bytes saved vs v3 always-send across all events
            self.accounting.reuse_bytes_saved = self
                .accounting
                .v3_always_send_wire_bytes
                .saturating_sub(self.accounting.wire_bytes_sent);
        }
        self.accounting.record_tokens(tokens_in_step);

        Ok(out_decisions)
    }
}

/// Multi-worker, multi-session thread-safe v4 cluster coordinator for testing concurrent
/// sessions, multi-worker residency, and single-worker restart recovery.
pub struct V4Cluster {
    registry: Mutex<PersistentTensorRegistry>,
    workers: Mutex<HashMap<String, Arc<Mutex<RemoteWorkerStore>>>>,
}

impl Default for V4Cluster {
    fn default() -> Self {
        Self::new(CodecCostPolicy::default())
    }
}

impl V4Cluster {
    pub fn new(policy: CodecCostPolicy) -> Self {
        Self {
            registry: Mutex::new(PersistentTensorRegistry::new(policy)),
            workers: Mutex::new(HashMap::new()),
        }
    }

    pub fn ensure_worker(&self, worker_id: &str) -> Arc<Mutex<RemoteWorkerStore>> {
        let mut map = self.workers.lock().expect("workers lock poisoned");
        map.entry(worker_id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(RemoteWorkerStore::new(worker_id))))
            .clone()
    }

    /// Synchronize a tensor for `(session_id, worker_id, tensor_id)`.
    /// Thread-safe across concurrent sessions (`N=4+`).
    pub fn sync_tensor(
        &self,
        session_id: &str,
        worker_id: &str,
        tensor_id: &str,
        bytes: &[u8],
    ) -> io::Result<SyncDecision> {
        let worker_arc = self.ensure_worker(worker_id);
        let mut reg = self.registry.lock().expect("registry lock poisoned");
        let mut worker = worker_arc.lock().expect("worker lock poisoned");
        reg.sync_tensor(session_id, tensor_id, bytes, &mut worker)
    }

    /// Synchronize multiple tensors in a single coalesced batch frame.
    pub fn sync_batch(
        &self,
        session_id: &str,
        worker_id: &str,
        tensors: &[(&str, &[u8])],
        tokens_in_step: u64,
    ) -> io::Result<Vec<SyncDecision>> {
        let worker_arc = self.ensure_worker(worker_id);
        let mut reg = self.registry.lock().expect("registry lock poisoned");
        let mut worker = worker_arc.lock().expect("worker lock poisoned");
        reg.sync_tensors_batched(session_id, tensors, &mut worker, tokens_in_step)
    }

    /// Kill and restart a single worker (`worker_id`), clearing its volatile memory and
    /// invalidating ONLY its tensors in the persistent registry while keeping all other
    /// workers' tensors valid and resident.
    pub fn restart_worker(&self, worker_id: &str) -> usize {
        let worker_arc = self.ensure_worker(worker_id);
        {
            let mut worker = worker_arc.lock().expect("worker lock poisoned");
            worker.restart();
        }
        let mut reg = self.registry.lock().expect("registry lock poisoned");
        reg.notify_worker_restart(worker_id)
    }

    pub fn invalidate_tensor(&self, session_id: &str, tensor_id: &str) -> bool {
        let mut reg = self.registry.lock().expect("registry lock poisoned");
        reg.invalidate_tensor(session_id, tensor_id)
    }

    pub fn invalidate_session(&self, session_id: &str) -> usize {
        let mut reg = self.registry.lock().expect("registry lock poisoned");
        reg.invalidate_session(session_id)
    }

    pub fn read_worker_tensor(
        &self,
        worker_id: &str,
        session_id: &str,
        tensor_id: &str,
    ) -> Option<Vec<u8>> {
        let worker_arc = self.ensure_worker(worker_id);
        let worker = worker_arc.lock().expect("worker lock poisoned");
        worker
            .get_tensor_bytes(session_id, tensor_id)
            .map(|b| b.to_vec())
    }

    pub fn verify_all_workers_integrity(&self) -> io::Result<usize> {
        let map = self.workers.lock().expect("workers lock poisoned");
        let mut total = 0usize;
        for w in map.values() {
            let guard = w.lock().expect("worker lock poisoned");
            total += guard.verify_integrity()?;
        }
        Ok(total)
    }

    pub fn accounting(&self) -> V4WireAccounting {
        self.registry
            .lock()
            .expect("registry lock poisoned")
            .accounting()
            .clone()
    }
}

/// Loopback TCP server for `BatchedSyncFrame` transport verification.
pub struct V4LoopbackServer {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    store: Arc<Mutex<RemoteWorkerStore>>,
    batches_received: Arc<AtomicU64>,
    handle: Option<JoinHandle<()>>,
}

impl V4LoopbackServer {
    pub fn spawn(address: &str, worker_id: &str) -> io::Result<Self> {
        let sock = validate_listen_address(address)?;
        let listener = TcpListener::bind(sock)?;
        let local_addr = listener.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let store = Arc::new(Mutex::new(RemoteWorkerStore::new(worker_id)));
        let batches_received = Arc::new(AtomicU64::new(0));

        let stop_clone = Arc::clone(&stop);
        let store_clone = Arc::clone(&store);
        let batches_clone = Arc::clone(&batches_received);

        let handle = thread::spawn(move || {
            for conn in listener.incoming() {
                if stop_clone.load(Ordering::SeqCst) {
                    break;
                }
                if let Ok(mut stream) = conn {
                    if stop_clone.load(Ordering::SeqCst) {
                        break;
                    }
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
                    let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));
                    let store_session = Arc::clone(&store_clone);
                    let batches_session = Arc::clone(&batches_clone);
                    thread::spawn(move || {
                        if let Ok(batch) = BatchedSyncFrame::read_from(&mut stream) {
                            let res = {
                                let mut guard = store_session.lock().expect("store lock poisoned");
                                guard.apply_batch(&batch)
                            };
                            match res {
                                Ok((applied, _)) => {
                                    batches_session.fetch_add(1, Ordering::Relaxed);
                                    let _ = write_u8(&mut stream, 0);
                                    let _ = write_u32(&mut stream, applied);
                                    let _ = stream.flush();
                                }
                                Err(_) => {
                                    let _ = write_u8(&mut stream, 1);
                                    let _ = write_u32(&mut stream, 0);
                                    let _ = stream.flush();
                                }
                            }
                        }
                    });
                }
            }
        });

        Ok(Self {
            addr: local_addr,
            stop,
            store,
            batches_received,
            handle: Some(handle),
        })
    }

    pub fn addr_string(&self) -> String {
        self.addr.to_string()
    }

    pub fn batches_received(&self) -> u64 {
        self.batches_received.load(Ordering::Relaxed)
    }

    pub fn store(&self) -> &Arc<Mutex<RemoteWorkerStore>> {
        &self.store
    }

    pub fn shutdown(mut self) {
        self.stop_inner();
    }

    fn stop_inner(&mut self) {
        if !self.stop.swap(true, Ordering::SeqCst) {
            let _ = TcpStream::connect(self.addr);
        }
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for V4LoopbackServer {
    fn drop(&mut self) {
        self.stop_inner();
    }
}

/// Send a coalesced `BatchedSyncFrame` over a single TCP loopback connection (`1` round-trip).
/// Returns `(wire_bytes_total, applied_ops)`.
pub fn send_batch_tcp(server: &str, batch: &BatchedSyncFrame) -> io::Result<(u64, u32)> {
    if batch.is_empty() {
        return Ok((0, 0));
    }
    let sock = validate_connect_address(server)?;
    let mut stream = TcpStream::connect(sock)?;
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    stream.set_write_timeout(Some(Duration::from_secs(30)))?;
    let encoded = batch.encode()?;
    stream.write_all(&encoded)?;
    stream.flush()?;
    let status = read_u8(&mut stream)?;
    let applied = read_u32(&mut stream)?;
    if status != 0 {
        return Err(invalid("remote worker rejected v4 batch frame"));
    }
    Ok(((encoded.len() + 5) as u64, applied))
}
