use fastlink::scheduler::{
    plan_heterogeneous_stages, sample_profiles_16, sample_profiles_4, sample_profiles_7,
    DeviceRole, ModelSpec, MultiStagePlan, NodeProfile, PlanObjective,
};

const GIB: u64 = 1024 * 1024 * 1024;

fn node(
    id: &str,
    memory_budget_bytes: u64,
    per_layer_compute_us: Option<u64>,
    adjacent_rtt_p95_us: Option<u64>,
    adjacent_bandwidth_mbps: Option<u64>,
    _activation_bytes_per_token: u64,
) -> NodeProfile {
    NodeProfile {
        id: id.into(),
        os: "linux".into(),
        backend: "cpu".into(),
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

fn model_70b() -> ModelSpec {
    ModelSpec {
        model_id: "synthetic-70b".into(),
        total_layers: 80,
        layer_weights_bytes: 2 * GIB,
        kv_bytes_per_layer_per_token: 100_000,
        activation_bytes_per_token: 4_000_000,
        context_len: 8192,
        min_usable_headroom_bytes: 512 * 1024 * 1024,
    }
}

fn model_small() -> ModelSpec {
    ModelSpec {
        model_id: "synthetic-small".into(),
        total_layers: 8,
        layer_weights_bytes: 1_000_000_000,
        kv_bytes_per_layer_per_token: 10_000,
        activation_bytes_per_token: 100_000,
        context_len: 1024,
        min_usable_headroom_bytes: 0,
    }
}

fn assert_full_coverage(plan: &MultiStagePlan, total_layers: u32) {
    let mut cursor = 0u32;
    for stage in &plan.stages {
        assert_eq!(
            stage.layer_start, cursor,
            "stage must start where the previous stage ended"
        );
        assert!(
            stage.layer_end > stage.layer_start,
            "stage must cover at least one layer"
        );
        assert!(
            stage.layer_end <= total_layers,
            "stage must not overrun the model"
        );
        cursor = stage.layer_end;
    }
    assert_eq!(
        cursor, total_layers,
        "stages must tile the model with no gaps or overlaps"
    );
}

#[test]
fn seventy_b_class_model_memory_fit_across_heterogeneous_clusters() {
    let clusters: Vec<Vec<NodeProfile>> = vec![
        sample_profiles_4(),
        sample_profiles_7(),
        sample_profiles_16(),
    ];
    for profiles in clusters {
        let plan =
            plan_heterogeneous_stages(&model_70b(), &profiles, PlanObjective::LowestLatency, None);
        assert!(
            plan.fits_model,
            "cluster of {} nodes should fit the 70B-class model",
            profiles.len()
        );
        assert_full_coverage(&plan, 80);
        assert!(
            plan.stages.len() > 1,
            "expected a multi-stage plan for the 70B-class model"
        );
        assert!(
            plan.stages
                .iter()
                .all(|s| (s.layer_end - s.layer_start) < 80),
            "no single stage should hold the full 70B-class model"
        );
    }
}

#[test]
fn slow_wifi_phone_is_excluded_from_decode_critical_path_via_aux_role() {
    let local = node("local", 4 * GIB, Some(60), None, None, 0);
    let fast = node("fast", 16 * GIB, Some(30), Some(2), Some(1_000), 100_000);
    let mut phone = node("phone", 8 * GIB, Some(900), Some(280), Some(60), 100_000);
    phone.is_mobile = true;
    phone.battery_pct = Some(80);
    phone.allowed_roles = vec![
        DeviceRole::DecodeWorker,
        DeviceRole::DraftWorker,
        DeviceRole::CacheStaging,
    ];

    let plan = plan_heterogeneous_stages(
        &model_small(),
        &[local, fast, phone],
        PlanObjective::LowestLatency,
        Some("local"),
    );

    assert!(plan.fits_model);
    assert_full_coverage(&plan, 8);
    assert!(
        !plan.stages.iter().any(|s| s.node_id.as_str() == "phone"),
        "slow WiFi phone must not take a DecodeWorker stage"
    );
    assert!(plan
        .aux_roles
        .iter()
        .any(|(id, role)| id.as_str() == "phone" && *role == DeviceRole::DraftWorker));
    // Local (3 layers) + fast peer (5 layers) only; the phone is not on the
    // decode critical path: 60*3 + (30*5 + 2 + transfer) = 1132us per token.
    assert_eq!(plan.critical_path_us_per_token, 1132);
}

#[test]
fn slower_peer_does_not_degrade_lowest_latency_decode_critical_path() {
    let local = node("local", 4 * GIB, Some(60), None, None, 0);
    let fast = node("fast", 16 * GIB, Some(30), Some(2), Some(1_000), 100_000);

    let baseline = plan_heterogeneous_stages(
        &model_small(),
        &[local.clone(), fast.clone()],
        PlanObjective::LowestLatency,
        Some("local"),
    );
    assert_eq!(baseline.critical_path_us_per_token, 1132);

    let mut cluster = vec![local, fast];
    cluster.push(node(
        "slow",
        16 * GIB,
        Some(200),
        Some(50),
        Some(200),
        100_000,
    ));
    cluster[2].allowed_roles = vec![DeviceRole::DecodeWorker];

    let degraded = plan_heterogeneous_stages(
        &model_small(),
        &cluster,
        PlanObjective::LowestLatency,
        Some("local"),
    );

    assert_eq!(
        degraded.critical_path_us_per_token, baseline.critical_path_us_per_token,
        "adding a slower peer must not degrade the LowestLatency decode critical path"
    );
    assert!(
        !degraded.stages.iter().any(|s| s.node_id.as_str() == "slow"),
        "slower peer must not take a DecodeWorker stage"
    );
}

#[test]
fn layer_ranges_cover_the_model_contiguously_without_gaps_or_overlaps() {
    let plan = plan_heterogeneous_stages(
        &model_70b(),
        &sample_profiles_4(),
        PlanObjective::LowestLatency,
        None,
    );
    assert!(plan.fits_model);
    assert_full_coverage(&plan, 80);
}
