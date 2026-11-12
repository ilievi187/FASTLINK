"""FL3-C1: VRAM-aware layer placement cost model (deterministic, CPU-only).

Planning artifact for WP-C on FASTLINK v3. Every number printed here is
*modelled*: declared arithmetic over the shared inputs in qa/sim_common.py.
Nothing is a measurement, an observation, a GPU probe or a telemetry value.

Placement space
---------------
The workload is split *contiguously by decoder layer*: ``k`` layers on the
local device, ``WORKLOAD.layers - k`` layers on the remote device, residual
hidden states crossing one declared hop (LINK_REMOTE) per direction.

Per-split costs
---------------
- decode (single autoregressive token, strictly synchronous handoff):
  * local compute  = k    * per-layer roofline (weights + KV read)/bandwidth
  * remote compute = (L-k)* per-layer roofline (weights + KV read)/bandwidth
  * boundary       = 2 round trips (out and back) carrying one token's
    residual stream through network_sim.Link (rtt + serialization + queue +
    wire). Boundary count is 0 only for the all-local split (k == L).
- prefill (prompt_tokens tokens, STREAMED in ubatch RPCs): the per-token
  per-layer roofline reads the layer weights once and charges KV write
  accounting as declared (2x KV bytes for read+write); boundary traffic is
  ceil(prompt/ubatch) round trips per direction, 1 RPC per ubatch.
- cold load: the remote device's weight share streams over LINK_REMOTE in
  one modelled round trip. Local weights are declared to load locally
  (zero modelled network cost).
- VRAM feasibility via cost_model.placement_is_feasible: weights + KV-share
  + workspace reserve must fit the declared usable VRAM on BOTH devices.

Ranking
-------
Feasible splits first, then by modelled decode tok/s descending, prefill
latency ascending, then deficit and k ascending. Infeasible splits are
retained and flagged: the model shows *why* they do not fit.
"""
from __future__ import annotations

import math
from dataclasses import dataclass

import network_sim
from cost_model import placement_is_feasible

import sim_common as sc
from sim_common import (
    KV_LABELS,
    LINK_REMOTE,
    LOCAL_DEDICATED,
    LOCAL_MIXED,
    REMOTE_DEVICE,
    WORKLOAD,
    Device,
    kv_bytes_per_token_per_layer,
)

BOTH_LOCAL_DEVICES = (LOCAL_MIXED, LOCAL_DEDICATED)


# ---------------------------------------------------------------------------
# Validation helpers
# ---------------------------------------------------------------------------


def validate_split(k_local: object) -> int:
    if not isinstance(k_local, int) or isinstance(k_local, bool):
        raise ValueError("k_local must be an integer")
    if not 0 <= k_local <= WORKLOAD.layers:
        raise ValueError(f"k_local must be within [0, {WORKLOAD.layers}]")
    return k_local


# ---------------------------------------------------------------------------
# Per-token roofline helpers (declared arithmetic)
# ---------------------------------------------------------------------------


def layer_ms(device: Device, kv_bpl: int, kv_multiplier: int) -> float:
    """Roofline milliseconds per token, per layer, on ``device``.

    Bytes charged per token per layer: layer weights + KV traffic at the
    declared multiplier (decode: read 1x; prefill: read+write 2x).
    """
    if kv_multiplier not in (1, 2):
        raise ValueError("kv_multiplier must be 1 or 2")
    bytes_per_token_layer = (
        WORKLOAD.layer_weights_gib * 2**30 + kv_multiplier * kv_bpl
    )
    gib = bytes_per_token_layer / 2**30
    return gib * device.compute_ms_per_gib()


def boundary_ms(tokens: int, link: network_sim.Link = LINK_REMOTE) -> float:
    """One-direction boundary transfer for ``tokens`` residual-stream values."""
    result = network_sim.transfer(link, WORKLOAD.hidden_mib(tokens), 1)
    return result.latency_ms


# ---------------------------------------------------------------------------
# Matrix rows
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class MatrixRow:
    k_local: int
    layers_remote: int
    weights_local_gib: float
    weights_remote_gib: float
    kv_local_gib: float
    kv_remote_gib: float
    feasible: bool
    deficit_gib: float          # worst-side GiB over budget (0 if feasible)
    cold_load_remote_s: float
    prefill_ms: float
    prefill_tokens_per_s: float
    decode_step_ms: float
    decode_tokens_per_s: float
    boundary_overhead_ms_per_token: float

    def __post_init__(self) -> None:
        validate_split(self.k_local)
        if not all(math.isfinite(v) for v in (
            self.weights_local_gib, self.weights_remote_gib,
            self.kv_local_gib, self.kv_remote_gib, self.deficit_gib,
            self.cold_load_remote_s, self.prefill_ms,
            self.prefill_tokens_per_s, self.decode_step_ms,
            self.decode_tokens_per_s, self.boundary_overhead_ms_per_token,
        )):
            raise ValueError("matrix row values must be finite")
        if min(self.cold_load_remote_s, self.prefill_ms, self.decode_step_ms,
               self.boundary_overhead_ms_per_token,
               self.deficit_gib) < 0 or self.decode_tokens_per_s < 0:
            raise ValueError("matrix row latencies must be non-negative")


