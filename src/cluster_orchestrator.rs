//! Cluster orchestrator bridging the heterogeneous scheduler, pairing trust,
//! versioned partition manifests, KV delta migration plans, and loopback runtime.

use std::collections::HashMap;
use std::fmt;

use crate::manifest::{
    ManifestCoordinator, ManifestError, NodeLifecycle, PartitionManifest, StageSpec,
};
use crate::node_runtime::{run_loopback_pipeline, RuntimeError};
use crate::pairing::NetworkTrust;
use crate::pipeline::PipelineSummary;
use crate::scheduler::{
    plan_heterogeneous_stages, ModelSpec, MultiStagePlan, NodeProfile, PlanObjective,
};
use crate::Digest;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClusterNode {
    pub profile: NodeProfile,
    pub addr: String,
    pub trust: NetworkTrust,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LayerMigration {
    pub from_node: String,
    pub to_node: String,
    pub layer_start: u32,
    pub layer_end: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReshardOutcome {
    pub previous_epoch: Option<u64>,
    pub new_epoch: u64,
    pub plan: MultiStagePlan,
    pub migrations: Vec<LayerMigration>,
}

#[derive(Debug)]
pub enum OrchestratorError {
    PlanDoesNotFit,
    MissingEndpoint(String),
    UntrustedNode(String),
    NoActiveManifest,
    ContextLenOverflow(u64),
    Manifest(ManifestError),
    Runtime(RuntimeError),
}

impl fmt::Display for OrchestratorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PlanDoesNotFit => write!(f, "cluster cannot fit model within memory budgets"),
            Self::MissingEndpoint(id) => write!(f, "missing P2P endpoint for node {id}"),
            Self::UntrustedNode(id) => {
                write!(f, "node {id} has Untrusted network policy and cannot join")
            }
            Self::NoActiveManifest => write!(f, "no active partition manifest committed"),
            Self::ContextLenOverflow(v) => write!(f, "context_len {v} exceeds u32::MAX"),
            Self::Manifest(e) => write!(f, "manifest error: {e}"),
            Self::Runtime(e) => write!(f, "runtime error: {e:?}"),
        }
    }
}

impl std::error::Error for OrchestratorError {}

impl From<ManifestError> for OrchestratorError {
    fn from(e: ManifestError) -> Self {
        Self::Manifest(e)
    }
}

impl From<RuntimeError> for OrchestratorError {
    fn from(e: RuntimeError) -> Self {
        Self::Runtime(e)
    }
}

/// Converts a `MultiStagePlan` and node endpoint map into `StageSpec`s for `PartitionManifest`.
pub fn stage_specs_from_plan(
    plan: &MultiStagePlan,
    addrs: &HashMap<String, String>,
) -> Result<Vec<StageSpec>, OrchestratorError> {
    if !plan.fits_model || plan.stages.is_empty() {
        return Err(OrchestratorError::PlanDoesNotFit);
    }
    let mut specs = Vec::with_capacity(plan.stages.len());
    for stage in &plan.stages {
        let addr = addrs
            .get(&stage.node_id)
            .ok_or_else(|| OrchestratorError::MissingEndpoint(stage.node_id.clone()))?;
        specs.push(StageSpec {
            node_id: stage.node_id.clone(),
            addr: addr.clone(),
            layer_start: stage.layer_start,
            layer_end: stage.layer_end,
        });
    }
    Ok(specs)
}

fn compute_migrations(
    previous: Option<&PartitionManifest>,
    next: &PartitionManifest,
) -> Vec<LayerMigration> {
    let Some(prev) = previous else {
        return Vec::new();
    };
    let mut migrations = Vec::new();
    for new_stage in &next.stages {
        for old_stage in &prev.stages {
            if old_stage.node_id == new_stage.node_id {
                continue;
            }
            let overlap_start = old_stage.layer_start.max(new_stage.layer_start);
            let overlap_end = old_stage.layer_end.min(new_stage.layer_end);
            if overlap_start < overlap_end {
                migrations.push(LayerMigration {
                    from_node: old_stage.node_id.clone(),
                    to_node: new_stage.node_id.clone(),
                    layer_start: overlap_start,
                    layer_end: overlap_end,
                });
            }
        }
    }
    migrations
}

/// Coordinates heterogeneous planning, manifest epoch commits, evacuation/fault
/// resharding, and loopback pipeline execution.
pub struct ClusterOrchestrator {
    model: ModelSpec,
    model_digest: Digest,
    coordinator: ManifestCoordinator,
    nodes: HashMap<String, ClusterNode>,
}

impl ClusterOrchestrator {
    pub fn new(model: ModelSpec, model_digest: Digest) -> Self {
        Self {
            model,
            model_digest,
            coordinator: ManifestCoordinator::new(),
            nodes: HashMap::new(),
        }
    }

