//! Heterogeneous multi-stage partitioner and scheduler (Issue #7, Wave 1),
//! plus CPU-only MoE expert placement and top-k routing planning.
//!
//! Plans contiguous layer ranges or MoE expert placements from
//! operator-declared hardware measurements. This module implements deterministic
//! planning only (not model loading, inference execution, or benchmarking);
//! unknown compute/network values remain `None`/unknown and are never invented.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};

/// Logical device role considered by the heterogeneous scheduler.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum DeviceRole {
    DecodeWorker,
    PrefillWorker,
    DraftWorker,
    CacheStaging,
    Guest,
}

impl DeviceRole {
    pub fn as_str(self) -> &'static str {
        match self {
            DeviceRole::DecodeWorker => "DecodeWorker",
            DeviceRole::PrefillWorker => "PrefillWorker",
            DeviceRole::DraftWorker => "DraftWorker",
            DeviceRole::CacheStaging => "CacheStaging",
            DeviceRole::Guest => "Guest",
        }
    }
}

/// Optimization objective for a staged plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlanObjective {
    /// Minimize the number of stages by maximizing the largest viable stage.
    LargestModel,
    /// Minimize the staged decode critical-path latency.
    LowestLatency,
    /// Maximize aggregate decode throughput.
    MaxThroughput,
}

impl PlanObjective {
    pub fn as_str(self) -> &'static str {
        match self {
            PlanObjective::LargestModel => "LargestModel",
            PlanObjective::LowestLatency => "LowestLatency",
            PlanObjective::MaxThroughput => "MaxThroughput",
        }
    }
}

/// Immutable specification of a model to partition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelSpec {
    pub model_id: String,
    pub total_layers: u32,
    pub layer_weights_bytes: u64,
    pub kv_bytes_per_layer_per_token: u64,
    pub activation_bytes_per_token: u64,
    pub context_len: u64,
    pub min_usable_headroom_bytes: u64,
}

impl ModelSpec {
    pub fn weights_bytes(&self, layers: u32) -> u64 {
        self.layer_weights_bytes.saturating_mul(u64::from(layers))
    }

    pub fn kv_bytes(&self, layers: u32) -> u64 {
        self.kv_bytes_per_layer_per_token
            .saturating_mul(self.context_len)
            .saturating_mul(u64::from(layers))
    }
}

/// Operator-known hardware profile for one node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeProfile {
    pub id: String,
    pub os: String,
    pub backend: String,
    pub memory_budget_bytes: u64,
    pub workspace_bytes: u64,
    pub runtime_overhead_bytes: u64,
    pub safety_reserve_bytes: u64,
    pub per_layer_compute_us: Option<u64>,
    pub adjacent_rtt_p95_us: Option<u64>,
    pub adjacent_bandwidth_mbps: Option<u64>,
    pub thermal_limited: bool,
    pub opted_in: bool,
    pub is_mobile: bool,
    pub battery_pct: Option<u8>,
    pub allowed_roles: Vec<DeviceRole>,
}

/// Contiguous layer range assigned to a single device.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StageAssignment {
    pub node_id: String,
    pub role: DeviceRole,
    pub layer_start: u32,
    pub layer_end: u32,
    pub weights_bytes: u64,
    pub kv_bytes: u64,
    pub estimated_stage_us: u64,
    pub boundary_bytes: u64,
}

/// Result of a heterogeneous staging plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MultiStagePlan {
    pub objective: PlanObjective,
    pub stages: Vec<StageAssignment>,
    pub aux_roles: Vec<(String, DeviceRole)>,
    pub excluded: Vec<(String, String)>,
    pub critical_path_us_per_token: u64,
    pub total_boundary_bytes: u64,
    pub fits_model: bool,
}

const GIB: u64 = 1024 * 1024 * 1024;
const LOW_BATTERY_PCT: u8 = 15;

fn battery_ok(node: &NodeProfile) -> bool {
    node.battery_pct.is_none_or(|pct| pct > LOW_BATTERY_PCT)
}

fn node_allows(node: &NodeProfile, role: DeviceRole) -> bool {
    node.allowed_roles.is_empty() || node.allowed_roles.contains(&role)
}

fn node_allows_decode(node: &NodeProfile) -> bool {
    node_allows(node, DeviceRole::DecodeWorker)
}