def compute_row(k_local: int, ctx_tokens: int, kv_label: str,
                local: Device = LOCAL_DEDICATED,
                remote: Device = REMOTE_DEVICE) -> MatrixRow:
    """Modelled costs of the contiguous split ``k_local`` / ``L - k_local``."""
    validate_split(k_local)
    if kv_label not in KV_LABELS:
        raise ValueError(f"unknown KV mode: {kv_label!r}")
    if not isinstance(ctx_tokens, int) or isinstance(ctx_tokens, bool) or ctx_tokens < 0:
        raise ValueError("ctx_tokens must be a non-negative integer")

    kv_bpl = kv_bytes_per_token_per_layer(kv_label)
    kv_total = sc.kv_gib_total(ctx_tokens, kv_label)
    layers = WORKLOAD.layers
    weights_layer = WORKLOAD.layer_weights_gib

    weights_local = weights_layer * k_local
    weights_remote = weights_layer * (layers - k_local)
    kv_local = kv_total * k_local / layers
    kv_remote = kv_total * (layers - k_local) / layers

    # Workspace reserves are already excluded from Device.budget_gib, so the
    # feasibility check charges weights + KV against (usable - reserve).
    fits_local = weights_local + kv_local <= local.budget_gib
    fits_remote = weights_remote + kv_remote <= remote.budget_gib
    feasible = fits_local and fits_remote
    deficit = 0.0
    if not feasible:
        deficit_local = max(0.0, weights_local + kv_local - local.budget_gib)
        deficit_remote = max(0.0, weights_remote + kv_remote - remote.budget_gib)
        deficit = round(max(deficit_local, deficit_remote), 4)

    cold_load_remote_s = 0.0
    if k_local < layers:
        cold_load_remote_s = (
            network_sim.transfer(LINK_REMOTE, weights_remote * 1024.0, 1)
            .latency_ms / 1000.0
        )

    # ---- decode: strictly synchronous single-token step ---------------------
    per_tok_local = k_local * layer_ms(local, kv_bpl, 1)
    per_tok_remote = (layers - k_local) * layer_ms(remote, kv_bpl, 1)
    boundary_count = 0 if k_local == layers else 2
    boundary_cost = boundary_count * boundary_ms(1)
    boundary_overhead_ms_per_token = boundary_cost
    decode_step_ms = per_tok_local + per_tok_remote + boundary_cost
    decode_tokens_per_s = 0.0 if decode_step_ms == 0 else 1000.0 / decode_step_ms

    # ---- prefill: prompt streamed through ubatch-sized RPCs ------------------
    n_ub = math.ceil(WORKLOAD.prompt_tokens / WORKLOAD.ubatch_tokens)
    prefill_compute = (
        WORKLOAD.prompt_tokens * k_local * layer_ms(local, kv_bpl, 2)
        + WORKLOAD.prompt_tokens * (layers - k_local) * layer_ms(remote, kv_bpl, 2)
    )
    prefill_boundary = (
        0 if k_local == layers else 2 * n_ub * boundary_ms(WORKLOAD.ubatch_tokens)
    )
    prefill_ms = prefill_compute + prefill_boundary
    prefill_tokens_per_s = 0.0 if prefill_ms == 0 else (
        1000.0 * WORKLOAD.prompt_tokens / prefill_ms
    )

    # `placement_is_feasible` is the canonical check used by the base CPU
    # model; the direct budget comparison above must agree with it.
    assert placement_is_feasible(
        weights_local / 2**20, kv_local / 2**20,
        0.0, local.budget_gib / 2**20
    ) == fits_local
    assert placement_is_feasible(
        weights_remote / 2**20, kv_remote / 2**20,
        0.0, remote.budget_gib / 2**20
    ) == fits_remote

    return MatrixRow(
        k_local=k_local,
        layers_remote=layers - k_local,
        weights_local_gib=weights_local,
        weights_remote_gib=weights_remote,
        kv_local_gib=kv_local,
        kv_remote_gib=kv_remote,
        feasible=feasible,
        deficit_gib=deficit,
        cold_load_remote_s=cold_load_remote_s,
        prefill_ms=prefill_ms,
        prefill_tokens_per_s=prefill_tokens_per_s,
        decode_step_ms=decode_step_ms,
        decode_tokens_per_s=decode_tokens_per_s,
        boundary_overhead_ms_per_token=boundary_overhead_ms_per_token,
    )


