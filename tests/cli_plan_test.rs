use fastlink::cli_plan::{
    parse_plan_args, plan_for_args, render_plan_json, render_plan_text, sample_profiles_for_count,
    PlanArgs, PlanModel, PlanObjective,
};

fn tokens(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_string()).collect()
}

#[test]
fn plan_args_defaults_to_safe_cluster() {
    let parsed = parse_plan_args(&tokens(&[])).expect("default plan args must parse");
    assert_eq!(parsed.nodes, 4);
    assert_eq!(parsed.objective, PlanObjective::LowestLatency);
    assert_eq!(parsed.model, PlanModel::SeventyB);
    assert!(!parsed.json);
}

#[test]
fn plan_args_parse_explicit_values() {
    let parsed = parse_plan_args(&tokens(&[
        "--nodes",
        "7",
        "--objective",
        "throughput",
        "--model",
        "small",
        "--json",
    ]))
    .expect("explicit plan args must parse");
    assert_eq!(parsed.nodes, 7);
    assert_eq!(parsed.objective, PlanObjective::MaxThroughput);
    assert_eq!(parsed.model, PlanModel::Small);
    assert!(parsed.json);
}

#[test]
fn plan_args_reject_unknown_or_invalid_flags() {
    assert!(parse_plan_args(&tokens(&["--nodes", "5"])).is_err());
    assert!(parse_plan_args(&tokens(&["--objective", "bogus"])).is_err());
    assert!(parse_plan_args(&tokens(&["--model", "13b"])).is_err());
    assert!(parse_plan_args(&tokens(&["--nope"])).is_err());
    assert!(parse_plan_args(&tokens(&["--nodes"])).is_err());
}

#[test]
fn plan_profiles_exist_for_supported_cluster_sizes() {
    for count in [1usize, 2, 3, 4, 7, 16] {
        assert_eq!(sample_profiles_for_count(count).len(), count);
    }
}

#[test]
fn plan_renders_json_and_text_from_scheduler_plan() {
    let plan = plan_for_args(&PlanArgs {
        nodes: 2,
        objective: PlanObjective::LowestLatency,
        model: PlanModel::Small,
        json: false,
    });
    let json = render_plan_json(&plan);
    assert!(json.contains("\"objective\":\"LowestLatency\""));
    assert!(json.contains("\"fits_model\""));
    assert!(json.contains("\"stages\""));
    let text = render_plan_text(&plan);
    assert!(text.contains("dry-run"));
    assert!(text.contains("not measured"));
}
