//! Deterministic, hardware-neutral admission and placement primitives.
//!
//! This is a planning model, not an inference backend. Rates and latency must
//! come from an operator's measurements; unknown values are never invented.

use std::cmp::Ordering;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeCapability {
    pub id: String,
    pub os: String,
    pub backend: String,
    pub memory_budget_bytes: u64,
    /// Estimated resident model bytes assigned to this node.
    pub model_bytes: u64,
    /// Measured stage compute time per decoded token, in microseconds.
    pub compute_us_per_token: Option<u64>,
    /// Measured p95 link RTT to the adjacent stage, in microseconds.
    pub adjacent_rtt_p95_us: Option<u64>,
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
    let compute = node
        .compute_us_per_token
        .ok_or(PlanRejection::MissingMeasurement)?;
    let (rtt, bytes) = match (node.adjacent_rtt_p95_us, node.activation_bytes_per_token) {
        (Some(rtt), Some(bytes)) => (rtt, bytes),
        (None, None) => (0, 0),
        _ => return Err(PlanRejection::MissingMeasurement),
    };
    Ok(NodeCost {
        node_id: node.id.clone(),
        stage_us_per_token: compute.saturating_add(rtt),
        boundary_bytes_per_token: bytes,
    })
}

/// Select a latency-critical pipeline only when it improves on the mandatory
/// local baseline. Additional discovered peers are optional and deterministic.
pub fn plan_with_local_fallback(nodes: &[NodeCapability], local_id: &str) -> Option<TopologyPlan> {
    let mut candidates = Vec::new();
    let mut excluded = Vec::new();
    for node in nodes {
        match candidate(node) {
            Ok(cost) => candidates.push((node, cost)),
            Err(reason) => excluded.push((node.id.clone(), reason)),
        }
    }
    candidates.sort_by(|(a, ca), (b, cb)| {
        ca.stage_us_per_token
            .cmp(&cb.stage_us_per_token)
            .then_with(|| a.id.cmp(&b.id))
    });
    let local = candidates.iter().find(|(node, _)| node.id == local_id)?;
    let mut admitted = vec![local.1.clone()];
    let mut total = local.1.stage_us_per_token;
    let mut bytes = 0u64;

    // This first admission primitive only accepts an additional stage if it
    // reduces measured critical-path latency versus keeping work local.
    for (node, cost) in candidates.iter().filter(|(node, _)| node.id != local_id) {
        if cost.stage_us_per_token < total {
            total = cost.stage_us_per_token;
            bytes = bytes.saturating_add(cost.boundary_bytes_per_token);
            admitted.push(cost.clone());
        } else {
            excluded.push((node.id.clone(), PlanRejection::MissingMeasurement));
        }
    }
    admitted.sort_by(|a, b| {
        if a.node_id == local_id {
            Ordering::Less
        } else if b.node_id == local_id {
            Ordering::Greater
        } else {
            a.node_id.cmp(&b.node_id)
        }
    });
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
            compute_us_per_token: Some(compute),
            adjacent_rtt_p95_us: rtt,
            activation_bytes_per_token: bytes,
            thermal_limited: false,
            opted_in: true,
        }
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
        assert_eq!(plan.estimated_decode_us_per_token, 45);
        assert_eq!(plan.total_boundary_bytes_per_token, 2048);
        assert!(plan.evidence.contains("not-inference-benchmark"));
    }
}