fn eligibility_exclusion(node: &NodeProfile) -> Option<&'static str> {
    if !node.opted_in {
        Some("NotOptedIn")
    } else if node.thermal_limited {
        Some("ThermalLimit")
    } else if !battery_ok(node) {
        Some("LowBattery")
    } else if !node_allows_decode(node) {
        Some("RoleNotAllowed")
    } else {
        None
    }
}

fn aux_allowed(node: &NodeProfile) -> bool {
    node.opted_in && !node.thermal_limited && battery_ok(node) && {
        node.allowed_roles.is_empty()
            || node
                .allowed_roles
                .iter()
                .any(|r| matches!(*r, DeviceRole::DraftWorker | DeviceRole::CacheStaging))
    }
}

fn aux_role_for(node: &NodeProfile) -> DeviceRole {
    if node.allowed_roles.contains(&DeviceRole::DraftWorker) {
        DeviceRole::DraftWorker
    } else {
        DeviceRole::CacheStaging
    }
}

fn stage_cost(
    model: &ModelSpec,
    node: &NodeProfile,
    is_local: bool,
    layers: u32,
) -> Option<(u64, u64)> {
    let compute = node.per_layer_compute_us?;
    let stage_us = compute.saturating_mul(u64::from(layers));
    if is_local {
        return Some((stage_us, 0));
    }
    let rtt = node.adjacent_rtt_p95_us?;
    let bandwidth_mbps = node.adjacent_bandwidth_mbps?;
    if bandwidth_mbps == 0 {
        return None;
    }
    let transfer_us = model
        .activation_bytes_per_token
        .saturating_mul(8)
        .div_ceil(bandwidth_mbps);
    Some((
        stage_us.saturating_add(rtt).saturating_add(transfer_us),
        model.activation_bytes_per_token,
    ))
}

fn stage_fits(model: &ModelSpec, node: &NodeProfile, layers: u32) -> bool {
    let required = model
        .weights_bytes(layers)
        .saturating_add(model.kv_bytes(layers))
        .saturating_add(node.workspace_bytes)
        .saturating_add(node.runtime_overhead_bytes)
        .saturating_add(node.safety_reserve_bytes)
        .saturating_add(model.min_usable_headroom_bytes);
    required <= node.memory_budget_bytes
}

fn max_fittable_layers(model: &ModelSpec, node: &NodeProfile) -> u32 {
    if model.total_layers == 0 {
        return 0;
    }
    for layers in (1..=model.total_layers).rev() {
        if stage_fits(model, node, layers) {
            return layers;
        }
    }
    0
}

struct Candidate<'a> {
    node: &'a NodeProfile,
    is_local: bool,
    max_fittable_layers: u32,
    stage_us: u64,
}