def compute_matrix(ctx_tokens: int, kv_label: str, local: Device) -> list:
    return [
        compute_row(k, ctx_tokens, kv_label, local)
        for k in range(WORKLOAD.layers + 1)
    ]


def rank_rows(rows: list) -> list:
    """Feasible first (decode tok/s desc), then infeasible by deficit."""
    return sorted(
        rows,
        key=lambda r: (
            0 if r.feasible else 1,
            0.0 if r.feasible else r.deficit_gib,
            -r.decode_tokens_per_s,
            r.prefill_ms,
            r.k_local,
        ),
    )


def feasible_ks(rows: list) -> list:
    return [r.k_local for r in rows if r.feasible]


def best_row(rows: list):
    ranked = rank_rows(rows)
    return ranked[0] if ranked and ranked[0].feasible else None


# ---------------------------------------------------------------------------
# Output
# ---------------------------------------------------------------------------


def _fmt_row(rank: int, r: MatrixRow) -> str:
    return (
        f"{rank:>2} | {r.k_local:>2}/{r.layers_remote:<2} |"
        f" {r.weights_local_gib:>6.2f} {r.kv_local_gib:>5.2f} |"
        f" {r.weights_remote_gib:>5.2f} {r.kv_remote_gib:>5.2f} |"
        f" {('yes' if r.feasible else 'no '):>3} |"
        f" {r.deficit_gib:>6.2f} |"
        f" {r.cold_load_remote_s:>7.1f} |"
        f" {r.prefill_ms:>11.0f} {r.prefill_tokens_per_s:>5.1f} |"
        f" {r.decode_step_ms:>7.2f} {r.decode_tokens_per_s:>5.1f} |"
        f" {r.boundary_overhead_ms_per_token:>5.2f}"
    )


MATRIX_SEP = "-" * 118


MATRIX_HEADER = (
    "# | split l/r | w loc | kv loc | w rem | kv rem | fit | deficit"
    " | cold s | prefill ms | prefl t/s | step ms | t/s | bnd ms"
)


def print_matrix(title: str, rows: list) -> None:
    print(f"--- ranked placement matrix: {title} ---")
    print(MATRIX_SEP)
    print(MATRIX_HEADER)
    print(MATRIX_SEP)
    for rank, row in enumerate(rank_rows(rows), start=1):
        print(_fmt_row(rank, row))
    print(MATRIX_SEP)


def print_assumptions() -> None:
    print("=== FL3-C1: VRAM-aware layer placement cost model (SIMULATED) ===")
    print("All values below are MODEL outputs from declared inputs. No GPU")
    print("probe, benchmark, or telemetry value is used. Declared inputs:")
    print(f"  workload            : {WORKLOAD.name}")
    print(f"  params              : {WORKLOAD.params_b:.1f} B x "
          f"{WORKLOAD.bits_per_weight} bpw (declared) = "
          f"{WORKLOAD.weights_gib:.3f} GiB weights")
    print(f"  layers              : {WORKLOAD.layers} (split unit)"
          f" | hidden {WORKLOAD.hidden_dim} x "
          f"{WORKLOAD.activation_bytes} B = "
          f"{WORKLOAD.hidden_mib(1) * 1024:.4f} KiB per boundary value")
    print(f"  KV/token/layer fp16 : {sc.KV_BYTES_PER_TOKEN_PER_LAYER_FP16} B"
          f" | KV modes: "
          + ", ".join(f"{k}={v:g}" for k, v in KV_LABELS.items()))
    print(f"  prefill workload    : {WORKLOAD.prompt_tokens} tokens, "
          f"{WORKLOAD.ubatch_tokens}-token ubatches")
    for device in (LOCAL_MIXED, LOCAL_DEDICATED, REMOTE_DEVICE):
        print(f"  device              : {device.name} usable "
              f"{device.usable_vram_gib:.1f} GiB (-{device.workspace_reserve_gib:.1f}"
              f" reserve -> budget {device.budget_gib:.1f}), declared"
              f" {device.bandwidth_gib_s:.0f} GiB/s roofline")
    for link_name, link in (("LINK_REMOTE (boundary hop)", LINK_REMOTE),):
        print(f"  link                : {link_name} rtt {link.rtt_ms} ms,"
              f" {link.throughput_mib_s} MiB/s, ser {link.serialization_ms} ms,"
              f" queue {link.queue_ms} ms (declared)")
    print("  local-side link     : used only for local disk->VRAM loading"
          " (modelled zero network cost)")
    print()