    pub fn current_manifest(&self) -> Option<&PartitionManifest> {
        self.coordinator.current_manifest()
    }

    pub fn node_state(&self, node_id: &str) -> Option<NodeLifecycle> {
        self.coordinator.node_state(node_id)
    }

    pub fn register_node(&mut self, node: ClusterNode) -> Result<(), OrchestratorError> {
        if node.trust == NetworkTrust::Untrusted {
            return Err(OrchestratorError::UntrustedNode(node.profile.id.clone()));
        }
        let id = node.profile.id.clone();
        self.coordinator.register_joining(&id);
        self.nodes.insert(id, node);
        Ok(())
    }

    fn eligible_profiles_and_addrs(&self) -> (Vec<NodeProfile>, HashMap<String, String>) {
        let mut ids: Vec<&String> = self.nodes.keys().collect();
        ids.sort();
        let mut profiles = Vec::new();
        let mut addrs = HashMap::new();
        for id in ids {
            if matches!(
                self.coordinator.node_state(id),
                Some(NodeLifecycle::Active) | Some(NodeLifecycle::Joining)
            ) {
                let node = &self.nodes[id];
                profiles.push(node.profile.clone());
                addrs.insert(id.clone(), node.addr.clone());
            }
        }
        (profiles, addrs)
    }

    fn commit_planned_manifest(
        &mut self,
        objective: PlanObjective,
        local_id: Option<&str>,
    ) -> Result<(MultiStagePlan, PartitionManifest), OrchestratorError> {
        let (profiles, addrs) = self.eligible_profiles_and_addrs();
        let plan = plan_heterogeneous_stages(&self.model, &profiles, objective, local_id);
        let specs = stage_specs_from_plan(&plan, &addrs)?;
        let context_len = u32::try_from(self.model.context_len)
            .map_err(|_| OrchestratorError::ContextLenOverflow(self.model.context_len))?;
        let manifest = self
            .coordinator
            .commit_reshard(
                self.model_digest,
                context_len,
                self.model.total_layers,
                &specs,
            )?
            .clone();
        Ok((plan, manifest))
    }

    pub fn plan_and_commit(
        &mut self,
        objective: PlanObjective,
        local_id: Option<&str>,
    ) -> Result<(MultiStagePlan, &PartitionManifest), OrchestratorError> {
        let (plan, _) = self.commit_planned_manifest(objective, local_id)?;
        let manifest = self
            .coordinator
            .current_manifest()
            .ok_or(OrchestratorError::NoActiveManifest)?;
        Ok((plan, manifest))
    }

    pub fn evacuate_and_reshard(
        &mut self,
        leaving_node_id: &str,
        objective: PlanObjective,
        local_id: Option<&str>,
    ) -> Result<(ReshardOutcome, &PartitionManifest), OrchestratorError> {
        let prev = self.coordinator.current_manifest().cloned();
        self.coordinator.begin_evacuation(leaving_node_id)?;
        let (plan, next) = self.commit_planned_manifest(objective, local_id)?;
        self.coordinator.complete_evacuation(leaving_node_id)?;
        let migrations = compute_migrations(prev.as_ref(), &next);
        let outcome = ReshardOutcome {
            previous_epoch: prev.map(|m| m.epoch),
            new_epoch: next.epoch,
            plan,
            migrations,
        };
        Ok((
            outcome,
            self.coordinator
                .current_manifest()
                .ok_or(OrchestratorError::NoActiveManifest)?,
        ))
    }

    pub fn fault_and_reshard(
        &mut self,
        faulted_node_id: &str,
        objective: PlanObjective,
        local_id: Option<&str>,
    ) -> Result<(ReshardOutcome, &PartitionManifest), OrchestratorError> {
        let prev = self.coordinator.current_manifest().cloned();
        self.coordinator.mark_faulted(faulted_node_id)?;
        let (plan, next) = self.commit_planned_manifest(objective, local_id)?;
        let migrations = compute_migrations(prev.as_ref(), &next);
        let outcome = ReshardOutcome {
            previous_epoch: prev.map(|m| m.epoch),
            new_epoch: next.epoch,
            plan,
            migrations,
        };
        Ok((
            outcome,
            self.coordinator
                .current_manifest()
                .ok_or(OrchestratorError::NoActiveManifest)?,
        ))
    }

    pub fn run_loopback_inference<F>(
        &self,
        tokens: &[Vec<u8>],
        stage_fn: F,
    ) -> Result<(Vec<Vec<u8>>, PipelineSummary), OrchestratorError>
    where
        F: Fn(usize, &[u8]) -> Vec<u8> + Send + Sync + 'static,
    {
        let manifest = self
            .coordinator
            .current_manifest()
            .ok_or(OrchestratorError::NoActiveManifest)?;
        Ok(run_loopback_pipeline(manifest, tokens, stage_fn)?)
    }
}
