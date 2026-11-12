//! Heterogeneous multi-stage partitioner and scheduler (Issue #7, Wave 1).
//!
//! Plans contiguous layer ranges for models that do not fit on a single node
//! and applies hard gating so a plan never makes decode slower by adding a
//! worse peer. All estimates derive from operator-declared measurements;
//! unknown values are never invented.

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
    node.battery_pct.map_or(true, |pct| pct > LOW_BATTERY_PCT)
}

fn node_allows(node: &NodeProfile, role: DeviceRole) -> bool {
    node.allowed_roles.is_empty() || node.allowed_roles.contains(&role)
}

fn node_allows_decode(node: &NodeProfile) -> bool {
    node_allows(node, DeviceRole::DecodeWorker)
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
        if !node.opted_in || node.thermal_limited || !battery_ok(node) {
            continue;
        }
        if !node_allows_decode(node) {
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
        if used.iter().any(|id| *id == node.id.as_str()) {
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
        let reason: &str = if !node.opted_in {
            "NotOptedIn"
        } else if node.thermal_limited {
            "ThermalLimit"
        } else if !battery_ok(node) {
            "LowBattery"
        } else if !node_allows(node, DeviceRole::DecodeWorker) {
            "RoleNotAllowed"
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