def print_scenario_grid() -> None:
    print("--- scenario grid: context x KV mode x local device ---------")
    header = (
        f"{'ctx':>4} {'KV':>4} {'local':>9} | {'feas ks':>22} |"
        f" {'best k':>6} {'t/s':>5} {'step ms':>7} {'prefill ms':>10}"
        f" {'prefl t/s':>9} {'cold s':>7} {'max B*':>6}"
    )
    print(header)
    print("-" * (len(header) + 2))
    for ctx_label, ctx in (("128K", 131072), ("64K", 65536), ("32K", 32768)):
        for kv_label in KV_LABELS:
            for local in BOTH_LOCAL_DEVICES:
                rows = compute_matrix(ctx, kv_label, local)
                ks = feasible_ks(rows)
                best = best_row(rows)
                tag = local.name.split("(")[-1].rstrip(")").split()[0]
                if best is None:
                    print(f"{ctx_label:>4} {kv_label:>4} {tag:>9} | "
                          f"{'INFEASIBLE':>22} | {'-':>6} {'-':>5} {'-':>7}"
                          f" {'-':>10} {'-':>9} {'-':>7} {'-':>6}")
                    continue
                max_b = max_decode_streams(best, ctx, kv_label, local)
                print(f"{ctx_label:>4} {kv_label:>4} {tag:>9} | "
                      f"{(str(min(ks)) + '..' + str(max(ks)))if ks else '-':>22} |"
                      f" {best.k_local:>6} {best.decode_tokens_per_s:>5.1f}"
                      f" {best.decode_step_ms:>7.2f} {best.prefill_ms:>10.0f}"
                      f" {best.prefill_tokens_per_s:>9.1f}"
                      f" {best.cold_load_remote_s:>7.1f} {max_b:>6}")
    print()
    print("* max decode streams = floor of local and remote KV-side budget")
    print("  divided by per-stream KV share at the best split (fixed k).")
    print()


def max_decode_streams(row: MatrixRow, ctx_tokens: int, kv_label: str,
                       local: Device) -> int:
    """How many independent 128K-class sequences the split can batch.

    Decode-side batching stores one KV share sequence per stream, so
    B_max = floor(KV budget / per-stream KV share) on the tight side.
    Return 0 when even one stream does not fit (row was infeasible).
    """
    if not row.feasible:
        return 0
    kv_bpl = kv_bytes_per_token_per_layer(kv_label)
    # Per-stream KV share: ctx * bytes-per-token-per-layer * layer count.
    per_stream_local = ctx_tokens * kv_bpl * row.k_local / 2**30
    per_stream_remote = (
        ctx_tokens * kv_bpl * row.layers_remote / 2**30
    )
    kv_space_local = local.budget_gib - row.weights_local_gib
    kv_space_remote = REMOTE_DEVICE.budget_gib - row.weights_remote_gib
    local_streams = math.floor(kv_space_local / per_stream_local) if per_stream_local else 0
    remote_streams = (
        math.floor(kv_space_remote / per_stream_remote)
        if per_stream_remote else 999
    )
    return max(0, min(local_streams, remote_streams))


def print_conclusions() -> None:
    print("--- computed conclusions (model arithmetic only) -------------")
    for ctx_label, ctx in (("128K", 131072), ("64K", 65536), ("32K", 32768)):
        for kv_label in KV_LABELS:
            for local in BOTH_LOCAL_DEVICES:
                rows = compute_matrix(ctx, kv_label, local)
                ks = feasible_ks(rows)
                tag = (local.name.split("(")[-1].rstrip(")").split()[0])
                if ks:
                    print(f"  {ctx_label} {kv_label} @{tag}: feasible k = "
                          f"{min(ks)}..{max(ks)}")
                else:
                    print(f"  {ctx_label} {kv_label} @{tag}: infeasible at"
                          f" every split")
    print()


def main() -> None:
    print_assumptions()
    print_scenario_grid()
    # Full matrices for the two most decision-relevant scenarios.
    print_matrix("131072-token ctx / fp16 KV / LOCAL_MIXED",
                 compute_matrix(131072, "fp16", LOCAL_MIXED))
    print_matrix("131072-token ctx / q4 KV / LOCAL_DEDICATED",
                 compute_matrix(131072, "q4", LOCAL_DEDICATED))
    print_conclusions()


if __name__ == "__main__":
    main()
