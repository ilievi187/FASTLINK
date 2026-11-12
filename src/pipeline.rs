//! Point-to-point activation framing and per-boundary pipeline telemetry.
//!
//! Adjacent stages exchange activations directly without routing data through
//! the coordinator. Every frame carries the manifest epoch, stage routing
//! indices, and a BLAKE3 payload digest.

use std::fmt;

use crate::{decode_chunk, digest, encode_chunk, Digest, CHUNK_BYTES};

pub const ACTIVATION_MAGIC: &[u8; 4] = b"FLAC";
pub const ACTIVATION_HEADER_BYTES: usize = 69;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActivationFrame {
    pub epoch: u64,
    pub seq_id: u64,
    pub token_pos: u32,
    pub src_stage: u16,
    pub dst_stage: u16,
    pub codec: u8,
    pub decoded_len: u32,
    pub payload_digest: Digest,
    pub payload: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ActivationError {
    EmptyPayload,
    PayloadTooLarge(usize),
    TruncatedFrame,
    BadMagic,
    UnsupportedCodec(u8),
    DigestMismatch,
    StaleEpoch { frame_epoch: u64, active_epoch: u64 },
    InvalidStageHop { src_stage: u16, dst_stage: u16 },
    DecodeFailed(String),
}

impl fmt::Display for ActivationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyPayload => write!(f, "activation payload must not be empty"),
            Self::PayloadTooLarge(n) => {
                write!(f, "activation payload {n} B exceeds {CHUNK_BYTES} B limit")
            }
            Self::TruncatedFrame => write!(f, "truncated activation frame"),
            Self::BadMagic => write!(f, "invalid activation frame magic"),
            Self::UnsupportedCodec(c) => write!(f, "unsupported activation codec {c}"),
            Self::DigestMismatch => write!(f, "activation payload BLAKE3 digest mismatch"),
            Self::StaleEpoch {
                frame_epoch,
                active_epoch,
            } => write!(
                f,
                "stale activation frame epoch {frame_epoch}, active epoch {active_epoch}"
            ),
            Self::InvalidStageHop {
                src_stage,
                dst_stage,
            } => write!(f, "non-adjacent stage hop {src_stage} -> {dst_stage}"),
            Self::DecodeFailed(e) => write!(f, "activation decode failed: {e}"),
        }
    }
}

impl std::error::Error for ActivationError {}

/// Encodes an activation tensor slice into a bounded wire frame with BLAKE3 integrity.
pub fn encode_activation_frame(
    epoch: u64,
    seq_id: u64,
    token_pos: u32,
    src_stage: u16,
    dst_stage: u16,
    raw_activation: &[u8],
) -> Result<Vec<u8>, ActivationError> {
    if raw_activation.is_empty() {
        return Err(ActivationError::EmptyPayload);
    }
    if raw_activation.len() > CHUNK_BYTES {
        return Err(ActivationError::PayloadTooLarge(raw_activation.len()));
    }
    if dst_stage != src_stage.saturating_add(1) {
        return Err(ActivationError::InvalidStageHop {
            src_stage,
            dst_stage,
        });
    }

    let payload_digest = digest(raw_activation);
    let (codec, encoded) = encode_chunk(raw_activation);
    let decoded_len = raw_activation.len() as u32;
    let encoded_len = encoded.len() as u32;

    let mut out = Vec::with_capacity(ACTIVATION_HEADER_BYTES + encoded.len());
    out.extend_from_slice(ACTIVATION_MAGIC);
    out.extend_from_slice(&epoch.to_le_bytes());
    out.extend_from_slice(&seq_id.to_le_bytes());
    out.extend_from_slice(&token_pos.to_le_bytes());
    out.extend_from_slice(&src_stage.to_le_bytes());
    out.extend_from_slice(&dst_stage.to_le_bytes());
    out.push(codec);
    out.extend_from_slice(&decoded_len.to_le_bytes());
    out.extend_from_slice(&encoded_len.to_le_bytes());
    out.extend_from_slice(&payload_digest);
    out.extend_from_slice(&encoded);
    Ok(out)
}

