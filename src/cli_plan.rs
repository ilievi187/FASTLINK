//! Dry-run `fastlink plan` subcommand: heterogeneous staging preview.
//!
//! Parses `fastlink plan [--nodes N] [--objective O] [--model M] [--json]`,
//! plans a staged decode split over the requested synthetic sample cluster and
//! renders either a human-readable summary or JSON. Estimates are synthetic and
//! derived from declared measurements; they are not measured GPU inference.

pub use crate::scheduler::PlanObjective;
use crate::scheduler::{self, ModelSpec, MultiStagePlan, NodeProfile};
use std::fmt::Write as _;
use std::io;

pub const DEFAULT_PLAN_NODES: usize = 4;
pub const DEFAULT_PLAN_OBJECTIVE: &str = "latency";
pub const DEFAULT_PLAN_MODEL: &str = "70b";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlanModel {
    Small,
    SeventyB,
}

impl PlanModel {
    pub fn as_str(self) -> &'static str {
        match self {
            PlanModel::Small => "small",
            PlanModel::SeventyB => "70b",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlanArgs {
    pub nodes: usize,
    pub objective: PlanObjective,
    pub model: PlanModel,
    pub json: bool,
}

pub fn plan_model_from_str(value: &str) -> Option<PlanModel> {
    match value {
        "small" => Some(PlanModel::Small),
        "70b" => Some(PlanModel::SeventyB),
        _ => None,
    }
}

pub fn objective_from_str(value: &str) -> Option<PlanObjective> {
    match value {
        "largest" => Some(PlanObjective::LargestModel),
        "latency" => Some(PlanObjective::LowestLatency),
        "throughput" => Some(PlanObjective::MaxThroughput),
        _ => None,
    }
}

pub fn parse_plan_args(args: &[String]) -> Result<PlanArgs, String> {
    let mut nodes: usize = DEFAULT_PLAN_NODES;
    let mut objective: Option<PlanObjective> = None;
    let mut model: Option<PlanModel> = None;
    let mut json = false;
    let mut index: usize = 0;
    while index < args.len() {
        let flag = args[index].as_str();
        if flag == "--json" {
            json = true;
            index += 1;
            continue;
        }
        if flag != "--nodes" && flag != "--objective" && flag != "--model" {
            return Err(format!("unknown plan flag '{flag}'"));
        }
        let value = args
            .get(index + 1)
            .ok_or_else(|| format!("missing value for plan flag '{flag}'"))?
            .as_str();
        if flag == "--nodes" {
            let parsed = value
                .parse::<usize>()
                .map_err(|_| format!("invalid --nodes value '{value}'"))?;
            if !matches!(parsed, 1 | 2 | 3 | 4 | 7 | 16) {
                return Err(format!(
                    "invalid --nodes value '{value}' (expected 1, 2, 3, 4, 7 or 16)"
                ));
            }
            nodes = parsed;
        } else if flag == "--objective" {
            let parsed = objective_from_str(value)
                .ok_or_else(|| format!("invalid --objective value '{value}'"))?;
            objective = Some(parsed);
        } else {
            let parsed = plan_model_from_str(value)
                .ok_or_else(|| format!("invalid --model value '{value}'"))?;
            model = Some(parsed);
        }
        index += 2;
    }
    Ok(PlanArgs {
        nodes,
        objective: objective.unwrap_or(PlanObjective::LowestLatency),
        model: model.unwrap_or(PlanModel::SeventyB),
        json,
    })
}

pub fn sample_profiles_for_count(count: usize) -> Vec<NodeProfile> {
    match count {
        1 => scheduler::sample_profiles_1(),
        2 => scheduler::sample_profiles_2(),
        3 => scheduler::sample_profiles_3(),
        4 => scheduler::sample_profiles_4(),
        7 => scheduler::sample_profiles_7(),
        16 => scheduler::sample_profiles_16(),
        _ => Vec::new(),
    }
}

pub fn model_for(model: PlanModel) -> ModelSpec {
    match model {
        PlanModel::Small => ModelSpec {
            model_id: "synthetic-small".to_string(),
            total_layers: 8,
            layer_weights_bytes: 1_000_000_000,
            kv_bytes_per_layer_per_token: 10_000,
            activation_bytes_per_token: 100_000,
            context_len: 1024,
            min_usable_headroom_bytes: 0,
        },
        PlanModel::SeventyB => ModelSpec {
            model_id: "synthetic-70b".to_string(),
            total_layers: 80,
            layer_weights_bytes: 2 * 1024 * 1024 * 1024,
            kv_bytes_per_layer_per_token: 100_000,
            activation_bytes_per_token: 4_000_000,
            context_len: 8192,
            min_usable_headroom_bytes: 512 * 1024 * 1024,
        },
    }
}

pub fn plan_for_args(args: &PlanArgs) -> MultiStagePlan {
    let profiles = sample_profiles_for_count(args.nodes);
    scheduler::plan_heterogeneous_stages(&model_for(args.model), &profiles, args.objective, None)
}

pub fn render_plan_json(plan: &MultiStagePlan) -> String {
    let mut out = String::from("{");
    out.push_str(&format!("\"objective\":\"{}\",", plan.objective.as_str()));
    out.push_str(&format!("\"fits_model\":{},", plan.fits_model));
    out.push_str(&format!(
        "\"critical_path_us_per_token\":{},",
        plan.critical_path_us_per_token
    ));
    out.push_str(&format!(
        "\"total_boundary_bytes\":{},",
        plan.total_boundary_bytes
    ));
    out.push_str("\"stages\":[");
    for (index, stage) in plan.stages.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push('{');
        out.push_str(&format!("\"node_id\":\"{}\",", json_escape(&stage.node_id)));
        out.push_str(&format!("\"role\":\"{}\",", stage.role.as_str()));
        out.push_str(&format!("\"layer_start\":{},", stage.layer_start));
        out.push_str(&format!("\"layer_end\":{},", stage.layer_end));
        out.push_str(&format!("\"weights_bytes\":{},", stage.weights_bytes));
        out.push_str(&format!("\"kv_bytes\":{},", stage.kv_bytes));
        out.push_str(&format!(
            "\"estimated_stage_us\":{},",
            stage.estimated_stage_us
        ));
        out.push_str(&format!("\"boundary_bytes\":{}", stage.boundary_bytes));
        out.push('}');
    }
    out.push_str("],\"aux_roles\":[");
    for (index, (node_id, role)) in plan.aux_roles.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push('[');
        out.push_str(&format!("\"{}\",", json_escape(node_id)));
        out.push_str(&format!("\"{}\"", role.as_str()));
        out.push(']');
    }
    out.push_str("],\"excluded\":[");
    for (index, (node_id, reason)) in plan.excluded.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push('[');
        out.push_str(&format!("\"{}\",", json_escape(node_id)));
        out.push_str(&format!("\"{}\"", json_escape(reason)));
        out.push(']');
    }
    out.push_str("]}");
    out
}