pub fn plan_heterogeneous_stages(
    model: &ModelSpec,
    nodes: &[NodeProfile],
    objective: PlanObjective,
    local_id: Option<&str>,
) -> MultiStagePlan {
    let local_node = local_id.and_then(|id| nodes.iter().find(|n| n.id == id));

    let mut candidates: Vec<Candidate> = Vec::new();
    for node in nodes {
        let is_local = local_node.map(|n| n.id == node.id).unwrap_or(false);
        if eligibility_exclusion(node).is_some() {
            continue;
        }
        let max_layers = max_fittable_layers(model, node);
        let (stage_us, _boundary_bytes) =
            stage_cost(model, node, is_local, max_layers).unwrap_or((0, 0));
        candidates.push(Candidate {
            node,
            is_local,
            max_fittable_layers: max_layers,
            stage_us,
        });
    }

    candidates.sort_by(|a, b| {
        let a_key = (a.stage_us, a.node.id.clone());
        let b_key = (b.stage_us, b.node.id.clone());
        if matches!(objective, PlanObjective::LargestModel) {
            b.max_fittable_layers
                .cmp(&a.max_fittable_layers)
                .then_with(|| a_key.cmp(&b_key))
        } else {
            a_key.cmp(&b_key)
        }
    });

    let mut stages: Vec<StageAssignment> = Vec::new();
    let mut cursor: u32 = 0;
    for candidate in &candidates {
        if cursor >= model.total_layers {
            break;
        }
        if candidate.max_fittable_layers == 0 {
            continue;
        }
        let remaining = model.total_layers - cursor;
        let take = candidate.max_fittable_layers.min(remaining);
        let (stage_us, boundary_bytes) =
            stage_cost(model, candidate.node, candidate.is_local, take).unwrap_or((0, 0));
        stages.push(StageAssignment {
            node_id: candidate.node.id.clone(),
            role: DeviceRole::DecodeWorker,
            layer_start: cursor,
            layer_end: cursor + take,
            weights_bytes: model.weights_bytes(take),
            kv_bytes: model.kv_bytes(take),
            estimated_stage_us: stage_us,
            boundary_bytes,
        });
        cursor += take;
    }

    let fits_model = cursor >= model.total_layers;

    let mut critical_path_us_per_token: u64 = 0;
    let mut total_boundary_bytes: u64 = 0;
    for stage in &stages {
        critical_path_us_per_token =
            critical_path_us_per_token.saturating_add(stage.estimated_stage_us);
        total_boundary_bytes = total_boundary_bytes.saturating_add(stage.boundary_bytes);
    }

    let mut aux_roles: Vec<(String, DeviceRole)> = Vec::new();
    let mut excluded: Vec<(String, String)> = Vec::new();
    let used: Vec<&str> = stages.iter().map(|s| s.node_id.as_str()).collect();

    for node in nodes {
        if used.contains(&node.id.as_str()) {
            continue;
        }
        let mut assign_aux = false;
        if node.opted_in
            && !node.thermal_limited
            && battery_ok(node)
            && node_allows_decode(node)
            && node.per_layer_compute_us.is_some()
        {
            assign_aux = aux_allowed(node) && max_fittable_layers(model, node) > 0;
        }
        if assign_aux {
            aux_roles.push((node.id.clone(), aux_role_for(node)));
            continue;
        }
        let reason: &str = if let Some(exclusion) = eligibility_exclusion(node) {
            exclusion
        } else if node.per_layer_compute_us.is_none() {
            "MissingMeasurement"
        } else if max_fittable_layers(model, node) == 0 {
            "NoMemoryFit"
        } else {
            "CandidateNotSelected"
        };
        excluded.push((node.id.clone(), reason.to_string()));
    }

    MultiStagePlan {
        objective,
        stages,
        aux_roles,
        excluded,
        critical_path_us_per_token,
        total_boundary_bytes,
        fits_model,
    }
}

fn synth_profile(
    id: &str,
    memory_budget_bytes: u64,
    per_layer_compute_us: Option<u64>,
    adjacent_rtt_p95_us: Option<u64>,
    adjacent_bandwidth_mbps: Option<u64>,
    _activation_bytes_per_token: u64,
) -> NodeProfile {
    NodeProfile {
        id: id.to_string(),
        os: "linux".to_string(),
        backend: "cpu".to_string(),
        memory_budget_bytes,
        workspace_bytes: 256 * 1024 * 1024,
        runtime_overhead_bytes: 128 * 1024 * 1024,
        safety_reserve_bytes: 128 * 1024 * 1024,
        per_layer_compute_us,
        adjacent_rtt_p95_us,
        adjacent_bandwidth_mbps,
        thermal_limited: false,
        opted_in: true,
        is_mobile: false,
        battery_pct: None,
        allowed_roles: Vec::new(),
    }
}

/// Synthetic single-node cluster used as a baseline.
pub fn sample_profiles_1() -> Vec<NodeProfile> {
    vec![synth_profile(
        "synth-local-1",
        16 * GIB,
        Some(40),
        None,
        None,
        0,
    )]
}

/// Synthetic two-node cluster: local host plus one fast opted-in peer.
pub fn sample_profiles_2() -> Vec<NodeProfile> {
    let local = synth_profile("synth-local-2", 16 * GIB, Some(60), None, None, 0);
    let peer = synth_profile(
        "synth-peer-2",
        16 * GIB,
        Some(30),
        Some(2),
        Some(1_000),
        4096,
    );
    vec![local, peer]
}

/// Synthetic three-node cluster: local host, fast peer, slow-WiFi phone.
pub fn sample_profiles_3() -> Vec<NodeProfile> {
    let local = synth_profile("synth-local-3", 16 * GIB, Some(60), None, None, 0);
    let peer = synth_profile(
        "synth-peer-3",
        16 * GIB,
        Some(30),
        Some(2),
        Some(1_000),
        4096,
    );
    let mut phone = synth_profile(
        "synth-phone-3",
        8 * GIB,
        Some(900),
        Some(280),
        Some(60),
        8192,
    );
    phone.is_mobile = true;
    phone.battery_pct = Some(80);
    phone.allowed_roles = vec![
        DeviceRole::DecodeWorker,
        DeviceRole::DraftWorker,
        DeviceRole::CacheStaging,
    ];
    vec![local, peer, phone]
}

