//! Cost-based placement of local compute against a remote transfer.
//!
//! The arithmetic is a Rust port of the declared WP-C model in
//! `qa/sim_placement.py`, `qa/sim_common.py`, `qa/cost_model.py`, and
//! `qa/network_sim.py` as read from `feat/fastlink-v3-network-sim` at
//! `e9c96ad`. Every latency here is model arithmetic over those declared
//! inputs. Nothing in this module reads a GPU, a socket, or a clock.
//!
//! Loss, retry, and disconnect are outside this port. `sim_placement.py`
//! calls `network_sim.transfer` with the default retry budget of 0 and the
//! declared link's loss probability of 0, which is the closed form below.
//!
//! When both sites are feasible and the modelled latencies are equal, the
//! scheduler keeps the work local.

use std::cmp::Ordering;

/// Declared device. Fields are planning inputs, not probed hardware.
#[derive(Debug, Clone, Copy)]
pub struct DeviceSpec {
    pub name: &'static str,
    pub total_vram_gib: f64,
    pub occupied_gib: f64,
    pub bandwidth_gib_s: f64,
    pub workspace_reserve_gib: f64,
}

/// Declared hop. `throughput_mib_s` is the already-rounded planning value.
#[derive(Debug, Clone, Copy)]
pub struct LinkSpec {
    pub rtt_ms: f64,
    pub throughput_mib_s: f64,
    pub serialization_ms: f64,
    pub queue_ms: f64,
}

/// Declared dense workload. See `sim_common.WORKLOAD`.
#[derive(Debug, Clone, Copy)]
pub struct WorkloadSpec {
    pub name: &'static str,
    pub params_b: f64,
    pub bits_per_weight: f64,
    pub layers: u32,
    pub hidden_dim: u32,
    pub activation_bytes: u32,
    pub ctx_tokens: u64,
    pub prompt_tokens: u64,
    pub ubatch_tokens: u64,
}

/// KV storage scale relative to the declared fp16 baseline of 4096 bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KvMode {
    Fp16,
    Q8,
    Q4,
}

impl KvMode {
    fn scale(self) -> f64 {
        match self {
            Self::Fp16 => 1.0,
            Self::Q8 => 0.5,
            Self::Q4 => 0.25,
        }
    }

