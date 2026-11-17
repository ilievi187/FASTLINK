//! Deterministic, hardware-neutral admission and placement primitives.
//!
//! This is a planning model, not an inference backend. Rates and latency must
//! come from an operator's measurements; unknown values are never invented.

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeCapability {
    pub id: String,
    pub os: String,
    pub backend: String,
    pub memory_budget_bytes: u64,
    pub model_bytes: u64,
    /// Context-dependent KV/SSM state reserved for this assignment.
    pub context_state_bytes: u64,
    /// Backend workspace/scratch memory needed at peak.
    pub workspace_bytes: u64,
    /// Runtime and allocator overhead reserved for this worker.
    pub runtime_overhead_bytes: u64,
    /// Additional operator-selected headroom that must remain unused.
    pub safety_reserve_bytes: u64,
    /// Measured stage compute time per decoded token, in microseconds.
    pub compute_us_per_token: Option<u64>,
    /// Measured p95 link RTT to the adjacent stage, in microseconds.
    pub adjacent_rtt_p95_us: Option<u64>,
    /// Measured effective adjacent-link throughput in megabits per second.
    pub adjacent_bandwidth_mbps: Option<u64>,
    /// Measured activation bytes crossing the adjacent boundary per token.
    pub activation_bytes_per_token: Option<u64>,
    pub thermal_limited: bool,
    pub opted_in: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlanRejection {
    NotOptedIn,
    MemoryBudget,
    MissingMeasurement,
    SlowerThanLocal,
    CandidateNotSelected,
    ThermalLimit,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeCost {
    pub node_id: String,
    pub stage_us_per_token: u64,
    pub boundary_bytes_per_token: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TopologyPlan {
    pub admitted: Vec<NodeCost>,
    pub excluded: Vec<(String, PlanRejection)>,
    /// Serial pipeline estimate. Not measured inference throughput.
    pub estimated_decode_us_per_token: u64,
    pub total_boundary_bytes_per_token: u64,
    pub evidence: &'static str,
}

fn candidate(node: &NodeCapability) -> Result<NodeCost, PlanRejection> {
    if !node.opted_in {
        return Err(PlanRejection::NotOptedIn);
    }
    if node.model_bytes > node.memory_budget_bytes {
        return Err(PlanRejection::MemoryBudget);
    }
    if node.thermal_limited {
        return Err(PlanRejection::ThermalLimit);
    }
    let required_bytes = node
        .model_bytes
        .saturating_add(node.context_state_bytes)
        .saturating_add(node.workspace_bytes)
        .saturating_add(node.runtime_overhead_bytes)
        .saturating_add(node.safety_reserve_bytes);
    if required_bytes > node.memory_budget_bytes {
        return Err(PlanRejection::MemoryBudget);
    }
    let compute = node
        .compute_us_per_token
        .ok_or(PlanRejection::MissingMeasurement)?;
    let (rtt, bandwidth_mbps, bytes) = match (
        node.adjacent_rtt_p95_us,
        node.adjacent_bandwidth_mbps,
        node.activation_bytes_per_token,
    ) {
        (None, None, None) => (0, None, 0),
        (Some(rtt), Some(bandwidth), Some(bytes)) if bandwidth > 0 => (rtt, Some(bandwidth), bytes),
        _ => return Err(PlanRejection::MissingMeasurement),
    };
    let transfer_us = bandwidth_mbps.map_or(0, |mbps| bytes.saturating_mul(8).div_ceil(mbps));
    Ok(NodeCost {
        node_id: node.id.clone(),
        stage_us_per_token: compute.saturating_add(rtt).saturating_add(transfer_us),
        boundary_bytes_per_token: bytes,
    })
}

/// Select a single-node whole-model candidate against the local fallback.
///
/// This intentionally does NOT compose multi-stage pipelines: the current
/// capability schema has no explicit per-node contiguous layer ranges, so
/// adding stage times here would fabricate a placement. Keep the local plan
/// unless one fully measured remote candidate is faster end-to-end.
pub fn plan_with_local_fallback(nodes: &[NodeCapability], local_id: &str) -> Option<TopologyPlan> {
    let mut candidates = Vec::new();
    let mut excluded = Vec::new();
    for node in nodes {
        match candidate(node) {
            Ok(cost) => candidates.push((node, cost)),
            Err(reason) => excluded.push((node.id.clone(), reason)),
        }
    }
    let local = candidates.iter().find(|(node, _)| node.id == local_id)?;
    let selected = candidates
        .iter()
        .filter(|(node, cost)| {
            node.id != local_id && cost.stage_us_per_token < local.1.stage_us_per_token
        })
        .min_by(|(a, ca), (b, cb)| {
            ca.stage_us_per_token
                .cmp(&cb.stage_us_per_token)
                .then_with(|| a.id.cmp(&b.id))
        });
    let (admitted, total, bytes) = if let Some((node, cost)) = selected {
        for (other, other_cost) in candidates
            .iter()
            .filter(|(candidate, _)| candidate.id != local_id && candidate.id != node.id)
        {
            let reason = if other_cost.stage_us_per_token >= local.1.stage_us_per_token {
                PlanRejection::SlowerThanLocal
            } else {
                PlanRejection::CandidateNotSelected
            };
            excluded.push((other.id.clone(), reason));
        }
        (
            vec![cost.clone()],
            cost.stage_us_per_token,
            cost.boundary_bytes_per_token,
        )
    } else {
        for (other, _) in candidates
            .iter()
            .filter(|(candidate, _)| candidate.id != local_id)
        {
            excluded.push((other.id.clone(), PlanRejection::SlowerThanLocal));
        }
        (vec![local.1.clone()], local.1.stage_us_per_token, 0)
    };
    Some(TopologyPlan {
        admitted,
        excluded,
        estimated_decode_us_per_token: total,
        total_boundary_bytes_per_token: bytes,
        evidence: "predicted-from-declared-measurements; not-inference-benchmark",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str, compute: u64, rtt: Option<u64>, bytes: Option<u64>) -> NodeCapability {
        NodeCapability {
            id: id.into(),
            os: "linux".into(),
            backend: "cpu".into(),
            memory_budget_bytes: 16,
            model_bytes: 8,
            context_state_bytes: 0,
            workspace_bytes: 0,
            runtime_overhead_bytes: 0,
            safety_reserve_bytes: 0,
            compute_us_per_token: Some(compute),
            adjacent_rtt_p95_us: rtt,
            adjacent_bandwidth_mbps: if rtt.is_some() { Some(1_000_000) } else { None },
            activation_bytes_per_token: bytes,
            thermal_limited: false,
            opted_in: true,
        }
    }

    #[test]
    fn only_one_whole_model_candidate_is_selected_until_layer_ranges_exist() {
        let nodes = [
            node("local", 100, None, None),
            node("peer-a", 40, Some(0), Some(512)),
            node("peer-b", 50, Some(0), Some(512)),
        ];
        let plan = plan_with_local_fallback(&nodes, "local").unwrap();
        assert_eq!(
            plan.admitted
                .iter()
                .map(|n| n.node_id.as_str())
                .collect::<Vec<_>>(),
            ["peer-a"]
        );
        assert_eq!(plan.estimated_decode_us_per_token, 41);
        assert!(plan
            .excluded
            .contains(&("peer-b".into(), PlanRejection::CandidateNotSelected)));
    }

    #[test]
    fn measured_equal_or_slower_peer_is_not_reported_as_missing_data() {
        let nodes = [
            node("local", 100, None, None),
            node("slow", 100, Some(5), Some(1)),
        ];
        let plan = plan_with_local_fallback(&nodes, "local").unwrap();
        assert_eq!(
            plan.excluded,
            [("slow".into(), PlanRejection::SlowerThanLocal)]
        );
    }

    #[test]
    fn context_state_workspace_and_reserve_must_fit_memory() {
        let mut constrained = node("constrained", 10, None, None);
        constrained.memory_budget_bytes = 16;
        constrained.model_bytes = 8;
        constrained.context_state_bytes = 4;
        constrained.workspace_bytes = 3;
        constrained.runtime_overhead_bytes = 2;
        constrained.safety_reserve_bytes = 1;
        let nodes = [node("local", 100, None, None), constrained];
        let plan = plan_with_local_fallback(&nodes, "local").unwrap();
        assert!(plan
            .excluded
            .contains(&("constrained".into(), PlanRejection::MemoryBudget)));
    }

    #[test]
    fn transfer_estimate_uses_activation_bytes_and_bandwidth() {
        let mut slow = node("fast-link", 20, Some(10), Some(1024));
        slow.adjacent_bandwidth_mbps = Some(1);
        let nodes = [node("local", 100, None, None), slow];
        let plan = plan_with_local_fallback(&nodes, "local").unwrap();
        assert_eq!(
            plan.admitted
                .iter()
                .map(|n| n.node_id.as_str())
                .collect::<Vec<_>>(),
            ["local"]
        );
        // RTT + serialized transfer: 10us + 1024 bytes at 1 Mbps ~= 8192us.
        assert_eq!(
            plan.excluded,
            [("fast-link".into(), PlanRejection::SlowerThanLocal)]
        );
    }

    #[test]
    fn slow_discovered_peer_does_not_slow_local_decode() {
        let nodes = [
            node("local", 100, None, None),
            node("wifi-phone", 500, Some(300), Some(4096)),
        ];
        let plan = plan_with_local_fallback(&nodes, "local").unwrap();
        assert_eq!(
            plan.admitted
                .iter()
                .map(|x| x.node_id.as_str())
                .collect::<Vec<_>>(),
            ["local"]
        );
        assert_eq!(plan.estimated_decode_us_per_token, 100);
        assert_eq!(plan.total_boundary_bytes_per_token, 0);
    }

    #[test]
    fn infeasible_or_untrusted_nodes_are_excluded() {
        let mut full = node("full", 1, Some(1), Some(32));
        full.memory_budget_bytes = 4;
        let mut guest = node("guest", 1, Some(1), Some(32));
        guest.opted_in = false;
        let nodes = [node("local", 100, None, None), full, guest];
        let plan = plan_with_local_fallback(&nodes, "local").unwrap();
        assert!(plan
            .excluded
            .contains(&("full".into(), PlanRejection::MemoryBudget)));
        assert!(plan
            .excluded
            .contains(&("guest".into(), PlanRejection::NotOptedIn)));
    }

    #[test]
    fn faster_measured_remote_stage_is_admitted_with_accounting() {
        let nodes = [
            node("local", 100, None, None),
            node("worker", 40, Some(5), Some(2048)),
        ];
        let plan = plan_with_local_fallback(&nodes, "local").unwrap();
        assert!(plan.admitted.iter().any(|x| x.node_id == "worker"));
        assert_eq!(plan.estimated_decode_us_per_token, 46);
        assert_eq!(plan.total_boundary_bytes_per_token, 2048);
        assert!(plan.evidence.contains("not-inference-benchmark"));
    }
}