/// Synthetic four-node heterogeneous cluster for memory-fit planning.
pub fn sample_profiles_4() -> Vec<NodeProfile> {
    vec![
        synth_profile(
            "synth-node-a",
            48 * GIB,
            Some(25),
            Some(3),
            Some(5_000),
            2_000_000,
        ),
        synth_profile(
            "synth-node-b",
            64 * GIB,
            Some(22),
            Some(4),
            Some(4_000),
            2_000_000,
        ),
        synth_profile(
            "synth-node-c",
            96 * GIB,
            Some(18),
            Some(5),
            Some(3_000),
            2_000_000,
        ),
        synth_profile(
            "synth-node-d",
            128 * GIB,
            Some(14),
            Some(6),
            Some(2_000),
            2_000_000,
        ),
    ]
}

/// Synthetic seven-node cluster for memory-fit planning.
pub fn sample_profiles_7() -> Vec<NodeProfile> {
    let mut profiles = Vec::new();
    for i in 1..=7u64 {
        profiles.push(synth_profile(
            &format!("synth-node-{i:02}"),
            64 * GIB,
            Some(40 - (i % 8) * 3),
            Some(2),
            Some(2_000),
            1_000_000,
        ));
    }
    profiles
}

/// Synthetic sixteen-node cluster for memory-fit planning.
pub fn sample_profiles_16() -> Vec<NodeProfile> {
    let mut profiles = Vec::new();
    for i in 1..=16u64 {
        profiles.push(synth_profile(
            &format!("synth-node-{i:02}"),
            24 * GIB + i * GIB,
            Some(40 - (i % 8) * 3),
            Some(2),
            Some(2_000),
            1_000_000,
        ));
    }
    profiles
}

const MAX_PLANNABLE_EXPERTS: u32 = 65_536;
const BALANCED_MOE_ASSUMPTION: &str = "Static placement plan only (not an MoE inference runner or throughput benchmark); assumes uniform expert popularity (1/total_experts) with no runtime trace.";
const MIN_MOVEMENT_MOE_ASSUMPTION: &str = "Static placement plan only (not an MoE inference runner or throughput benchmark); packs local-first and largest-capacity eligible nodes from operator-declared capacity.";

/// Optimization objective for static CPU-only MoE expert placement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MoePlanObjective {
    /// Spread experts as evenly as possible across eligible nodes up to capacity.
    /// Assumes uniform expert popularity (`1 / total_experts` each) in v1 because no
    /// runtime activation trace is available.
    BalancedUniformPopularity,
    /// Pack experts onto the local node first (when eligible), then fewest/largest
    /// eligible peers to minimize cross-node movement.
    MinimizeCrossNodeMovement,
}

impl MoePlanObjective {
    /// Return a stable string identifier for this objective.
    pub fn as_str(self) -> &'static str {
        match self {
            MoePlanObjective::BalancedUniformPopularity => "BalancedUniformPopularity",
            MoePlanObjective::MinimizeCrossNodeMovement => "MinimizeCrossNodeMovement",
        }
    }

    fn assumption_note(self) -> &'static str {
        match self {
            MoePlanObjective::BalancedUniformPopularity => BALANCED_MOE_ASSUMPTION,
            MoePlanObjective::MinimizeCrossNodeMovement => MIN_MOVEMENT_MOE_ASSUMPTION,
        }
    }
}

/// Immutable operator-declared Mixture-of-Experts specification for static placement planning.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MoeModelSpec {
    pub model_id: String,
    /// Total count of distinct routable experts (`0..total_experts`).
    pub total_experts: u32,
    /// Maximum experts selected per routing step (`top-k`).
    pub active_top_k: u32,
    /// Weight memory required per assigned expert, in bytes.
    pub expert_weights_bytes: u64,
    /// Shared base/attention/router weights required on each participating node, in bytes.
    pub shared_weights_bytes: u64,
    /// Minimum unallocated headroom preserved on every participating node, in bytes.
    pub min_usable_headroom_bytes: u64,
}

impl MoeModelSpec {
    /// Total weight bytes for `experts` placed experts plus `shared_weights_bytes`.
    pub fn weights_bytes_for_experts(&self, experts: u32) -> u64 {
        self.shared_weights_bytes
            .saturating_add(self.expert_weights_bytes.saturating_mul(u64::from(experts)))
    }
}

