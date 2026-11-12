use fastlink::cluster_orchestrator::{ClusterNode, ClusterOrchestrator, OrchestratorError};
use fastlink::digest;
use fastlink::manifest::NodeLifecycle;
use fastlink::pairing::NetworkTrust;
use fastlink::scheduler::{DeviceRole, ModelSpec, NodeProfile, PlanObjective};

const GIB: u64 = 1024 * 1024 * 1024;

fn test_model() -> ModelSpec {
    ModelSpec {
        model_id: "test-70b-partition".into(),
        total_layers: 16,
        layer_weights_bytes: GIB,
        kv_bytes_per_layer_per_token: 64,
        activation_bytes_per_token: 4096,
        context_len: 1024,
        min_usable_headroom_bytes: GIB,
    }
}

fn test_profile(id: &str, budget_gib: u64, compute_us: u64) -> NodeProfile {
    NodeProfile {
        id: id.into(),
        os: "linux".into(),
        backend: "cpu".into(),
        memory_budget_bytes: budget_gib * GIB,
        workspace_bytes: 0,
        runtime_overhead_bytes: 0,
        safety_reserve_bytes: 0,
        per_layer_compute_us: Some(compute_us),
        adjacent_rtt_p95_us: Some(5),
        adjacent_bandwidth_mbps: Some(10_000),
        thermal_limited: false,
        opted_in: true,
        is_mobile: false,
        battery_pct: None,
        allowed_roles: vec![DeviceRole::DecodeWorker],
    }
}

#[test]
fn rejects_untrusted_node_registration() {
    let mut orch = ClusterOrchestrator::new(test_model(), digest(b"model-v1"));
    let err = orch
        .register_node(ClusterNode {
            profile: test_profile("rogue", 16, 10),
            addr: "127.0.0.1:9000".into(),
            trust: NetworkTrust::Untrusted,
        })
        .unwrap_err();
    assert!(matches!(err, OrchestratorError::UntrustedNode(ref id) if id == "rogue"));
}

#[test]
fn plans_commits_evacuates_and_runs_loopback_pipeline() {
    let mut orch = ClusterOrchestrator::new(test_model(), digest(b"model-v1"));
    for (idx, (id, budget)) in [("node-a", 10), ("node-b", 10), ("node-c", 10)]
        .into_iter()
        .enumerate()
    {
        orch.register_node(ClusterNode {
            profile: test_profile(id, budget, 20 + idx as u64),
            addr: format!("127.0.0.1:1900{idx}"),
            trust: NetworkTrust::Loopback,
        })
        .unwrap();
    }

    let (plan, manifest_e1) = orch
        .plan_and_commit(PlanObjective::LargestModel, Some("node-a"))
        .unwrap();
    assert!(plan.fits_model);
    assert_eq!(manifest_e1.epoch, 1);
    assert_eq!(manifest_e1.total_layers, 16);
    assert_eq!(orch.node_state("node-a"), Some(NodeLifecycle::Active));

    let tokens = vec![vec![1u8, 2, 3, 4], vec![5u8, 6, 7, 8]];
    let (outputs, summary) = orch
        .run_loopback_inference(&tokens, |_stage, data| {
            data.iter().map(|b| b.wrapping_add(1)).collect()
        })
        .unwrap();
    assert_eq!(outputs.len(), 2);
    assert_eq!(summary.total_tokens, 2);

    let (reshard, manifest_e2) = orch
        .evacuate_and_reshard("node-b", PlanObjective::LargestModel, Some("node-a"))
        .unwrap();
    assert_eq!(reshard.previous_epoch, Some(1));
    assert_eq!(reshard.new_epoch, 2);
    assert_eq!(manifest_e2.epoch, 2);
    assert_eq!(orch.node_state("node-b"), Some(NodeLifecycle::Evacuated));
    assert!(!reshard.migrations.is_empty());
    assert!(reshard
        .migrations
        .iter()
        .all(|m| m.from_node == "node-b" && m.layer_start < m.layer_end));
}

#[test]
fn fault_reshard_excludes_faulted_node_and_bumps_epoch() {
    let mut orch = ClusterOrchestrator::new(test_model(), digest(b"model-v1"));
    for (idx, id) in ["node-a", "node-b", "node-c"].into_iter().enumerate() {
        orch.register_node(ClusterNode {
            profile: test_profile(id, 10, 20),
            addr: format!("127.0.0.1:1910{idx}"),
            trust: NetworkTrust::Lan,
        })
        .unwrap();
    }

    orch.plan_and_commit(PlanObjective::LargestModel, Some("node-a"))
        .unwrap();
    let (outcome, manifest) = orch
        .fault_and_reshard("node-c", PlanObjective::LargestModel, Some("node-a"))
        .unwrap();
    assert_eq!(outcome.new_epoch, 2);
    assert!(manifest.stages.iter().all(|s| s.node_id != "node-c"));
    assert_eq!(orch.node_state("node-c"), Some(NodeLifecycle::Faulted));
}
