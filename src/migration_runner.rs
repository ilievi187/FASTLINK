//! KV/SSM delta migration runner: moves verified KV pages between nodes.
//!
//! Executes layer migrations produced by the resharding workflow, transferring
//! only the KV pages whose BLAKE3 digests changed since the destination held its
//! previous snapshot. Cold migrations (no destination snapshot) transfer every
//! page; warm migrations transfer only the deltas.

use crate::cluster_orchestrator::{LayerMigration, ReshardOutcome};
use crate::digest;
use crate::kv_delta::{self, KvDeltaError, KvPageSnapshot};
use std::fmt;

/// Outcome of a single layer migration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigrationReport {
    pub from_node: String,
    pub to_node: String,
    pub layer_start: u32,
    pub layer_end: u32,
    pub target_epoch: u64,
    pub deltas_transferred: usize,
    pub total_wire_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MigrationError {
    InvalidLayerRange,
    LayerRangeMismatch,
    KvDelta(KvDeltaError),
    VerificationFailed(String),
}

impl fmt::Display for MigrationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLayerRange => write!(f, "invalid layer range for KV migration"),
            Self::LayerRangeMismatch => {
                write!(f, "KV migration layer range does not match source snapshot")
            }
            Self::KvDelta(e) => write!(f, "kv delta migration failed: {e}"),
            Self::VerificationFailed(e) => write!(f, "KV migration verification failed: {e}"),
        }
    }
}

impl std::error::Error for MigrationError {}

impl From<KvDeltaError> for MigrationError {
    fn from(error: KvDeltaError) -> Self {
        Self::KvDelta(error)
    }
}

/// Migrates one contiguous layer range from source KV pages into a target
/// buffer, using a delta packet when the target already holds a snapshot.
pub fn execute_layer_migration(
    migration: &LayerMigration,
    target_epoch: u64,
    target_generation: u64,
    source_pages: &[Vec<u8>],
    source_snapshot: &KvPageSnapshot,
    target_pages: &mut Vec<Vec<u8>>,
    target_snapshot: Option<&KvPageSnapshot>,
) -> Result<MigrationReport, MigrationError> {
    if migration.layer_start >= migration.layer_end {
        return Err(MigrationError::InvalidLayerRange);
    }
    if source_snapshot.layer_start != migration.layer_start
        || source_snapshot.layer_end != migration.layer_end
    {
        return Err(MigrationError::LayerRangeMismatch);
    }
    let current = KvPageSnapshot {
        epoch: target_epoch,
        generation: target_generation,
        layer_start: migration.layer_start,
        layer_end: migration.layer_end,
        page_digests: source_snapshot.page_digests.clone(),
    };
    let pages: Vec<&[u8]> = source_pages.iter().map(|p| p.as_slice()).collect();
    let packet = kv_delta::build_kv_delta_packet(target_snapshot, &current, &pages)
        .map_err(MigrationError::KvDelta)?;
    let deltas_transferred =
        kv_delta::apply_kv_delta_packet(&packet, target_pages).map_err(MigrationError::KvDelta)?;
    for (idx, page) in target_pages.iter().enumerate() {
        if digest(page) != source_snapshot.page_digests[idx] {
            return Err(MigrationError::VerificationFailed(format!(
                "target page {idx} does not match source digest"
            )));
        }
    }
    let mut total_wire_bytes: u64 = 0;
    for entry in &packet.deltas {
        total_wire_bytes += entry.encoded.len() as u64;
    }
    Ok(MigrationReport {
        from_node: migration.from_node.clone(),
        to_node: migration.to_node.clone(),
        layer_start: migration.layer_start,
        layer_end: migration.layer_end,
        target_epoch,
        deltas_transferred,
        total_wire_bytes,
    })
}

/// Executes every migration planned for a resharding outcome.
pub fn execute_reshard_migrations(
    outcome: &ReshardOutcome,
    source_pages: &[Vec<u8>],
    source_snapshot: &KvPageSnapshot,
    target_pages: &mut Vec<Vec<u8>>,
    target_snapshot: Option<&KvPageSnapshot>,
) -> Result<Vec<MigrationReport>, MigrationError> {
    let mut reports = Vec::new();
    for migration in &outcome.migrations {
        let report = execute_layer_migration(
            migration,
            source_snapshot.epoch,
            source_snapshot.generation,
            source_pages,
            source_snapshot,
            target_pages,
            target_snapshot,
        )?;
        reports.push(report);
    }
    Ok(reports)
}