/// Contiguous slice of MoE experts assigned to a single opted-in eligible node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MoeNodeAssignment {
    pub node_id: String,
    pub is_local: bool,
    pub role: DeviceRole,
    pub expert_ids: Vec<u32>,
    pub weights_bytes: u64,
    pub reserved_overhead_bytes: u64,
    pub remaining_budget_bytes: u64,
    pub max_fittable_experts: u32,
    /// Operator-declared per-layer compute time if known (`None` when unknown).
    pub reported_per_layer_compute_us: Option<u64>,
    /// Operator-declared p95 link RTT if known (`None` when unknown).
    pub reported_adjacent_rtt_p95_us: Option<u64>,
    /// Operator-declared link bandwidth in Mbps if known (`None` when unknown).
    pub reported_adjacent_bandwidth_mbps: Option<u64>,
}

/// Result of deterministic CPU-only MoE placement planning.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MoePlacementPlan {
    pub model_id: String,
    pub objective: MoePlanObjective,
    pub assumption_note: &'static str,
    pub total_experts: u32,
    pub active_top_k: u32,
    pub local_id: Option<String>,
    pub assignments: Vec<MoeNodeAssignment>,
    /// Direct `expert_id -> node_id` lookup table when `fits_model` is `true`; empty otherwise.
    pub expert_to_node: Vec<String>,
    pub excluded: Vec<(String, String)>,
    pub total_capacity_experts: u32,
    pub assigned_experts: u32,
    pub known_compute_nodes: usize,
    pub unknown_compute_nodes: usize,
    pub known_network_nodes: usize,
    pub unknown_network_nodes: usize,
    pub fits_model: bool,
    pub infeasible_reason: Option<String>,
}

/// Single destination mapping produced by [`route_moe_topk`] for a selected expert ID.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExpertRoute {
    pub expert_id: u32,
    pub node_id: String,
    pub is_local: bool,
    /// Operator-reported link RTT p95 in microseconds if known (`None` when unknown).
    pub reported_rtt_p95_us: Option<u64>,
    /// Operator-reported link bandwidth in Mbps if known (`None` when unknown).
    pub reported_bandwidth_mbps: Option<u64>,
}

/// Rejection reasons returned by [`route_moe_topk`] when routing input or plan state is invalid.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MoeRouteError {
    InfeasiblePlan,
    EmptySelection,
    TopKOverflow { selected: usize, max_top_k: u32 },
    InvalidExpertId { expert_id: u32, total_experts: u32 },
    DuplicateExpertSelection { expert_id: u32 },
    UnassignedExpert { expert_id: u32 },
}

fn validate_moe_spec(model: &MoeModelSpec) -> Option<String> {
    if model.total_experts == 0 {
        return Some("InvalidModelSpec: total_experts must be greater than zero".to_string());
    }
    if model.total_experts > MAX_PLANNABLE_EXPERTS {
        return Some(format!(
            "InvalidModelSpec: total_experts {} exceeds limit {}",
            model.total_experts, MAX_PLANNABLE_EXPERTS
        ));
    }
    if model.active_top_k == 0 || model.active_top_k > model.total_experts {
        return Some("InvalidModelSpec: active_top_k must be in 1..=total_experts".to_string());
    }
    None
}

fn node_reserved_overhead_bytes(model: &MoeModelSpec, node: &NodeProfile) -> u64 {
    node.workspace_bytes
        .saturating_add(node.runtime_overhead_bytes)
        .saturating_add(node.safety_reserve_bytes)
        .saturating_add(model.min_usable_headroom_bytes)
}

fn moe_fixed_memory_bytes(model: &MoeModelSpec, node: &NodeProfile) -> u64 {
    model
        .shared_weights_bytes
        .saturating_add(node_reserved_overhead_bytes(model, node))
}

fn max_fittable_experts(model: &MoeModelSpec, node: &NodeProfile) -> u32 {
    let fixed = moe_fixed_memory_bytes(model, node);
    let Some(available_for_experts) = node.memory_budget_bytes.checked_sub(fixed) else {
        return 0;
    };
    if model.expert_weights_bytes == 0 {
        return model.total_experts;
    }
    let fit = available_for_experts / model.expert_weights_bytes;
    u32::try_from(fit)
        .unwrap_or(u32::MAX)
        .min(model.total_experts)
}

