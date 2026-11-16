use fastlink::scheduler::{
    plan_heterogeneous_stages, plan_moe_experts, route_moe_topk, sample_profiles_16,
    sample_profiles_4, sample_profiles_7, DeviceRole, ExpertRoute, ModelSpec, MoeModelSpec,
    MoePlacementPlan, MoePlanObjective, MoeRouteError, MultiStagePlan, NodeProfile, PlanObjective,
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

fn moe_model(total_experts: u32, active_top_k: u32, expert_weights_bytes: u64) -> MoeModelSpec {
    MoeModelSpec {
        model_id: "synthetic-cpu-moe".into(),
        total_experts,
        active_top_k,
        expert_weights_bytes,
        shared_weights_bytes: 256 * 1024 * 1024,
        min_usable_headroom_bytes: 128 * 1024 * 1024,
    }
}

fn assert_exact_once_contiguous_moe_coverage(plan: &MoePlacementPlan, expected_experts: u32) {
    assert!(plan.fits_model, "expected feasible MoE plan: {plan:?}");
    assert_eq!(plan.infeasible_reason, None);
    assert_eq!(plan.assigned_experts, expected_experts);
    assert_eq!(plan.expert_to_node.len(), expected_experts as usize);

    let mut seen: Vec<u32> = Vec::with_capacity(expected_experts as usize);
    for assignment in &plan.assignments {
        assert!(
            !assignment.expert_ids.is_empty(),
            "assignments must not contain empty node entries: {}",
            assignment.node_id
        );
        for pair in assignment.expert_ids.windows(2) {
            assert_eq!(
                pair[1],
                pair[0] + 1,
                "node {} expert IDs must be strictly ascending and contiguous: {:?}",
                assignment.node_id,
                assignment.expert_ids
            );
        }
        for &expert_id in &assignment.expert_ids {
            assert_eq!(
                plan.expert_to_node
                    .get(expert_id as usize)
                    .map(String::as_str),
                Some(assignment.node_id.as_str()),
                "expert_to_node mismatch for expert {expert_id}"
            );
            seen.push(expert_id);
        }
    }
    seen.sort_unstable();
    let expected: Vec<u32> = (0..expected_experts).collect();
    assert_eq!(
        seen, expected,
        "experts must be assigned exactly once covering 0..{expected_experts} contiguously"
    );
}

#[test]
fn moe_exact_once_assignment_and_contiguous_id_coverage_respects_objectives() {
    let model = moe_model(7, 2, 512 * 1024 * 1024);
    // Node overhead in `node()` is 256+128+128 = 512 MiB; model adds 256 shared + 128 headroom = 384 MiB.
    // Total fixed overhead per node = 896 MiB.
    // Budget of 3 GiB (3072 MiB) -> 2176 MiB free for experts -> 4 experts max (4 * 512 = 2048 MiB).
    let nodes = vec![
        node("cpu-a", 3 * GIB, Some(40), None, None, 0),
        node("cpu-b", 3 * GIB, Some(45), Some(100), Some(1_000), 0),
        node("cpu-c", 3 * GIB, Some(50), Some(120), Some(1_000), 0),
    ];

    let balanced = plan_moe_experts(
        &model,
        &nodes,
        MoePlanObjective::BalancedUniformPopularity,
        Some("cpu-a"),
    );
    assert_exact_once_contiguous_moe_coverage(&balanced, 7);
    assert!(
        balanced
            .assumption_note
            .contains("uniform expert popularity"),
        "BalancedUniformPopularity must document uniform popularity assumption: {}",
        balanced.assumption_note
    );
    assert!(
        balanced
            .assumption_note
            .contains("not an MoE inference runner"),
        "plan must distinguish planner from inference runner: {}",
        balanced.assumption_note
    );
    assert_eq!(
        balanced
            .assignments
            .iter()
            .map(|a| a.expert_ids.len())
            .collect::<Vec<_>>(),
        vec![3, 2, 2],
        "uniform balance should distribute 7 experts as 3/2/2"
    );

    let local_first = plan_moe_experts(
        &model,
        &nodes,
        MoePlanObjective::MinimizeCrossNodeMovement,
        Some("cpu-a"),
    );
    assert_exact_once_contiguous_moe_coverage(&local_first, 7);
    assert_eq!(
        local_first
            .assignments
            .iter()
            .map(|a| (a.node_id.as_str(), a.expert_ids.len()))
            .collect::<Vec<_>>(),
        vec![("cpu-a", 4), ("cpu-b", 3)],
        "MinimizeCrossNodeMovement should fill local node up to capacity (4) then pack fewest peers"
    );
    assert!(local_first
        .excluded
        .iter()
        .any(|(id, reason)| id == "cpu-c" && reason == "CandidateNotSelected"));

    // Asymmetric capacity capping under uniform balance: cpu-small fits 2, cpu-large fits 6.
    // Total 6 experts must cap cpu-small at 2 and spill the remaining 4 onto cpu-large.
    let small_budget = 896 * 1024 * 1024 + 2 * 512 * 1024 * 1024;
    let large_budget = 896 * 1024 * 1024 + 6 * 512 * 1024 * 1024;
    let asym_model = moe_model(6, 2, 512 * 1024 * 1024);
    let asym = plan_moe_experts(
        &asym_model,
        &[
            node("cpu-small", small_budget, Some(40), None, None, 0),
            node(
                "cpu-large",
                large_budget,
                Some(50),
                Some(80),
                Some(1_000),
                0,
            ),
        ],
        MoePlanObjective::BalancedUniformPopularity,
        Some("cpu-small"),
    );
    assert_exact_once_contiguous_moe_coverage(&asym, 6);
    assert_eq!(
        asym.assignments
            .iter()
            .map(|a| (
                a.node_id.as_str(),
                a.expert_ids.len(),
                a.max_fittable_experts
            ))
            .collect::<Vec<_>>(),
        vec![("cpu-small", 2, 2), ("cpu-large", 4, 6)]
    );
    assert_eq!(asym.objective.as_str(), "BalancedUniformPopularity");
}

#[test]
fn moe_capacity_failure_and_memory_boundaries_are_reported_infeasibly() {
    let expert_size = 512 * 1024 * 1024;
    let model = moe_model(4, 2, expert_size);
    // Fixed overhead: 512 MiB node + 384 MiB model = 896 MiB.
    // Exact budget for 2 experts = 896 MiB + 1024 MiB = 1920 MiB.
    let exact_two = 1920 * 1024 * 1024;
    let nodes = vec![
        node("n1", exact_two, Some(30), None, None, 0),
        node("n2", exact_two, Some(30), Some(50), Some(1_000), 0),
    ];
    let feasible = plan_moe_experts(
        &model,
        &nodes,
        MoePlanObjective::BalancedUniformPopularity,
        Some("n1"),
    );
    assert_exact_once_contiguous_moe_coverage(&feasible, 4);

    // Dropping n2 by 1 byte reduces its capacity from 2 experts to 1 -> total capacity 3 < 4.
    let short_by_one_byte = vec![
        node("n1", exact_two, Some(30), None, None, 0),
        node("n2", exact_two - 1, Some(30), Some(50), Some(1_000), 0),
    ];
    let infeasible = plan_moe_experts(
        &model,
        &short_by_one_byte,
        MoePlanObjective::BalancedUniformPopularity,
        Some("n1"),
    );
    assert!(!infeasible.fits_model);
    assert_eq!(infeasible.total_capacity_experts, 3);
    assert_eq!(infeasible.assigned_experts, 0);
    assert!(infeasible.assignments.is_empty());
    assert!(infeasible.expert_to_node.is_empty());
    assert!(infeasible.infeasible_reason.is_some());

    // Zero eligible nodes / saturating arithmetic guard.
    let mut overflowing = node("overflow", u64::MAX / 2, Some(10), None, None, 0);
    overflowing.safety_reserve_bytes = u64::MAX;
    let overflow_plan = plan_moe_experts(
        &model,
        &[overflowing],
        MoePlanObjective::MinimizeCrossNodeMovement,
        None,
    );
    assert!(!overflow_plan.fits_model);
    assert!(overflow_plan
        .excluded
        .iter()
        .any(|(id, reason)| id == "overflow" && reason == "NoMemoryFit"));
}

#[test]
fn moe_excludes_opted_out_thermal_low_battery_and_disallowed_role_nodes() {
    let model = moe_model(4, 2, 256 * 1024 * 1024);

    let opted_in_ok = node("ok-battery-16", 4 * GIB, Some(40), None, None, 0);
    let mut bat16 = opted_in_ok.clone();
    bat16.is_mobile = true;
    bat16.battery_pct = Some(16);

    let mut opted_out = node("opted-out", 4 * GIB, Some(20), Some(5), Some(1_000), 0);
    opted_out.opted_in = false;

    let mut thermal = node("hot-node", 4 * GIB, Some(20), Some(5), Some(1_000), 0);
    thermal.thermal_limited = true;

    let mut low_battery = node("low-bat-15", 4 * GIB, Some(20), Some(5), Some(1_000), 0);
    low_battery.is_mobile = true;
    low_battery.battery_pct = Some(15);

    let mut bad_role = node("cache-only", 4 * GIB, Some(20), Some(5), Some(1_000), 0);
    bad_role.allowed_roles = vec![
        DeviceRole::CacheStaging,
        DeviceRole::DraftWorker,
        DeviceRole::Guest,
    ];

    let no_fit = node("tiny", 512 * 1024 * 1024, Some(20), Some(5), Some(1_000), 0);

    let plan = plan_moe_experts(
        &model,
        &[opted_out, thermal, low_battery, bad_role, no_fit, bat16],
        MoePlanObjective::BalancedUniformPopularity,
        None,
    );

    assert_exact_once_contiguous_moe_coverage(&plan, 4);
    assert_eq!(plan.assignments.len(), 1);
    assert_eq!(plan.assignments[0].node_id, "ok-battery-16");
    for expected in [
        ("opted-out", "NotOptedIn"),
        ("hot-node", "ThermalLimit"),
        ("low-bat-15", "LowBattery"),
        ("cache-only", "RoleNotAllowed"),
        ("tiny", "NoMemoryFit"),
    ] {
        assert!(
            plan.excluded
                .iter()
                .any(|(id, reason)| id == expected.0 && reason == expected.1),
            "expected exclusion {:?}, got {:?}",
            expected,
            plan.excluded
        );
    }
}

#[test]
fn moe_tie_breaking_is_deterministic_across_input_permutations() {
    let model = moe_model(6, 2, 256 * 1024 * 1024);
    let a = node("alpha", 4 * GIB, Some(50), Some(10), Some(1_000), 0);
    let b = node("bravo", 4 * GIB, Some(50), Some(10), Some(1_000), 0);
    let c = node("charlie", 4 * GIB, Some(50), Some(10), Some(1_000), 0);
    let mut off = node("zulu-off", 4 * GIB, Some(10), Some(1), Some(10_000), 0);
    off.opted_in = false;

    let forward = [a.clone(), b.clone(), c.clone(), off.clone()];
    let shuffled = [c, off, b, a];

    for objective in [
        MoePlanObjective::BalancedUniformPopularity,
        MoePlanObjective::MinimizeCrossNodeMovement,
    ] {
        let p1 = plan_moe_experts(&model, &forward, objective, None);
        let p2 = plan_moe_experts(&model, &shuffled, objective, None);
        assert_eq!(
            p1, p2,
            "plan must be identical regardless of input slice permutation for {objective:?}"
        );
        assert_eq!(p1.assignments[0].node_id, "alpha");
    }
}

#[test]
fn moe_topk_route_mapping_and_invalid_input_rejection() {
    let model = moe_model(8, 2, 256 * 1024 * 1024);
    let nodes = vec![
        node("cpu-local", 4 * GIB, Some(40), None, None, 0),
        node("cpu-peer", 4 * GIB, Some(45), Some(120), Some(2_500), 0),
    ];
    let plan = plan_moe_experts(
        &model,
        &nodes,
        MoePlanObjective::BalancedUniformPopularity,
        Some("cpu-local"),
    );
    assert_exact_once_contiguous_moe_coverage(&plan, 8);
    // 0..4 -> cpu-local, 4..8 -> cpu-peer.
    let routes = route_moe_topk(&plan, &[6, 1]).expect("valid top-2 selection");
    assert_eq!(
        routes,
        vec![
            ExpertRoute {
                expert_id: 6,
                node_id: "cpu-peer".into(),
                is_local: false,
                reported_rtt_p95_us: Some(120),
                reported_bandwidth_mbps: Some(2_500),
            },
            ExpertRoute {
                expert_id: 1,
                node_id: "cpu-local".into(),
                is_local: true,
                reported_rtt_p95_us: None,
                reported_bandwidth_mbps: None,
            },
        ]
    );

    assert_eq!(
        route_moe_topk(&plan, &[]),
        Err(MoeRouteError::EmptySelection)
    );
    assert_eq!(
        route_moe_topk(&plan, &[0, 1, 2]),
        Err(MoeRouteError::TopKOverflow {
            selected: 3,
            max_top_k: 2,
        })
    );
    assert_eq!(
        route_moe_topk(&plan, &[1, 8]),
        Err(MoeRouteError::InvalidExpertId {
            expert_id: 8,
            total_experts: 8,
        })
    );
    assert_eq!(
        route_moe_topk(&plan, &[3, 3]),
        Err(MoeRouteError::DuplicateExpertSelection { expert_id: 3 })
    );

    // Routing against infeasible plan must fail cleanly.
    let infeasible_model = moe_model(100, 2, 1024 * 1024 * 1024);
    let bad_plan = plan_moe_experts(
        &infeasible_model,
        &nodes,
        MoePlanObjective::BalancedUniformPopularity,
        Some("cpu-local"),
    );
    assert!(!bad_plan.fits_model);
    assert_eq!(
        route_moe_topk(&bad_plan, &[0, 1]),
        Err(MoeRouteError::InfeasiblePlan)
    );

    // Partial selection (`1 <= selected.len() <= active_top_k`) is permitted when valid.
    let single_route = route_moe_topk(&plan, &[2]).expect("single valid selection <= top_k");
    assert_eq!(single_route.len(), 1);
    assert_eq!(single_route[0].expert_id, 2);
}

#[test]
fn moe_rejects_invalid_model_spec_and_duplicate_or_empty_node_ids() {
    let nodes = vec![
        node("node-a", 4 * GIB, Some(30), None, None, 0),
        node("node-b", 4 * GIB, Some(30), Some(10), Some(1_000), 0),
    ];
    for invalid_model in [
        moe_model(0, 0, 256 * 1024 * 1024),
        moe_model(4, 0, 256 * 1024 * 1024),
        moe_model(4, 5, 256 * 1024 * 1024),
    ] {
        let plan = plan_moe_experts(
            &invalid_model,
            &nodes,
            MoePlanObjective::BalancedUniformPopularity,
            Some("node-a"),
        );
        assert!(
            !plan.fits_model,
            "invalid spec must be infeasible: {invalid_model:?}"
        );
        assert!(plan.infeasible_reason.is_some());
        assert_eq!(
            route_moe_topk(&plan, &[0]),
            Err(MoeRouteError::InfeasiblePlan)
        );
    }

    let valid_model = moe_model(4, 2, 256 * 1024 * 1024);
    let dup_nodes = vec![
        node("dup", 4 * GIB, Some(30), None, None, 0),
        node("dup", 4 * GIB, Some(30), None, None, 0),
    ];
    let dup_plan = plan_moe_experts(
        &valid_model,
        &dup_nodes,
        MoePlanObjective::BalancedUniformPopularity,
        None,
    );
    assert!(!dup_plan.fits_model);
    assert!(dup_plan.infeasible_reason.is_some());
    assert!(dup_plan
        .excluded
        .iter()
        .any(|(id, reason)| id == "dup" && reason == "DuplicateNodeId"));

    let empty_id_node = node("", 4 * GIB, Some(30), None, None, 0);
    let empty_id_plan = plan_moe_experts(
        &valid_model,
        &[empty_id_node],
        MoePlanObjective::MinimizeCrossNodeMovement,
        None,
    );
    assert!(!empty_id_plan.fits_model);
    assert!(empty_id_plan
        .excluded
        .iter()
        .any(|(id, reason)| id.is_empty() && reason == "InvalidNodeId"));
}

#[test]
fn moe_unknown_compute_and_network_evidence_remains_none() {
    let model = moe_model(4, 2, 256 * 1024 * 1024);
    let unmeasured_a = node("unmeasured-a", 4 * GIB, None, None, None, 0);
    let unmeasured_b = node("unmeasured-b", 4 * GIB, None, None, None, 0);

    let plan = plan_moe_experts(
        &model,
        &[unmeasured_b.clone(), unmeasured_a.clone()],
        MoePlanObjective::BalancedUniformPopularity,
        None,
    );
    assert_exact_once_contiguous_moe_coverage(&plan, 4);
    assert_eq!(plan.known_compute_nodes, 0);
    assert_eq!(plan.unknown_compute_nodes, 2);
    assert_eq!(plan.known_network_nodes, 0);
    assert_eq!(plan.unknown_network_nodes, 2);
    for assignment in &plan.assignments {
        assert_eq!(assignment.reported_per_layer_compute_us, None);
        assert_eq!(assignment.reported_adjacent_rtt_p95_us, None);
        assert_eq!(assignment.reported_adjacent_bandwidth_mbps, None);
    }

    let routes = route_moe_topk(&plan, &[0, 3]).expect("route with unknown perf evidence");
    for route in &routes {
        assert_eq!(route.reported_rtt_p95_us, None);
        assert_eq!(route.reported_bandwidth_mbps, None);
    }

    // Adding a measured peer must sort the measured peer ahead of an unmeasured peer
    // rather than treating `None` as a fast `0 us` node.
    let measured = node("z-measured", 4 * GIB, Some(80), Some(20), Some(1_000), 0);
    let mixed = plan_moe_experts(
        &model,
        &[unmeasured_a, measured],
        MoePlanObjective::MinimizeCrossNodeMovement,
        None,
    );
    assert_exact_once_contiguous_moe_coverage(&mixed, 4);
    assert_eq!(mixed.known_compute_nodes, 1);
    assert_eq!(mixed.unknown_compute_nodes, 1);
    assert_eq!(mixed.assignments.len(), 1);
    assert_eq!(mixed.assignments[0].node_id, "z-measured");
    assert_eq!(mixed.assignments[0].reported_per_layer_compute_us, Some(80));
    assert_eq!(mixed.assignments[0].reported_adjacent_rtt_p95_us, Some(20));
    assert_eq!(
        mixed.assignments[0].reported_adjacent_bandwidth_mbps,
        Some(1_000)
    );
}