    fn bytes_per_token_per_layer(self) -> u64 {
        match self {
            Self::Fp16 => 4096,
            Self::Q8 => 2048,
            Self::Q4 => 1024,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScheduleError {
    InvalidInput,
    InvalidSplit,
    InvalidTransfer,
    NoFeasiblePlacement,
    NoFeasibleSite,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComputeSite {
    Local,
    Remote,
}

/// One contiguous layer split. Latencies are modelled milliseconds.
#[derive(Debug, Clone)]
pub struct PlacementRow {
    pub k_local: u32,
    pub layers_remote: u32,
    pub weights_local_gib: f64,
    pub weights_remote_gib: f64,
    pub kv_local_gib: f64,
    pub kv_remote_gib: f64,
    pub feasible: bool,
    pub deficit_gib: f64,
    pub cold_load_remote_s: f64,
    pub prefill_ms: f64,
    pub prefill_tokens_per_s: f64,
    pub decode_step_ms: f64,
    pub decode_tokens_per_s: f64,
    pub boundary_overhead_ms_per_token: f64,
}

/// A single local-versus-remote decision, separate from the layer matrix.
#[derive(Debug, Clone, Copy)]
pub struct WorkItem {
    pub local_compute_gib: f64,
    pub remote_compute_gib: f64,
    pub transfer_mib: f64,
    pub round_trips: u32,
    pub local_model_gib: f64,
    pub local_kv_gib: f64,
    pub remote_model_gib: f64,
    pub remote_kv_gib: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SiteDecision {
    pub site: ComputeSite,
    pub local_ms: f64,
    pub remote_ms: f64,
    pub local_feasible: bool,
    pub remote_feasible: bool,
}

pub const LOCAL_MIXED: DeviceSpec = DeviceSpec {
    name: "LOCAL_MIXED: RX 7900 XT 20 GiB (existing 27B service resident)",
    total_vram_gib: 20.0,
    occupied_gib: 6.0,
    bandwidth_gib_s: 800.0,
    workspace_reserve_gib: 0.5,
};

pub const LOCAL_DEDICATED: DeviceSpec = DeviceSpec {
    name: "LOCAL_DEDICATED: RX 7900 XT 20 GiB (OS reserve only)",
    total_vram_gib: 20.0,
    occupied_gib: 0.5,
    bandwidth_gib_s: 800.0,
    workspace_reserve_gib: 0.5,
};

pub const REMOTE_DEVICE: DeviceSpec = DeviceSpec {
    name: "REMOTE: RX 6600 XT 8 GiB",
    total_vram_gib: 8.0,
    occupied_gib: 1.0,
    bandwidth_gib_s: 256.0,
    workspace_reserve_gib: 1.0,
};

/// 1 Gbps class hop at the declared 0.8 efficiency, already rounded to 4 d.p.
pub const LINK_REMOTE: LinkSpec = LinkSpec {
    rtt_ms: 1.0,
    throughput_mib_s: 95.3674,
    serialization_ms: 0.05,
    queue_ms: 0.2,
};

/// 2.5 Gbps class hop at the declared 0.8 efficiency, already rounded to 4 d.p.
pub const LINK_LOCAL: LinkSpec = LinkSpec {
    rtt_ms: 0.5,
    throughput_mib_s: 238.4186,
    serialization_ms: 0.05,
    queue_ms: 0.1,
};

pub const WORKLOAD_27B: WorkloadSpec = WorkloadSpec {
    name: "27B dense Q4_K_M-like, 131072-token ctx",
    params_b: 27.0,
    bits_per_weight: 4.7,
    layers: 64,
    hidden_dim: 4096,
    activation_bytes: 2,
    ctx_tokens: 131_072,
    prompt_tokens: 4096,
    ubatch_tokens: 512,
};

const KV_BYTES_FP16: f64 = 4096.0;

fn finite_nonneg(value: f64) -> Result<(), ScheduleError> {
    if value.is_finite() && value >= 0.0 {
        Ok(())
    } else {
        Err(ScheduleError::InvalidInput)
    }
}

impl DeviceSpec {
    pub fn budget_gib(self) -> Result<f64, ScheduleError> {
        finite_nonneg(self.total_vram_gib)?;
        finite_nonneg(self.occupied_gib)?;
        finite_nonneg(self.workspace_reserve_gib)?;
        if !self.bandwidth_gib_s.is_finite() || self.bandwidth_gib_s <= 0.0 {
            return Err(ScheduleError::InvalidInput);
        }
        let budget = self.total_vram_gib - self.occupied_gib - self.workspace_reserve_gib;
        if !budget.is_finite() || budget < 0.0 {
            return Err(ScheduleError::InvalidInput);
        }
        Ok(budget)
    }

    pub fn compute_ms_per_gib(self) -> Result<f64, ScheduleError> {
        let _ = self.budget_gib()?;
        Ok(1000.0 / self.bandwidth_gib_s)
    }
}

impl LinkSpec {
    fn check(self) -> Result<(), ScheduleError> {
        finite_nonneg(self.rtt_ms)?;
        finite_nonneg(self.serialization_ms)?;
        finite_nonneg(self.queue_ms)?;
        if !self.throughput_mib_s.is_finite() || self.throughput_mib_s <= 0.0 {
            return Err(ScheduleError::InvalidInput);
        }
        Ok(())
    }
}

impl WorkloadSpec {
    fn check(self) -> Result<(), ScheduleError> {
        finite_nonneg(self.params_b)?;
        finite_nonneg(self.bits_per_weight)?;
        if self.layers == 0 || self.hidden_dim == 0 || self.activation_bytes == 0 {
            return Err(ScheduleError::InvalidInput);
        }
        if self.ubatch_tokens == 0 {
            return Err(ScheduleError::InvalidInput);
        }
        Ok(())
    }

    pub fn weights_gib(self) -> Result<f64, ScheduleError> {
        self.check()?;
        Ok(self.params_b * 1e9 * self.bits_per_weight / 8.0 / 2f64.powi(30))
    }

    pub fn layer_weights_gib(self) -> Result<f64, ScheduleError> {
        Ok(self.weights_gib()? / f64::from(self.layers))
    }

    pub fn hidden_mib(self, tokens: u64) -> Result<f64, ScheduleError> {
        self.check()?;
        Ok(
            tokens as f64 * f64::from(self.hidden_dim) * f64::from(self.activation_bytes)
                / 2f64.powi(20),
        )
    }

    pub fn kv_gib_total(self, ctx_tokens: u64, kv: KvMode) -> Result<f64, ScheduleError> {
        self.check()?;
        Ok((ctx_tokens as f64) / 2f64.powi(30)
            * (KV_BYTES_FP16 * kv.scale() * f64::from(self.layers)))
    }
}

/// Same predicate as `cost_model.placement_is_feasible`.
///
/// The reference calls it with GiB values divided by 2^20 on every argument.
/// The comparison is scale-invariant when the caller uses one unit throughout.
pub fn placement_is_feasible(
    model_mib: f64,
    kv_mib: f64,
    reserve_mib: f64,
    free_vram_mib: f64,
) -> Result<bool, ScheduleError> {
    finite_nonneg(model_mib)?;
    finite_nonneg(kv_mib)?;
    finite_nonneg(reserve_mib)?;
    finite_nonneg(free_vram_mib)?;
    Ok(model_mib + kv_mib + reserve_mib <= free_vram_mib)
}

/// Modelled serial transfer latency. Queue delay is charged once.
pub fn transfer_latency_ms(
    link: LinkSpec,
    mib: f64,
    round_trips: u32,
) -> Result<f64, ScheduleError> {
    link.check()?;
    finite_nonneg(mib)?;
    if mib == 0.0 && round_trips == 0 {
        return Ok(0.0);
    }
    if mib > 0.0 && round_trips == 0 {
        return Err(ScheduleError::InvalidTransfer);
    }
    let trips = f64::from(round_trips);
    let wire_share_ms = 1000.0 * (mib / trips) / link.throughput_mib_s;
    Ok(link.queue_ms + trips * (link.rtt_ms + link.serialization_ms + wire_share_ms))
}

fn layer_ms(
    workload: WorkloadSpec,
    device: DeviceSpec,
    kv: KvMode,
    kv_multiplier: u32,
) -> Result<f64, ScheduleError> {
    if kv_multiplier != 1 && kv_multiplier != 2 {
        return Err(ScheduleError::InvalidInput);
    }
    let bytes = workload.layer_weights_gib()? * 2f64.powi(30)
        + f64::from(kv_multiplier) * kv.bytes_per_token_per_layer() as f64;
    let gib = bytes / 2f64.powi(30);
    Ok(gib * device.compute_ms_per_gib()?)
}

fn fits(device: DeviceSpec, model_gib: f64, kv_gib: f64) -> Result<bool, ScheduleError> {
    finite_nonneg(model_gib)?;
    finite_nonneg(kv_gib)?;
    let budget = device.budget_gib()?;
    let direct = model_gib + kv_gib <= budget;
    let scaled = placement_is_feasible(
        model_gib / 2f64.powi(20),
        kv_gib / 2f64.powi(20),
        0.0,
        budget / 2f64.powi(20),
    )?;
    if direct != scaled {
        return Err(ScheduleError::InvalidInput);
    }
    Ok(direct)
}

/// Python `round(value, 4)` for non-negative values (half to even).
fn round_4(value: f64) -> f64 {
    let scaled = value * 10_000.0;
    let floor = scaled.floor();
    let frac = scaled - floor;
    let rounded = if (frac - 0.5).abs() <= 1e-9 {
        if (floor as i64) % 2 == 0 {
            floor
        } else {
            floor + 1.0
        }
    } else {
        scaled.round()
    };
    rounded / 10_000.0
}

pub fn placement_row(
    workload: WorkloadSpec,
    k_local: u32,
    ctx_tokens: u64,
    kv: KvMode,
    local: DeviceSpec,
    remote: DeviceSpec,
    link: LinkSpec,
) -> Result<PlacementRow, ScheduleError> {
    workload.check()?;
    if k_local > workload.layers {
        return Err(ScheduleError::InvalidSplit);
    }
    let layers = workload.layers;
    let weights_layer = workload.layer_weights_gib()?;
    let weights_local = weights_layer * f64::from(k_local);
    let weights_remote = weights_layer * f64::from(layers - k_local);
    let kv_total = workload.kv_gib_total(ctx_tokens, kv)?;
    let kv_local = kv_total * f64::from(k_local) / f64::from(layers);
    let kv_remote = kv_total * f64::from(layers - k_local) / f64::from(layers);
    let fits_local = fits(local, weights_local, kv_local)?;
    let fits_remote = fits(remote, weights_remote, kv_remote)?;
    let feasible = fits_local && fits_remote;
    let deficit = if feasible {
        0.0
    } else {
        let deficit_local = (weights_local + kv_local - local.budget_gib()?).max(0.0);
        let deficit_remote = (weights_remote + kv_remote - remote.budget_gib()?).max(0.0);
        round_4(deficit_local.max(deficit_remote))
    };

    let cold_load_remote_s = if k_local == layers {
        0.0
    } else {
        transfer_latency_ms(link, weights_remote * 1024.0, 1)? / 1000.0
    };

    let per_tok_local = f64::from(k_local) * layer_ms(workload, local, kv, 1)?;
    let per_tok_remote = f64::from(layers - k_local) * layer_ms(workload, remote, kv, 1)?;
    let boundary_one = transfer_latency_ms(link, workload.hidden_mib(1)?, 1)?;
    let boundary_count = if k_local == layers { 0.0 } else { 2.0 };
    let boundary_overhead_ms_per_token = boundary_count * boundary_one;
    let decode_step_ms = per_tok_local + per_tok_remote + boundary_overhead_ms_per_token;
    let decode_tokens_per_s = if decode_step_ms == 0.0 {
        0.0
    } else {
        1000.0 / decode_step_ms
    };

    let n_ub = workload.prompt_tokens.div_ceil(workload.ubatch_tokens);
    let prefill_compute = workload.prompt_tokens as f64
        * (f64::from(k_local) * layer_ms(workload, local, kv, 2)?
            + f64::from(layers - k_local) * layer_ms(workload, remote, kv, 2)?);
    let prefill_boundary = if k_local == layers {
        0.0
    } else {
        let boundary_ubatch =
            transfer_latency_ms(link, workload.hidden_mib(workload.ubatch_tokens)?, 1)?;
        2.0 * n_ub as f64 * boundary_ubatch
    };
    let prefill_ms = prefill_compute + prefill_boundary;
    let prefill_tokens_per_s = if prefill_ms == 0.0 {
        0.0
    } else {
        1000.0 * workload.prompt_tokens as f64 / prefill_ms
    };

    Ok(PlacementRow {
        k_local,
        layers_remote: layers - k_local,
        weights_local_gib: weights_local,
        weights_remote_gib: weights_remote,
        kv_local_gib: kv_local,
        kv_remote_gib: kv_remote,
        feasible,
        deficit_gib: deficit,
        cold_load_remote_s,
        prefill_ms,
        prefill_tokens_per_s,
        decode_step_ms,
        decode_tokens_per_s,
        boundary_overhead_ms_per_token,
    })
}

pub fn placement_rows(
    workload: WorkloadSpec,
    ctx_tokens: u64,
    kv: KvMode,
    local: DeviceSpec,
    remote: DeviceSpec,
    link: LinkSpec,
) -> Result<Vec<PlacementRow>, ScheduleError> {
    (0..=workload.layers)
        .map(|k| placement_row(workload, k, ctx_tokens, kv, local, remote, link))
        .collect()
}

fn rank_key(row: &PlacementRow) -> (u8, f64, f64, f64, u32) {
    (
        if row.feasible { 0 } else { 1 },
        if row.feasible { 0.0 } else { row.deficit_gib },
        -row.decode_tokens_per_s,
        row.prefill_ms,
        row.k_local,
    )
}

pub fn rank_rows(rows: &mut [PlacementRow]) {
    rows.sort_by(|a, b| {
        let (a0, a1, a2, a3, a4) = rank_key(a);
        let (b0, b1, b2, b3, b4) = rank_key(b);
        a0.cmp(&b0)
            .then_with(|| a1.partial_cmp(&b1).unwrap_or(Ordering::Equal))
            .then_with(|| a2.partial_cmp(&b2).unwrap_or(Ordering::Equal))
            .then_with(|| a3.partial_cmp(&b3).unwrap_or(Ordering::Equal))
            .then_with(|| a4.cmp(&b4))
    });
}

/// Best feasible split under the WP-C rank: feasible first, then higher
/// modelled decode rate, then lower prefill latency, then lower `k`.
pub fn select_placement(
    workload: WorkloadSpec,
    ctx_tokens: u64,
    kv: KvMode,
    local: DeviceSpec,
    remote: DeviceSpec,
    link: LinkSpec,
) -> Result<PlacementRow, ScheduleError> {
    let mut rows = placement_rows(workload, ctx_tokens, kv, local, remote, link)?;
    rank_rows(&mut rows);
    rows.into_iter()
        .find(|row| row.feasible)
        .ok_or(ScheduleError::NoFeasiblePlacement)
}

/// Prefer the feasible site with the lower modelled latency.
/// Equal latency stays local.
pub fn choose_feasible_site(
    local_ms: f64,
    remote_ms: f64,
    local_feasible: bool,
    remote_feasible: bool,
) -> Result<ComputeSite, ScheduleError> {
    finite_nonneg(local_ms)?;
    finite_nonneg(remote_ms)?;
    match (local_feasible, remote_feasible) {
        (false, false) => Err(ScheduleError::NoFeasibleSite),
        (true, false) => Ok(ComputeSite::Local),
        (false, true) => Ok(ComputeSite::Remote),
        (true, true) if remote_ms < local_ms => Ok(ComputeSite::Remote),
        (true, true) => Ok(ComputeSite::Local),
    }
}

pub fn choose_site(
    local: DeviceSpec,
    remote: DeviceSpec,
    link: LinkSpec,
    item: WorkItem,
) -> Result<SiteDecision, ScheduleError> {
    finite_nonneg(item.local_compute_gib)?;
    finite_nonneg(item.remote_compute_gib)?;
    finite_nonneg(item.local_model_gib)?;
    finite_nonneg(item.local_kv_gib)?;
    finite_nonneg(item.remote_model_gib)?;
    finite_nonneg(item.remote_kv_gib)?;
    let local_ms = item.local_compute_gib * local.compute_ms_per_gib()?;
    let remote_ms = transfer_latency_ms(link, item.transfer_mib, item.round_trips)?
        + item.remote_compute_gib * remote.compute_ms_per_gib()?;
    let local_feasible = fits(local, item.local_model_gib, item.local_kv_gib)?;
    let remote_feasible = fits(remote, item.remote_model_gib, item.remote_kv_gib)?;
    let site = choose_feasible_site(local_ms, remote_ms, local_feasible, remote_feasible)?;
    Ok(SiteDecision {
        site,
        local_ms,
        remote_ms,
        local_feasible,
        remote_feasible,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(actual: f64, expected: f64) {
        let tol = 1e-9 * expected.abs().max(1.0);
        assert!(
            (actual - expected).abs() <= tol,
            "actual {actual} expected {expected} tol {tol}"
        );
    }

    #[test]
    fn declared_link_rates_match_the_rounded_formula() {
        let remote = (1000e6 / 8.0 / 2f64.powi(20) * 0.8 * 10_000.0).round() / 10_000.0;
        let local = (2500e6 / 8.0 / 2f64.powi(20) * 0.8 * 10_000.0).round() / 10_000.0;
        assert_eq!(LINK_REMOTE.throughput_mib_s, remote);
        assert_eq!(LINK_REMOTE.throughput_mib_s, 95.3674);
        assert_eq!(LINK_LOCAL.throughput_mib_s, local);
        assert_eq!(LINK_LOCAL.throughput_mib_s, 238.4186);
    }

    #[test]
    fn reference_q4_dedicated_split_matches_the_wp_c_model() {
        let rows = placement_rows(
            WORKLOAD_27B,
            131_072,
            KvMode::Q4,
            LOCAL_DEDICATED,
            REMOTE_DEVICE,
            LINK_REMOTE,
        )
        .unwrap();
        let feasible: Vec<u32> = rows
            .iter()
            .filter(|row| row.feasible)
            .map(|row| row.k_local)
            .collect();
        assert_eq!(feasible, vec![48, 49, 50, 51, 52, 53]);
        let best = select_placement(
            WORKLOAD_27B,
            131_072,
            KvMode::Q4,
            LOCAL_DEDICATED,
            REMOTE_DEVICE,
            LINK_REMOTE,
        )
        .unwrap();
        let again = select_placement(
            WORKLOAD_27B,
            131_072,
            KvMode::Q4,
            LOCAL_DEDICATED,
            REMOTE_DEVICE,
            LINK_REMOTE,
        )
        .unwrap();
        assert_eq!(best.k_local, 53);
        assert_eq!(again.k_local, 53);
        assert_eq!(best.layers_remote, 11);
        assert!(best.feasible);
        close(best.weights_local_gib, 12.233977031428367);
        close(best.weights_remote_gib, 2.539127308409661);
        close(best.kv_local_gib, 6.625);
        close(best.kv_remote_gib, 1.375);
        close(best.deficit_gib, 0.0);
        close(best.cold_load_remote_s, 27.264930920435);
        close(best.prefill_ms, 103_955.94146960336);
        close(best.prefill_tokens_per_s, 39.401307343242784);
        close(best.decode_step_ms, 27.87488155123564);
        close(best.decode_tokens_per_s, 35.87459190317786);
        close(best.boundary_overhead_ms_per_token, 2.663840054358198);
        close(again.decode_step_ms, best.decode_step_ms);
    }

    #[test]
    fn all_local_q4_is_infeasible_and_has_no_boundary() {
        let row = placement_row(
            WORKLOAD_27B,
            64,
            131_072,
            KvMode::Q4,
            LOCAL_DEDICATED,
            REMOTE_DEVICE,
            LINK_REMOTE,
        )
        .unwrap();
        assert!(!row.feasible);
        close(row.boundary_overhead_ms_per_token, 0.0);
        close(row.cold_load_remote_s, 0.0);
        close(row.decode_tokens_per_s, 54.15224020670045);
    }

    #[test]
    fn mixed_fp16_split_deficit_matches_the_reference_row() {
        let row = placement_row(
            WORKLOAD_27B,
            32,
            131_072,
            KvMode::Fp16,
            LOCAL_MIXED,
            REMOTE_DEVICE,
            LINK_REMOTE,
        )
        .unwrap();
        assert!(!row.feasible);
        close(row.deficit_gib, 17.3866);
        let err = select_placement(
            WORKLOAD_27B,
            131_072,
            KvMode::Fp16,
            LOCAL_MIXED,
            REMOTE_DEVICE,
            LINK_REMOTE,
        )
        .unwrap_err();
        assert_eq!(err, ScheduleError::NoFeasiblePlacement);
    }

    #[test]
    fn equal_cost_stays_local_and_cheaper_remote_moves() {
        assert_eq!(
            choose_feasible_site(5.0, 5.0, true, true).unwrap(),
            ComputeSite::Local
        );
        assert_eq!(
            choose_feasible_site(5.0, 4.9, true, true).unwrap(),
            ComputeSite::Remote
        );
        assert_eq!(
            choose_feasible_site(1.0, 9.0, true, false).unwrap(),
            ComputeSite::Local
        );
        assert_eq!(
            choose_feasible_site(9.0, 1.0, false, true).unwrap(),
            ComputeSite::Remote
        );
        assert_eq!(
            choose_feasible_site(1.0, 1.0, false, false).unwrap_err(),
            ScheduleError::NoFeasibleSite
        );
        assert_eq!(
            choose_feasible_site(f64::NAN, 1.0, true, true).unwrap_err(),
            ScheduleError::InvalidInput
        );
    }

    #[test]
    fn expensive_transfer_stays_local_when_both_fit() {
        let item = WorkItem {
            local_compute_gib: 1.0,
            remote_compute_gib: 1.0,
            transfer_mib: 100.0,
            round_trips: 1,
            local_model_gib: 1.0,
            local_kv_gib: 0.0,
            remote_model_gib: 1.0,
            remote_kv_gib: 0.0,
        };
        let decision = choose_site(LOCAL_DEDICATED, REMOTE_DEVICE, LINK_REMOTE, item).unwrap();
        assert_eq!(decision.site, ComputeSite::Local);
        assert!(decision.local_feasible && decision.remote_feasible);
        assert!(decision.remote_ms > decision.local_ms);
    }

    #[test]
    fn local_budget_miss_selects_remote() {
        let tiny_local = DeviceSpec {
            name: "tiny",
            total_vram_gib: 1.0,
            occupied_gib: 0.9,
            bandwidth_gib_s: 800.0,
            workspace_reserve_gib: 0.05,
        };
        let item = WorkItem {
            local_compute_gib: 0.1,
            remote_compute_gib: 0.1,
            transfer_mib: 1.0,
            round_trips: 1,
            local_model_gib: 1.0,
            local_kv_gib: 0.0,
            remote_model_gib: 0.1,
            remote_kv_gib: 0.0,
        };
        let decision = choose_site(tiny_local, REMOTE_DEVICE, LINK_REMOTE, item).unwrap();
        assert_eq!(decision.site, ComputeSite::Remote);
        assert!(!decision.local_feasible);
        assert!(decision.remote_feasible);
    }

    #[test]
    fn invalid_split_and_transfer_are_rejected() {
        assert_eq!(
            placement_row(
                WORKLOAD_27B,
                65,
                131_072,
                KvMode::Q4,
                LOCAL_DEDICATED,
                REMOTE_DEVICE,
                LINK_REMOTE,
            )
            .unwrap_err(),
            ScheduleError::InvalidSplit
        );
        assert_eq!(
            transfer_latency_ms(LINK_REMOTE, 1.0, 0).unwrap_err(),
            ScheduleError::InvalidTransfer
        );
        assert_eq!(transfer_latency_ms(LINK_REMOTE, 0.0, 0).unwrap(), 0.0);
        let mut bad = LOCAL_DEDICATED;
        bad.bandwidth_gib_s = 0.0;
        assert_eq!(bad.budget_gib().unwrap_err(), ScheduleError::InvalidInput);
    }
}