/// Rank known compute/RTT lower first, while keeping unknown (`None`) strictly after measured
/// nodes instead of inventing a `0 us` measurement.
fn latency_rank(node: &NodeProfile, is_local: bool) -> (u8, u64) {
    match (
        node.per_layer_compute_us,
        is_local,
        node.adjacent_rtt_p95_us,
    ) {
        (Some(compute), true, _) => (0, compute),
        (Some(compute), false, Some(rtt)) => (0, compute.saturating_add(rtt)),
        _ => (1, 0),
    }
}

fn allocate_balanced_counts(capacities: &[u32], total_experts: u32) -> Vec<u32> {
    let mut counts = vec![0u32; capacities.len()];
    let mut remaining = total_experts;
    while remaining > 0 {
        let mut active = 0usize;
        let mut min_rem = u32::MAX;
        for (&cap, &used) in capacities.iter().zip(&counts) {
            if used < cap {
                active += 1;
                min_rem = min_rem.min(cap - used);
            }
        }
        if active == 0 {
            break;
        }
        let batch = (remaining / active as u32).min(min_rem);
        if batch > 0 {
            for (&cap, used) in capacities.iter().zip(&mut counts) {
                if *used < cap {
                    *used += batch;
                }
            }
            remaining -= batch * active as u32;
        } else {
            for (&cap, used) in capacities.iter().zip(&mut counts) {
                if *used < cap {
                    *used += 1;
                    remaining -= 1;
                    if remaining == 0 {
                        break;
                    }
                }
            }
        }
    }
    counts
}

fn allocate_packed_counts(capacities: &[u32], total_experts: u32) -> Vec<u32> {
    let mut counts = vec![0u32; capacities.len()];
    let mut remaining = total_experts;
    for (&cap, used) in capacities.iter().zip(&mut counts) {
        if remaining == 0 {
            break;
        }
        let take = cap.min(remaining);
        *used = take;
        remaining -= take;
    }
    counts
}

struct MoeCandidate<'a> {
    node: &'a NodeProfile,
    is_local: bool,
    max_experts: u32,
}

#[derive(Clone, Copy)]
struct EvidenceCounts {
    known_compute: usize,
    unknown_compute: usize,
    known_network: usize,
    unknown_network: usize,
}

fn collect_moe_candidates<'a>(
    model: &MoeModelSpec,
    nodes: &'a [NodeProfile],
    local_id: Option<&str>,
) -> (Vec<MoeCandidate<'a>>, Vec<(String, String)>, bool) {
    let mut candidates = Vec::new();
    let mut excluded = Vec::new();
    let mut seen_ids = BTreeSet::new();
    let mut has_duplicate_id = false;

    for node in nodes {
        if node.id.trim().is_empty() {
            excluded.push((node.id.clone(), "InvalidNodeId".to_string()));
            continue;
        }
        if !seen_ids.insert(node.id.as_str()) {
            excluded.push((node.id.clone(), "DuplicateNodeId".to_string()));
            has_duplicate_id = true;
            continue;
        }
        if let Some(reason) = eligibility_exclusion(node) {
            excluded.push((node.id.clone(), reason.to_string()));
            continue;
        }
        let max_experts = max_fittable_experts(model, node);
        if max_experts == 0 {
            excluded.push((node.id.clone(), "NoMemoryFit".to_string()));
            continue;
        }
        let is_local = local_id.is_some_and(|id| id == node.id);
        candidates.push(MoeCandidate {
            node,
            is_local,
            max_experts,
        });
    }

    (candidates, excluded, has_duplicate_id)
}