/// Decodes and verifies a bounded activation frame against the active manifest epoch.
pub fn decode_activation_frame(
    frame_bytes: &[u8],
    active_epoch: u64,
) -> Result<(ActivationFrame, Vec<u8>), ActivationError> {
    if frame_bytes.len() < ACTIVATION_HEADER_BYTES {
        return Err(ActivationError::TruncatedFrame);
    }
    if &frame_bytes[0..4] != ACTIVATION_MAGIC {
        return Err(ActivationError::BadMagic);
    }

    let epoch = u64::from_le_bytes(frame_bytes[4..12].try_into().unwrap());
    let seq_id = u64::from_le_bytes(frame_bytes[12..20].try_into().unwrap());
    let token_pos = u32::from_le_bytes(frame_bytes[20..24].try_into().unwrap());
    let src_stage = u16::from_le_bytes(frame_bytes[24..26].try_into().unwrap());
    let dst_stage = u16::from_le_bytes(frame_bytes[26..28].try_into().unwrap());
    let codec = frame_bytes[28];
    let decoded_len = u32::from_le_bytes(frame_bytes[29..33].try_into().unwrap());
    let encoded_len = u32::from_le_bytes(frame_bytes[33..37].try_into().unwrap()) as usize;
    let mut payload_digest = [0u8; 32];
    payload_digest.copy_from_slice(&frame_bytes[37..69]);

    if epoch < active_epoch {
        return Err(ActivationError::StaleEpoch {
            frame_epoch: epoch,
            active_epoch,
        });
    }
    if dst_stage != src_stage.saturating_add(1) {
        return Err(ActivationError::InvalidStageHop {
            src_stage,
            dst_stage,
        });
    }
    if decoded_len == 0 || decoded_len as usize > CHUNK_BYTES {
        return Err(ActivationError::PayloadTooLarge(decoded_len as usize));
    }
    if codec > 1 {
        return Err(ActivationError::UnsupportedCodec(codec));
    }
    let expected_total = ACTIVATION_HEADER_BYTES.saturating_add(encoded_len);
    if frame_bytes.len() < expected_total {
        return Err(ActivationError::TruncatedFrame);
    }

    let encoded_slice = &frame_bytes[ACTIVATION_HEADER_BYTES..expected_total];
    let raw = decode_chunk(codec, encoded_slice, decoded_len as usize)
        .map_err(|e| ActivationError::DecodeFailed(e.to_string()))?;

    if digest(&raw) != payload_digest {
        return Err(ActivationError::DigestMismatch);
    }

    let frame = ActivationFrame {
        epoch,
        seq_id,
        token_pos,
        src_stage,
        dst_stage,
        codec,
        decoded_len,
        payload_digest,
        payload: encoded_slice.to_vec(),
    };
    Ok((frame, raw))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TelemetrySource {
    Measured,
    Simulated,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BoundarySample {
    pub boundary_index: u16,
    pub raw_bytes: u64,
    pub wire_bytes: u64,
    pub link_rtt_us: u64,
    pub stage_compute_us: u64,
}

/// Computes percentile (p50, p95) on a slice of microsecond samples.
pub fn percentile_p50_p95(samples: &[u64]) -> (u64, u64) {
    if samples.is_empty() {
        return (0, 0);
    }
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let p50 = sorted[(sorted.len() - 1) / 2];
    let p95_idx = ((sorted.len() - 1) * 95) / 100;
    let p95 = sorted[p95_idx];
    (p50, p95)
}

#[derive(Clone, Debug, PartialEq)]
pub struct PipelineSummary {
    pub source: TelemetrySource,
    pub total_tokens: u64,
    pub total_raw_bytes: u64,
    pub total_wire_bytes: u64,
    pub compute_p50_us: u64,
    pub compute_p95_us: u64,
    pub link_rtt_p50_us: u64,
    pub link_rtt_p95_us: u64,
    pub mean_decode_tok_per_sec: f64,
}

pub fn summarize_pipeline(
    source: TelemetrySource,
    samples: &[BoundarySample],
    tokens: u64,
) -> PipelineSummary {
    let total_raw_bytes: u64 = samples.iter().map(|s| s.raw_bytes).sum();
    let total_wire_bytes: u64 = samples.iter().map(|s| s.wire_bytes).sum();
    let compute: Vec<u64> = samples.iter().map(|s| s.stage_compute_us).collect();
    let rtt: Vec<u64> = samples.iter().map(|s| s.link_rtt_us).collect();
    let (compute_p50_us, compute_p95_us) = percentile_p50_p95(&compute);
    let (link_rtt_p50_us, link_rtt_p95_us) = percentile_p50_p95(&rtt);

    let total_us: u64 = samples
        .iter()
        .map(|s| s.stage_compute_us.saturating_add(s.link_rtt_us))
        .sum();
    let mean_decode_tok_per_sec = if total_us > 0 && tokens > 0 {
        (tokens as f64) / (total_us as f64 / 1_000_000.0)
    } else {
        0.0
    };

    PipelineSummary {
        source,
        total_tokens: tokens,
        total_raw_bytes,
        total_wire_bytes,
        compute_p50_us,
        compute_p95_us,
        link_rtt_p50_us,
        link_rtt_p95_us,
        mean_decode_tok_per_sec,
    }
}