pub fn render_plan_text(plan: &MultiStagePlan) -> String {
    let mut out = String::from("FASTLINK plan (dry-run)\n");
    out.push_str("objective: ");
    out.push_str(plan.objective.as_str());
    out.push('\n');
    out.push_str("fits_model: ");
    out.push_str(if plan.fits_model { "true" } else { "false" });
    out.push_str("\ncritical_path_us_per_token: ");
    out.push_str(&plan.critical_path_us_per_token.to_string());
    out.push_str("\ntotal_boundary_bytes: ");
    out.push_str(&plan.total_boundary_bytes.to_string());
    out.push_str("\n\nstages:");
    for stage in &plan.stages {
        out.push_str(&format!(
            "\n  - node: {} role: {} layers: [{}, {}) weights_bytes: {} kv_bytes: {} estimated_stage_us: {} boundary_bytes: {}",
            stage.node_id,
            stage.role.as_str(),
            stage.layer_start,
            stage.layer_end,
            stage.weights_bytes,
            stage.kv_bytes,
            stage.estimated_stage_us,
            stage.boundary_bytes
        ));
    }
    if plan.stages.is_empty() {
        out.push_str("\n  (none)");
    }
    out.push_str("\n\naux_roles:");
    for (node_id, role) in &plan.aux_roles {
        out.push_str(&format!(
            "\n  - node: {} aux_role: {}",
            node_id,
            role.as_str()
        ));
    }
    out.push_str("\n\nexcluded:");
    for (node_id, reason) in &plan.excluded {
        out.push_str(&format!("\n  - node: {} reason: {}", node_id, reason));
    }
    out.push_str("\n\nnote: dry-run synthetic estimates from declared measurements; not measured GPU inference");
    out
}

fn json_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

pub fn run_plan(args: &[String]) -> io::Result<String> {
    let parsed = parse_plan_args(args)
        .map_err(|message| io::Error::new(io::ErrorKind::InvalidData, message))?;
    let plan = plan_for_args(&parsed);
    if parsed.json {
        Ok(render_plan_json(&plan))
    } else {
        Ok(render_plan_text(&plan))
    }
}