fn count_evidence(candidates: &[MoeCandidate<'_>]) -> EvidenceCounts {
    let known_compute = candidates
        .iter()
        .filter(|c| c.node.per_layer_compute_us.is_some())
        .count();
    let known_network = candidates
        .iter()
        .filter(|c| {
            c.node.adjacent_rtt_p95_us.is_some() && c.node.adjacent_bandwidth_mbps.is_some()
        })
        .count();
    EvidenceCounts {
        known_compute,
        unknown_compute: candidates.len().saturating_sub(known_compute),
        known_network,
        unknown_network: candidates.len().saturating_sub(known_network),
    }
}

fn infeasible_moe_plan(
    model: &MoeModelSpec,
    objective: MoePlanObjective,
    local_id: Option<&str>,
    mut excluded: Vec<(String, String)>,
    total_capacity_experts: u32,
    evidence: EvidenceCounts,
    reason: String,
) -> MoePlacementPlan {
    excluded.sort();
    MoePlacementPlan {
        model_id: model.model_id.clone(),
        objective,
        assumption_note: objective.assumption_note(),
        total_experts: model.total_experts,
        active_top_k: model.active_top_k,
        local_id: local_id.map(str::to_string),
        assignments: Vec::new(),
        expert_to_node: Vec::new(),
        excluded,
        total_capacity_experts,
        assigned_experts: 0,
        known_compute_nodes: evidence.known_compute,
        unknown_compute_nodes: evidence.unknown_compute,
        known_network_nodes: evidence.known_network,
        unknown_network_nodes: evidence.unknown_network,
        fits_model: false,
        infeasible_reason: Some(reason),
    }
}

fn build_moe_assignments(
    model: &MoeModelSpec,
    candidates: &[MoeCandidate<'_>],
    counts: &[u32],
    excluded: &mut Vec<(String, String)>,
) -> (Vec<MoeNodeAssignment>, Vec<String>, u32) {
    let mut assignments = Vec::new();
    let mut expert_to_node = vec![String::new(); model.total_experts as usize];
    let mut cursor: u32 = 0;

    for (candidate, &count) in candidates.iter().zip(counts) {
        if count == 0 {
            excluded.push((
                candidate.node.id.clone(),
                "CandidateNotSelected".to_string(),
            ));
            continue;
        }
        let end = cursor + count;
        let expert_ids: Vec<u32> = (cursor..end).collect();
        for &expert_id in &expert_ids {
            expert_to_node[expert_id as usize] = candidate.node.id.clone();
        }
        cursor = end;

        let weights_bytes = model.weights_bytes_for_experts(count);
        let reserved_overhead_bytes = node_reserved_overhead_bytes(model, candidate.node);
        let used_bytes = weights_bytes.saturating_add(reserved_overhead_bytes);
        let remaining_budget_bytes = candidate
            .node
            .memory_budget_bytes
            .saturating_sub(used_bytes);

        assignments.push(MoeNodeAssignment {
            node_id: candidate.node.id.clone(),
            is_local: candidate.is_local,
            role: DeviceRole::DecodeWorker,
            expert_ids,
            weights_bytes,
            reserved_overhead_bytes,
            remaining_budget_bytes,
            max_fittable_experts: candidate.max_experts,
            reported_per_layer_compute_us: candidate.node.per_layer_compute_us,
            reported_adjacent_rtt_p95_us: candidate.node.adjacent_rtt_p95_us,
            reported_adjacent_bandwidth_mbps: candidate.node.adjacent_bandwidth_mbps,
        });
    }

    (assignments, expert_to_node, cursor)
}

/// Compute a deterministic CPU-only placement plan assigning `0..model.total_experts`
/// to opted-in eligible nodes without inventing unknown measurements or running inference.
pub fn plan_moe_experts(
    model: &MoeModelSpec,
    nodes: &[NodeProfile],
    objective: MoePlanObjective,
    local_id: Option<&str>,
) -> MoePlacementPlan {
    let (mut candidates, mut excluded, has_duplicate_id) =
        collect_moe_candidates(model, nodes, local_id);
    let evidence = count_evidence(&candidates);
    let total_capacity_experts = candidates
        .iter()
        .fold(0u32, |acc, c| acc.saturating_add(c.max_experts));

    if let Some(spec_err) = validate_moe_spec(model) {
        return infeasible_moe_plan(
            model,
            objective,
            local_id,
            excluded,
            total_capacity_experts,
            evidence,
            spec_err,
        );
    }

    if has_duplicate_id {
        return infeasible_moe_plan(
            model,
            objective,
            local_id,
            excluded,
            total_capacity_experts,
            evidence,
            "DuplicateNodeId: node IDs must be unique".to_string(),
        );
    }

    if candidates.is_empty() {
        return infeasible_moe_plan(
            model,
            objective,
            local_id,
            excluded,
            0,
            evidence,
            "NoEligibleNodes: no opted-in eligible nodes can fit an expert".to_string(),
        );
    }

    if total_capacity_experts < model.total_experts {
        return infeasible_moe_plan(
            model,
            objective,
            local_id,
            excluded,
            total_capacity_experts,
            evidence,
            format!(
                "InsufficientMemoryCapacity: total capacity {} experts is below required {}",
                total_capacity_experts, model.total_experts
            ),
        );
    }

    candidates.sort_by(|a, b| match objective {
        MoePlanObjective::BalancedUniformPopularity => (!a.is_local)
            .cmp(&(!b.is_local))
            .then_with(|| latency_rank(a.node, a.is_local).cmp(&latency_rank(b.node, b.is_local)))
            .then_with(|| Reverse(a.max_experts).cmp(&Reverse(b.max_experts)))
            .then_with(|| a.node.id.cmp(&b.node.id)),
        MoePlanObjective::MinimizeCrossNodeMovement => (!a.is_local)
            .cmp(&(!b.is_local))
            .then_with(|| Reverse(a.max_experts).cmp(&Reverse(b.max_experts)))
            .then_with(|| latency_rank(a.node, a.is_local).cmp(&latency_rank(b.node, b.is_local)))
            .then_with(|| a.node.id.cmp(&b.node.id)),
    });

    let capacities: Vec<u32> = candidates.iter().map(|c| c.max_experts).collect();
    let counts = match objective {
        MoePlanObjective::BalancedUniformPopularity => {
            allocate_balanced_counts(&capacities, model.total_experts)
        }
        MoePlanObjective::MinimizeCrossNodeMovement => {
            allocate_packed_counts(&capacities, model.total_experts)
        }
    };

    let (assignments, expert_to_node, assigned_experts) =
        build_moe_assignments(model, &candidates, &counts, &mut excluded);
    excluded.sort();

    MoePlacementPlan {
        model_id: model.model_id.clone(),
        objective,
        assumption_note: objective.assumption_note(),
        total_experts: model.total_experts,
        active_top_k: model.active_top_k,
        local_id: local_id.map(str::to_string),
        assignments,
        expert_to_node,
        excluded,
        total_capacity_experts,
        assigned_experts,
        known_compute_nodes: evidence.known_compute,
        unknown_compute_nodes: evidence.unknown_compute,
        known_network_nodes: evidence.known_network,
        unknown_network_nodes: evidence.unknown_network,
        fits_model: assigned_experts == model.total_experts,
        infeasible_reason: None,
    }
}

/// Map a caller-provided top-k selection of expert IDs to destination node routes.
///
/// Validates plan feasibility, non-empty selection bounded by `plan.active_top_k`,
/// expert ID bounds (`0..plan.total_experts`), and duplicate selections. Unknown
/// network evidence remains `None`.
pub fn route_moe_topk(
    plan: &MoePlacementPlan,
    selected_experts: &[u32],
) -> Result<Vec<ExpertRoute>, MoeRouteError> {
    if !plan.fits_model || plan.expert_to_node.len() != plan.total_experts as usize {
        return Err(MoeRouteError::InfeasiblePlan);
    }
    if selected_experts.is_empty() {
        return Err(MoeRouteError::EmptySelection);
    }
    if selected_experts.len() > plan.active_top_k as usize {
        return Err(MoeRouteError::TopKOverflow {
            selected: selected_experts.len(),
            max_top_k: plan.active_top_k,
        });
    }

    let by_node: BTreeMap<&str, &MoeNodeAssignment> = plan
        .assignments
        .iter()
        .map(|a| (a.node_id.as_str(), a))
        .collect();
    let mut seen = BTreeSet::new();
    let mut routes = Vec::with_capacity(selected_experts.len());

    for &expert_id in selected_experts {
        if expert_id >= plan.total_experts {
            return Err(MoeRouteError::InvalidExpertId {
                expert_id,
                total_experts: plan.total_experts,
            });
        }
        if !seen.insert(expert_id) {
            return Err(MoeRouteError::DuplicateExpertSelection { expert_id });
        }
        let node_id = plan
            .expert_to_node
            .get(expert_id as usize)
            .filter(|id| !id.is_empty())
            .ok_or(MoeRouteError::UnassignedExpert { expert_id })?;
        let assignment = by_node
            .get(node_id.as_str())
            .ok_or(MoeRouteError::UnassignedExpert { expert_id })?;
        routes.push(ExpertRoute {
            expert_id,
            node_id: assignment.node_id.clone(),
            is_local: assignment.is_local,
            reported_rtt_p95_us: assignment.reported_adjacent_rtt_p95_us,
            reported_bandwidth_mbps: assignment.reported_adjacent_bandwidth_mbps,
        });
    }

    Ok(routes)
}
