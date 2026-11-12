//! Versioned partition manifests and graceful join/leave/reshard state.
//!
//! Coordinates two-tier control+data plane topology: the coordinator publishes
//! BLAKE3-verified, epoch-versioned `PartitionManifest`s, while adjacent stages
//! exchange activations directly via `next_stage_addr`.

use std::collections::HashMap;
use std::fmt;

use crate::{digest, Digest};

pub const MANIFEST_VERSION: u16 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeLifecycle {
    Joining,
    Active,
    Draining,
    Evacuated,
    Faulted,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StageEndpoint {
    pub stage_index: usize,
    pub node_id: String,
    pub addr: String,
    pub layer_start: u32,
    pub layer_end: u32,
    /// Direct point-to-point endpoint of stage_index + 1 (None for final stage).
    pub next_stage_addr: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StageSpec {
    pub node_id: String,
    pub addr: String,
    pub layer_start: u32,
    pub layer_end: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PartitionManifest {
    pub manifest_version: u16,
    pub epoch: u64,
    pub model_digest: Digest,
    pub context_len: u32,
    pub total_layers: u32,
    pub stages: Vec<StageEndpoint>,
    pub manifest_digest: Digest,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ManifestError {
    EmptyStages,
    InvalidLayerRange,
    NonContiguousLayers,
    DigestMismatch,
    StaleEpoch {
        expected_gt: u64,
        got: u64,
    },
    UnknownNode(String),
    InvalidTransition {
        node_id: String,
        from: NodeLifecycle,
        to: NodeLifecycle,
    },
    NodeNotReady(String),
}

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyStages => write!(f, "manifest must contain at least one stage"),
            Self::InvalidLayerRange => write!(f, "stage has empty or inverted layer range"),
            Self::NonContiguousLayers => {
                write!(f, "stages do not contiguously cover 0..total_layers")
            }
            Self::DigestMismatch => write!(f, "manifest BLAKE3 digest verification failed"),
            Self::StaleEpoch { expected_gt, got } => {
                write!(
                    f,
                    "stale manifest epoch: got {got}, expected > {expected_gt}"
                )
            }
            Self::UnknownNode(id) => write!(f, "unknown node in manifest coordinator: {id}"),
            Self::InvalidTransition { node_id, from, to } => {
                write!(
                    f,
                    "invalid lifecycle transition for {node_id}: {from:?} -> {to:?}"
                )
            }
            Self::NodeNotReady(id) => write!(f, "node {id} is not active or joining"),
        }
    }
}

impl std::error::Error for ManifestError {}

fn compute_manifest_digest(
    manifest_version: u16,
    epoch: u64,
    model_digest: &Digest,
    context_len: u32,
    total_layers: u32,
    stages: &[StageEndpoint],
) -> Digest {
    let mut buf = Vec::new();
    buf.extend_from_slice(b"FASTLINK-MANIFEST-V1");
    buf.extend_from_slice(&manifest_version.to_le_bytes());
    buf.extend_from_slice(&epoch.to_le_bytes());
    buf.extend_from_slice(model_digest);
    buf.extend_from_slice(&context_len.to_le_bytes());
    buf.extend_from_slice(&total_layers.to_le_bytes());
    buf.extend_from_slice(&(stages.len() as u32).to_le_bytes());
    for stage in stages {
        buf.extend_from_slice(&(stage.stage_index as u32).to_le_bytes());
        buf.extend_from_slice(&(stage.node_id.len() as u32).to_le_bytes());
        buf.extend_from_slice(stage.node_id.as_bytes());
        buf.extend_from_slice(&(stage.addr.len() as u32).to_le_bytes());
        buf.extend_from_slice(stage.addr.as_bytes());
        buf.extend_from_slice(&stage.layer_start.to_le_bytes());
        buf.extend_from_slice(&stage.layer_end.to_le_bytes());
        match &stage.next_stage_addr {
            Some(next) => {
                buf.push(1);
                buf.extend_from_slice(&(next.len() as u32).to_le_bytes());
                buf.extend_from_slice(next.as_bytes());
            }
            None => buf.push(0),
        }
    }
    digest(&buf)
}

impl PartitionManifest {
    /// Builds a validated manifest with direct adjacent-stage `next_stage_addr` links.
    pub fn build(
        epoch: u64,
        model_digest: Digest,
        context_len: u32,
        total_layers: u32,
        specs: &[StageSpec],
    ) -> Result<Self, ManifestError> {
        if specs.is_empty() {
            return Err(ManifestError::EmptyStages);
        }
        let mut expected_start = 0u32;
        for spec in specs {
            if spec.layer_start >= spec.layer_end {
                return Err(ManifestError::InvalidLayerRange);
            }
            if spec.layer_start != expected_start {
                return Err(ManifestError::NonContiguousLayers);
            }
            expected_start = spec.layer_end;
        }
        if expected_start != total_layers {
            return Err(ManifestError::NonContiguousLayers);
        }

        let stages: Vec<StageEndpoint> = specs
            .iter()
            .enumerate()
            .map(|(idx, spec)| StageEndpoint {
                stage_index: idx,
                node_id: spec.node_id.clone(),
                addr: spec.addr.clone(),
                layer_start: spec.layer_start,
                layer_end: spec.layer_end,
                next_stage_addr: specs.get(idx + 1).map(|next| next.addr.clone()),
            })
            .collect();

        let manifest_digest = compute_manifest_digest(
            MANIFEST_VERSION,
            epoch,
            &model_digest,
            context_len,
            total_layers,
            &stages,
        );

        Ok(Self {
            manifest_version: MANIFEST_VERSION,
            epoch,
            model_digest,
            context_len,
            total_layers,
            stages,
            manifest_digest,
        })
    }

    /// Recomputes and verifies the BLAKE3 manifest digest and contiguous layers.
    pub fn verify(&self) -> Result<(), ManifestError> {
        if self.stages.is_empty() {
            return Err(ManifestError::EmptyStages);
        }
        let mut expected_start = 0u32;
        for (idx, stage) in self.stages.iter().enumerate() {
            if stage.stage_index != idx || stage.layer_start >= stage.layer_end {
                return Err(ManifestError::InvalidLayerRange);
            }
            if stage.layer_start != expected_start {
                return Err(ManifestError::NonContiguousLayers);
            }
            expected_start = stage.layer_end;
        }
        if expected_start != self.total_layers {
            return Err(ManifestError::NonContiguousLayers);
        }
        let recomputed = compute_manifest_digest(
            self.manifest_version,
            self.epoch,
            &self.model_digest,
            self.context_len,
            self.total_layers,
            &self.stages,
        );
        if recomputed != self.manifest_digest {
            return Err(ManifestError::DigestMismatch);
        }
        Ok(())
    }
}

/// Manages node lifecycle and atomic epoch-bumped manifest resharding.
pub struct ManifestCoordinator {
    current: Option<PartitionManifest>,
    lifecycles: HashMap<String, NodeLifecycle>,
}

impl ManifestCoordinator {
    pub fn new() -> Self {
        Self {
            current: None,
            lifecycles: HashMap::new(),
        }
    }

    pub fn current_manifest(&self) -> Option<&PartitionManifest> {
        self.current.as_ref()
    }

    pub fn node_state(&self, node_id: &str) -> Option<NodeLifecycle> {
        self.lifecycles.get(node_id).copied()
    }

    pub fn register_joining(&mut self, node_id: impl Into<String>) {
        self.lifecycles
            .insert(node_id.into(), NodeLifecycle::Joining);
    }

    pub fn begin_evacuation(&mut self, node_id: &str) -> Result<(), ManifestError> {
        let state = self
            .lifecycles
            .get_mut(node_id)
            .ok_or_else(|| ManifestError::UnknownNode(node_id.to_string()))?;
        match *state {
            NodeLifecycle::Active | NodeLifecycle::Joining => {
                *state = NodeLifecycle::Draining;
                Ok(())
            }
            other => Err(ManifestError::InvalidTransition {
                node_id: node_id.to_string(),
                from: other,
                to: NodeLifecycle::Draining,
            }),
        }
    }

    pub fn complete_evacuation(&mut self, node_id: &str) -> Result<(), ManifestError> {
        let state = self
            .lifecycles
            .get_mut(node_id)
            .ok_or_else(|| ManifestError::UnknownNode(node_id.to_string()))?;
        match *state {
            NodeLifecycle::Draining => {
                *state = NodeLifecycle::Evacuated;
                Ok(())
            }
            other => Err(ManifestError::InvalidTransition {
                node_id: node_id.to_string(),
                from: other,
                to: NodeLifecycle::Evacuated,
            }),
        }
    }

    pub fn mark_faulted(&mut self, node_id: &str) -> Result<(), ManifestError> {
        let state = self
            .lifecycles
            .get_mut(node_id)
            .ok_or_else(|| ManifestError::UnknownNode(node_id.to_string()))?;
        *state = NodeLifecycle::Faulted;
        Ok(())
    }

    /// Atomically validates and commits a new resharded manifest at a strictly
    /// increasing epoch, promoting participating `Joining` nodes to `Active`.
    pub fn commit_reshard(
        &mut self,
        model_digest: Digest,
        context_len: u32,
        total_layers: u32,
        specs: &[StageSpec],
    ) -> Result<&PartitionManifest, ManifestError> {
        let next_epoch = self.current.as_ref().map_or(1, |m| m.epoch + 1);
        for spec in specs {
            match self.lifecycles.get(&spec.node_id).copied() {
                Some(NodeLifecycle::Active) | Some(NodeLifecycle::Joining) => {}
                Some(_) => return Err(ManifestError::NodeNotReady(spec.node_id.clone())),
                None => return Err(ManifestError::UnknownNode(spec.node_id.clone())),
            }
        }

        let manifest =
            PartitionManifest::build(next_epoch, model_digest, context_len, total_layers, specs)?;
        manifest.verify()?;

        for spec in specs {
            self.lifecycles
                .insert(spec.node_id.clone(), NodeLifecycle::Active);
        }
        self.current = Some(manifest);
        Ok(self.current.as_ref().expect("just assigned"))
    }

    /// Rejects any worker message or state update carrying an outdated epoch.
    pub fn validate_epoch(&self, epoch: u64) -> Result<(), ManifestError> {
        let current_epoch = self.current.as_ref().map_or(0, |m| m.epoch);
        if epoch < current_epoch || epoch == 0 {
            return Err(ManifestError::StaleEpoch {
                expected_gt: current_epoch.saturating_sub(1),
                got: epoch,
            });
        }
        Ok(())
    }
}

impl Default for ManifestCoordinator {
    fn default() -> Self {
        Self::new()
    }
}
