//! BLAKE3-verified KV-cache and SSM state delta checkpointing.
//!
//! Enables safe stage migration and node evacuation by transferring only
//! changed KV/SSM pages between epochs instead of retransmitting full state.

use std::fmt;

use crate::{decode_chunk, digest, encode_chunk, Digest, CHUNK_BYTES};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KvPageSnapshot {
    pub epoch: u64,
    pub generation: u64,
    pub layer_start: u32,
    pub layer_end: u32,
    pub page_digests: Vec<Digest>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KvDeltaEntry {
    pub page_index: usize,
    pub codec: u8,
    pub expected_len: u32,
    pub expected_digest: Digest,
    pub encoded: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KvDeltaPacket {
    pub target_epoch: u64,
    pub target_generation: u64,
    pub total_pages: usize,
    pub deltas: Vec<KvDeltaEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KvDeltaError {
    PageCountMismatch,
    LayerRangeMismatch,
    PageOutOfBounds(usize),
    PageTooLarge(usize),
    DigestMismatch(usize),
    DecodeFailed(String),
}

impl fmt::Display for KvDeltaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PageCountMismatch => write!(f, "KV snapshot page count mismatch"),
            Self::LayerRangeMismatch => write!(f, "KV snapshot layer range mismatch"),
            Self::PageOutOfBounds(idx) => write!(f, "KV page index {idx} out of bounds"),
            Self::PageTooLarge(n) => write!(f, "KV page {n} B exceeds {CHUNK_BYTES} B limit"),
            Self::DigestMismatch(idx) => write!(f, "KV page {idx} BLAKE3 digest mismatch"),
            Self::DecodeFailed(e) => write!(f, "KV page decode failed: {e}"),
        }
    }
}

impl std::error::Error for KvDeltaError {}

/// Computes the page indices whose BLAKE3 digests differ from `previous`.
pub fn diff_kv_pages(
    previous: Option<&KvPageSnapshot>,
    current: &KvPageSnapshot,
) -> Result<Vec<usize>, KvDeltaError> {
    let Some(prev) = previous else {
        return Ok((0..current.page_digests.len()).collect());
    };
    if prev.layer_start != current.layer_start || prev.layer_end != current.layer_end {
        return Err(KvDeltaError::LayerRangeMismatch);
    }
    if prev.page_digests.len() != current.page_digests.len() {
        return Ok((0..current.page_digests.len()).collect());
    }
    Ok(current
        .page_digests
        .iter()
        .enumerate()
        .filter_map(|(idx, d)| {
            if prev.page_digests[idx] != *d {
                Some(idx)
            } else {
                None
            }
        })
        .collect())
}

/// Builds a compressed delta packet containing only pages that changed since `previous`.
pub fn build_kv_delta_packet(
    previous: Option<&KvPageSnapshot>,
    current: &KvPageSnapshot,
    current_pages: &[&[u8]],
) -> Result<KvDeltaPacket, KvDeltaError> {
    if current_pages.len() != current.page_digests.len() {
        return Err(KvDeltaError::PageCountMismatch);
    }
    let changed = diff_kv_pages(previous, current)?;
    let mut deltas = Vec::with_capacity(changed.len());
    for idx in changed {
        let page = current_pages[idx];
        if page.len() > CHUNK_BYTES {
            return Err(KvDeltaError::PageTooLarge(page.len()));
        }
        if digest(page) != current.page_digests[idx] {
            return Err(KvDeltaError::DigestMismatch(idx));
        }
        let (codec, encoded) = encode_chunk(page);
        deltas.push(KvDeltaEntry {
            page_index: idx,
            codec,
            expected_len: page.len() as u32,
            expected_digest: current.page_digests[idx],
            encoded,
        });
    }
    Ok(KvDeltaPacket {
        target_epoch: current.epoch,
        target_generation: current.generation,
        total_pages: current.page_digests.len(),
        deltas,
    })
}

/// Applies a delta packet to `target_pages` and verifies every updated page's BLAKE3 digest.
pub fn apply_kv_delta_packet(
    packet: &KvDeltaPacket,
    target_pages: &mut Vec<Vec<u8>>,
) -> Result<usize, KvDeltaError> {
    if target_pages.len() != packet.total_pages {
        target_pages.resize_with(packet.total_pages, Vec::new);
    }
    for entry in &packet.deltas {
        if entry.page_index >= packet.total_pages {
            return Err(KvDeltaError::PageOutOfBounds(entry.page_index));
        }
        let decoded = decode_chunk(entry.codec, &entry.encoded, entry.expected_len as usize)
            .map_err(|e| KvDeltaError::DecodeFailed(e.to_string()))?;
        if digest(&decoded) != entry.expected_digest {
            return Err(KvDeltaError::DigestMismatch(entry.page_index));
        }
        target_pages[entry.page_index] = decoded;
    }
    Ok(packet.deltas.len())
}
