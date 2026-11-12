//! Epoch-scoped keyed-BLAKE3 authentication for P2P activation frames.
//!
//! Wraps `ActivationFrame` wire packets with a 32-byte keyed BLAKE3 MAC
//! bound to the epoch and adjacent stage hop, rejecting tampered frames
//! and stale-epoch replays without coordinator involvement on the data path.

use std::fmt;

use crate::pipeline::{
    decode_activation_frame, encode_activation_frame, ActivationError, ActivationFrame,
};

pub const AUTH_TAG_BYTES: usize = 32;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StageAuthError {
    PacketTooShort,
    InvalidAuthTag,
    StageMismatch { expected: u16, got: u16 },
    Activation(ActivationError),
}

impl fmt::Display for StageAuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PacketTooShort => {
                write!(f, "authenticated stage packet shorter than 32-byte MAC")
            }
            Self::InvalidAuthTag => write!(f, "stage frame keyed-BLAKE3 authentication failed"),
            Self::StageMismatch { expected, got } => {
                write!(
                    f,
                    "destination stage mismatch: expected {expected}, got {got}"
                )
            }
            Self::Activation(e) => write!(f, "activation frame error: {e}"),
        }
    }
}

impl std::error::Error for StageAuthError {}

impl From<ActivationError> for StageAuthError {
    fn from(e: ActivationError) -> Self {
        Self::Activation(e)
    }
}

/// Derives an epoch- and hop-scoped 32-byte MAC key from a shared cluster secret.
pub fn derive_hop_key(
    cluster_secret: &[u8; 32],
    epoch: u64,
    from_stage: u16,
    to_stage: u16,
) -> [u8; 32] {
    let mut material = Vec::with_capacity(32);
    material.extend_from_slice(b"FASTLINK-HOP-KEY-V1");
    material.extend_from_slice(&epoch.to_le_bytes());
    material.extend_from_slice(&from_stage.to_le_bytes());
    material.extend_from_slice(&to_stage.to_le_bytes());
    *blake3::keyed_hash(cluster_secret, &material).as_bytes()
}

/// Encodes an activation payload into a `FLAC` wire frame and appends a
/// 32-byte keyed-BLAKE3 MAC.
pub fn seal_stage_frame(
    hop_key: &[u8; 32],
    epoch: u64,
    seq_id: u64,
    token_pos: u32,
    src_stage: u16,
    dst_stage: u16,
    raw_activation: &[u8],
) -> Result<Vec<u8>, StageAuthError> {
    let mut packet = encode_activation_frame(
        epoch,
        seq_id,
        token_pos,
        src_stage,
        dst_stage,
        raw_activation,
    )?;
    let tag = blake3::keyed_hash(hop_key, &packet);
    packet.extend_from_slice(tag.as_bytes());
    Ok(packet)
}

/// Verifies the 32-byte keyed-BLAKE3 MAC, then decodes and validates the frame
/// and destination stage.
pub fn open_stage_frame(
    hop_key: &[u8; 32],
    packet: &[u8],
    expected_epoch: u64,
    expected_dst_stage: u16,
) -> Result<(ActivationFrame, Vec<u8>), StageAuthError> {
    if packet.len() <= AUTH_TAG_BYTES {
        return Err(StageAuthError::PacketTooShort);
    }
    let split = packet.len() - AUTH_TAG_BYTES;
    let (wire, tag_bytes) = packet.split_at(split);
    let expected_tag = blake3::keyed_hash(hop_key, wire);
    let mut provided = [0u8; AUTH_TAG_BYTES];
    provided.copy_from_slice(tag_bytes);
    if expected_tag != blake3::Hash::from(provided) {
        return Err(StageAuthError::InvalidAuthTag);
    }
    let (frame, payload) = decode_activation_frame(wire, expected_epoch)?;
    if frame.dst_stage != expected_dst_stage {
        return Err(StageAuthError::StageMismatch {
            expected: expected_dst_stage,
            got: frame.dst_stage,
        });
    }
    Ok((frame, payload))
}
