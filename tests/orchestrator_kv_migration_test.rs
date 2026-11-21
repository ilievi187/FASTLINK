use fastlink::cluster_orchestrator::{ClusterKvStore, ClusterNode, ClusterOrchestrator};
use fastlink::digest;
use fastlink::manifest::NodeLifecycle;
use fastlink::pairing::NetworkTrust;
use fastlink::scheduler::{DeviceRole, ModelSpec, NodeProfile, PlanObjective};

const GIB: u64 = 1024 * 1024 * 1024;

fn model_16_layers() -> ModelSpec {
    ModelSpec {
        model_id: "orch-kv-mig-test".into(),
        total_layers: 16,
        layer_weights_bytes: GIB,
        kv_bytes_per_layer_per_token: 64,
        activation_bytes_per_token: 4096,
        context_len: 1024,
        min_usable_headroom_bytes: GIB,
    }
}

fn profile(id: &str, budget_gib: u64, compute_us: u64) -> NodeProfile {
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
fn evacuate_with_kv_migration_transfers_and_verifies_pages() {
    let mut orch = ClusterOrchestrator::new(model_16_layers(), digest(b"model-v1"));
    for (idx, id) in ["node-a", "node-b", "node-c"].into_iter().enumerate() {
        orch.register_node(ClusterNode {
            profile: profile(id, 10, 20 + idx as u64),
            addr: format!("127.0.0.1:1920{idx}"),
            trust: NetworkTrust::Loopback,
        })
        .unwrap();
    }

    let (_plan, manifest) = orch
        .plan_and_commit(PlanObjective::LargestModel, Some("node-a"))
        .unwrap();
    let stages = manifest.stages.clone();

    let mut kv_store = ClusterKvStore::new();
    for stage in &stages {
        let pages = vec![
            vec![stage.stage_index as u8 + 1; 256],
            vec![stage.stage_index as u8 + 2; 256],
        ];
        kv_store.put_stage_pages(
            stage.node_id.clone(),
            manifest.epoch,
            1,
            stage.layer_start,
            stage.layer_end,
            pages,
        );
    }

    let (outcome, reports, new_manifest) = orch
        .evacuate_with_kv_migration(
            "node-b",
            PlanObjective::LargestModel,
            Some("node-a"),
            &mut kv_store,
            2,
        )
        .unwrap();

    assert_eq!(outcome.new_epoch, 2);
    assert_eq!(new_manifest.epoch, 2);
    assert_eq!(orch.node_state("node-b"), Some(NodeLifecycle::Evacuated));
    assert!(!reports.is_empty());
    for report in &reports {
        assert_eq!(report.from_node, "node-b");
        assert_eq!(report.target_epoch, 2);
        assert_eq!(report.deltas_transferred, 2);
        let (snap, pages) = kv_store
            .get_stage_pages(&report.to_node, report.layer_start, report.layer_end)
            .expect("migrated pages stored on target");
        assert_eq!(snap.epoch, 2);
        assert_eq!(pages.len(), 2);
    }
}
