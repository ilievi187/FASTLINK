"""FL3-C2: prefill-vs-decode pipeline model (deterministic, CPU-only).

Quantifies where batching/pipelining of independent RPC calls saves time
versus a strictly synchronous serial step, using the declared reference
placement from FL3-C1 (k=53 local layers, q4 KV, LOCAL_DEDICATED, one
boundary hop LINK_REMOTE).

INTERPRETATION GUARDS (modelled, never measured):

- A single strictly autoregressive stream is dependency-chained: local
  layer-k+1 compute needs token t's remote result, nothing on the other
  device can run "in parallel" for that stream, and per-token boundary
  round trips cannot overlap anything. That mode is FLOOR (strict sync).
- Batching (MODE B) merges B independent sequences into one decode step:
  one RPC round trip per direction carries B residual streams, amortising
  RTT/queue/serialization across the batch. Throughput improves, but
  per-token compute is unchanged and KV storage scales with B. The VRAM
  B_max comes from sim_placement.max_decode_streams.
- MODE P (pipelined two-stage) is an IDEALIZED UPPER BOUND: both devices
  stay busy on different batches and the link stays busy on transfers;
  scheduler overhead, KV append serialization and sampler/host latency
  are declared out of model. It can never beat the single-device roofline
  ``1000 / per-token compute`` of the busier side.
- Prefill scheduling: UBATCH_MAJOR = llama.cpp-RPC-style, each ubatch is
  an own RPC chain and the next ubatch waits for the previous one
  end-to-end (strictly serial). LAYER_MAJOR = declared two-stage pipeline
  where the local ubatch i+1 computes while ubatch i runs remotely; every
  ubatch still crosses the full layer block once per direction.
"""
from __future__ import annotations

import math
from dataclasses import dataclass

import sim_placement as sp
from sim_common import (
    KV_LABELS,
    LINK_REMOTE,
    LOCAL_DEDICATED,
    REMOTE_DEVICE,
    WORKLOAD,
    kv_bytes_per_token_per_layer,
)

REF_SPLIT_K_LOCAL = 53
REF_KV_LABEL = "q4"
REF_LOCAL_DEVICE = LOCAL_DEDICATED

SCENARIO_LABELS = ("128K", "64K", "32K")


def _validate_ctx_label(label: object) -> int:
    mapping = {"128K": 131072, "64K": 65536, "32K": 32768}
    if not isinstance(label, str) or label not in mapping:
        raise ValueError(f"unknown context label: {label!r}")
    return mapping[label]


# ---------------------------------------------------------------------------
# Per-token model constants for the reference split
# ---------------------------------------------------------------------------


def per_token_local_ms(kv_label: str = REF_KV_LABEL,
                       k_local: int = REF_SPLIT_K_LOCAL) -> float:
    return (
        k_local * sp.layer_ms(REF_LOCAL_DEVICE,
                              kv_bytes_per_token_per_layer(kv_label), 1)
    )


def per_token_remote_ms(kv_label: str = REF_KV_LABEL,
                        k_local: int = REF_SPLIT_K_LOCAL) -> float:
    return (
        (WORKLOAD.layers - k_local)
        * sp.layer_ms(REMOTE_DEVICE, kv_bytes_per_token_per_layer(kv_label), 1)
    )


def boundary_one_stream_ms() -> float:
    """One-direction boundary cost for a single residual stream."""
    return sp.boundary_ms(1)


# ---------------------------------------------------------------------------
# Decode modes
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class StepResult:
    mode: str
    batch_streams: int
    vram_feasible: bool
    step_ms: float
    tokens_per_s: float


def _validate_batch(b: object) -> int:
    if not isinstance(b, int) or isinstance(b, bool) or b <= 0:
        raise ValueError("batch streams must be a positive integer")
    return b


def strict_sync_ms(kv_label: str = REF_KV_LABEL,
                   k_local: int = REF_SPLIT_K_LOCAL) -> float:
    """MODE S single stream: per-token RTTs are strictly serialized."""
    return (
        per_token_local_ms(kv_label, k_local)
        + per_token_remote_ms(kv_label, k_local)
        + 2 * sp.boundary_ms(1)
    )


def batched_sync_ms(b: int, kv_label: str = REF_KV_LABEL,
                    k_local: int = REF_SPLIT_K_LOCAL) -> float:
    """MODE B: one RPC per direction carries B residual streams."""
    validated = _validate_batch(b)
    per_stream = per_token_local_ms(kv_label, k_local) \
        + per_token_remote_ms(kv_label, k_local)
    return validated * per_stream + 2 * sp.boundary_ms(validated)


def batched_sync_result(b: int, ctx_tokens: int, kv_label: str) -> StepResult:
    b = _validate_batch(b)
    ref_row = sp.compute_row(REF_SPLIT_K_LOCAL, ctx_tokens, kv_label,
                             REF_LOCAL_DEVICE)
    streams = sp.max_decode_streams(ref_row, ctx_tokens, kv_label,
                                    REF_LOCAL_DEVICE)
    step = batched_sync_ms(b, kv_label)
    return StepResult(
        mode="B",
        batch_streams=b,
        vram_feasible=(b <= streams),
        step_ms=step,
        tokens_per_s=0.0 if step == 0 else 1000.0 * b / step,
    )


def pipelined_ideal_ms(b: int, kv_label: str = REF_KV_LABEL,
                       k_local: int = REF_SPLIT_K_LOCAL) -> float:
    """MODE P steady-state cycle: both devices + link in flight (upper bound).

    The local device chain, the remote device chain and the hop (one
    direction after the other) run as independent servers; the cycle is the
    max of the three. Requires at least 2 resident streams to be legal.
    """
    validated = _validate_batch(b)
    local_chain = validated * per_token_local_ms(kv_label, k_local)
    remote_chain = validated * per_token_remote_ms(kv_label, k_local)
    link_chain = 2 * sp.boundary_ms(validated)
    return max(local_chain, remote_chain, link_chain)


def pipelined_ideal_result(b: int, ctx_tokens: int, kv_label: str) -> StepResult:
    b = _validate_batch(b)
    ref_row = sp.compute_row(REF_SPLIT_K_LOCAL, ctx_tokens, kv_label,
                             REF_LOCAL_DEVICE)
    streams = sp.max_decode_streams(ref_row, ctx_tokens, kv_label,
                                    REF_LOCAL_DEVICE)
    cycle = pipelined_ideal_ms(b, kv_label)
    return StepResult(
        mode="P",
        batch_streams=b,
        vram_feasible=(b <= streams and b >= 2),
        step_ms=cycle,
        tokens_per_s=0.0 if cycle <= 0 else 1000.0 * b / cycle,
    )


# ---------------------------------------------------------------------------
# Prefill scheduling
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class PrefillResult:
    mode: str
    ubatch_tokens: int
    total_ms: float
    steady_tokens_per_s: float
    prompt_tokens_per_s: float
    boundary_ms_total: float


def _validate_ubatch(u: object) -> int:
    if not isinstance(u, int) or isinstance(u, bool) or u <= 0:
        raise ValueError("ubatch size must be a positive integer")
    return u


def prefill_kv_bpl(kv_label: str) -> int:
    if kv_label not in KV_LABELS:
        raise ValueError(f"unknown KV mode: {kv_label!r}")
    return kv_bytes_per_token_per_layer(kv_label)


def ubatch_major_prefill(ubatch: int, kv_label: str = REF_KV_LABEL,
                         k_local: int = REF_SPLIT_K_LOCAL) -> PrefillResult:
    """MODE UB: every ubatch is an own RPC chain (strictly serial handoff)."""
    ub = _validate_ubatch(ubatch)
    prompt = WORKLOAD.prompt_tokens
    n_ub = math.ceil(prompt / ub)
    kv_bpl = prefill_kv_bpl(kv_label)
    per_ub_local = ub * k_local * sp.layer_ms(REF_LOCAL_DEVICE, kv_bpl, 2)
    per_ub_remote = ub * (WORKLOAD.layers - k_local) * sp.layer_ms(
        REMOTE_DEVICE, kv_bpl, 2)
    per_ub_boundary = 2 * sp.boundary_ms(ub)
    per_ub = per_ub_local + per_ub_remote + per_ub_boundary
    total = n_ub * per_ub
    return PrefillResult(
        mode="UB",
        ubatch_tokens=ub,
        total_ms=total,
        steady_tokens_per_s=0.0 if per_ub <= 0 else 1000.0 * ub / per_ub,
        prompt_tokens_per_s=0.0 if total <= 0 else 1000.0 * prompt / total,
        boundary_ms_total=n_ub * per_ub_boundary,
    )


def layer_major_prefill(ubatch: int, kv_label: str = REF_KV_LABEL,
                        k_local: int = REF_SPLIT_K_LOCAL) -> PrefillResult:
    """MODE LM: two-stage pipeline; ubatch i+1 local-computes while ubatch i
    runs remotely. Cycle = max(stage durations); prompt total carries one
    fill/tail penalty declared as ``L + R + 2b`` once.
    """
    ub = _validate_ubatch(ubatch)
    prompt = WORKLOAD.prompt_tokens
    n_ub = math.ceil(prompt / ub)
    kv_bpl = prefill_kv_bpl(kv_label)
    per_ub_local = ub * k_local * sp.layer_ms(REF_LOCAL_DEVICE, kv_bpl, 2)
    per_ub_remote = ub * (WORKLOAD.layers - k_local) * sp.layer_ms(
        REMOTE_DEVICE, kv_bpl, 2)
    per_ub_boundary = 2 * sp.boundary_ms(ub)
    cycle = max(per_ub_local, per_ub_remote, per_ub_boundary)
    total = (n_ub - 1) * cycle + per_ub_local + per_ub_remote + per_ub_boundary
    return PrefillResult(
        mode="LM",
        ubatch_tokens=ub,
        total_ms=total,
        steady_tokens_per_s=0.0 if cycle <= 0 else 1000.0 * ub / cycle,
        prompt_tokens_per_s=0.0 if total <= 0 else 1000.0 * prompt / total,
        boundary_ms_total=n_ub * per_ub_boundary,
    )


# ---------------------------------------------------------------------------
# Output
# ---------------------------------------------------------------------------

DECODE_TABLE_CTXS = ((SCENARIO_LABELS[0], 131072), (SCENARIO_LABELS[1], 65536),
                     (SCENARIO_LABELS[2], 32768))


def print_assumptions() -> None:
    print("=== FL3-C2: prefill-vs-decode pipeline model (SIMULATED) ===")
    print("All values are MODEL arithmetic. Reference placement (from"
          " FL3-C1):")
    print(f"  split               : {REF_SPLIT_K_LOCAL}/{WORKLOAD.layers - REF_SPLIT_K_LOCAL}"
          f" layers local/remote, {REF_KV_LABEL} KV,"
          " LOCAL_DEDICATED + REMOTE")
    ptk_l = per_token_local_ms()
    ptk_r = per_token_remote_ms()
    bnd = sp.boundary_ms(1)
    print(f"  per-token compute   : local {ptk_l:.3f} ms + remote"
          f" {ptk_r:.3f} ms = {ptk_l + ptk_r:.3f} ms/token (roofline)")
    print(f"  boundary per token  : {2 * bnd:.3f} ms/token (2 directions x"
          f" {bnd:.3f} ms: queue + rtt + ser + wire 8 KiB)")
    print("  strictly sync stream: dependency-chained, no overlap possible")
    print("  MODE B              : B independent streams share one RPC per")
    print("                        direction per step; KV stores x B. B_max")
    print("                        is VRAM-limited (C1 max_decode_streams).")
    print("  MODE P (ideal)      : both devices + link never idle; declared")
    print("                        UPPER BOUND, excludes scheduler/KV/sampler")
    print("                        overhead; >= 2 streams required.")
    print("  MODE UB prefill     : llama.cpp-RPC style: each ubatch waits for")
    print("                        the previous one end-to-end.")
    print("  MODE LM prefill     : two-stage pipeline, one fill/tail penalty.")
    print("  excluded            : host sampler, framework CPU time, queue")
    print("                        growth, thermal effects (all declared 0).")
    print()


def print_decode_model() -> None:
    ptk_l = per_token_local_ms()
    ptk_r = per_token_remote_ms()
    bnd = 2 * sp.boundary_ms(1)
    step = strict_sync_ms()
    print("--- decode step model (single token) ------------------------")
    print(f"  local compute chain : {ptk_l:10.3f} ms/token "
          f"({100 * ptk_l / (ptk_l + ptk_r + bnd):.1f}% of step)")
    print(f"  remote compute chain: {ptk_r:10.3f} ms/token "
          f"({100 * ptk_r / (ptk_l + ptk_r + bnd):.1f}% of step)")
    print(f"  boundary RTTs       : {bnd:10.3f} ms/token "
          f"({100 * bnd / (ptk_l + ptk_r + bnd):.1f}% of step)")
    print(f"  MODE S step latency : {step:10.3f} ms/token -> "
          f"{1000.0 / step:6.1f} tok/s (strict sync, 1 stream)")
    amp = 1000.0 / (ptk_l + ptk_r)
    print(f"  no-RTT compute floor: {ptk_l + ptk_r:10.3f} ms/token -> "
          f"{amp:6.1f} tok/s (RTT fully amortised asymptote)")
    roof = 1000.0 / max(ptk_l, ptk_r)
    print(f"  device roofline     : {max(ptk_l, ptk_r):10.3f} ms/token -> "
          f"{roof:6.1f} tok/s (pipelining cap: busier device)")
    print(f"  computed ceiling gap: batching/pipelining can recover at most "
          f"{(roof - 1000.0 / step):.1f} tok/s = "
          f"{(100 * roof / (1000.0 / step) - 100):.0f}%")
    print()


def print_decode_batches() -> None:
    print("--- decode batching: VRAM-limited B over context ------------")
    header = (f"{'ctx':>4} {'B_max':>5} | {'MODE S t/s':>10} |"
              f" {'MODE B t/s':>10} {'step ms':>9} | {'MODE P t/s':>10}"
              f" {'cycle ms':>9} | {'delta t/s':>9}")
    print(header)
    print("-" * (len(header) + 2))
    base = 1000.0 / strict_sync_ms()
    for label, ctx in DECODE_TABLE_CTXS:
        b_max_result = sp.compute_row(REF_SPLIT_K_LOCAL, ctx, REF_KV_LABEL,
                                      REF_LOCAL_DEVICE)
        b_max = sp.max_decode_streams(b_max_result, ctx, REF_KV_LABEL,
                                      REF_LOCAL_DEVICE)
        if b_max <= 1:
            print(f"{label:>4} {b_max:>5} | {base:>10.1f} |"
                  f" {'n/a (B=1)':>10} {'-':>9} |"
                  f" {'n/a (B=1)':>10} {'-':>9} | {'+0.0':>9}")
            continue
        b_res = batched_sync_result(min(b_max, 8), ctx, REF_KV_LABEL)
        p_res = pipelined_ideal_result(min(b_max, 8), ctx, REF_KV_LABEL)
        print(f"{label:>4} {b_max:>5} | {base:>10.1f} |"
              f" {b_res.tokens_per_s:>10.1f} {b_res.step_ms:>9.1f} |"
              f" {p_res.tokens_per_s:>10.1f} {p_res.step_ms:>9.1f} |"
              f" {'+' + format(p_res.tokens_per_s - base, '.1f'):>9}")
    print()
    print("(B capped at 8 in the table; MODE P requires B >= 2 streams.)")
    print()


def print_prefill_model() -> None:
    print("--- prefill scheduling: 4096-token prompt -------------------")
    header = (f"{'ub':>5} {'rid':>2} | {'total ms':>10} {'prompt t/s':>10} |"
              f" {'LM total ms':>11} {'LM t/s':>7} {'LM steady t/s':>13} |"
              f" {'LM saving ms':>12}")
    print(header)
    print("-" * (len(header) + 2))
    for ub in (256, 512, 1024, 2048, 4096):
        ub_res = ubatch_major_prefill(ub)
        lm_res = layer_major_prefill(ub)
        print(f"{ub:>5} {'UB':>2} | {ub_res.total_ms:>10.0f}"
              f" {ub_res.prompt_tokens_per_s:>10.1f} |"
              f" {lm_res.total_ms:>11.0f} {lm_res.prompt_tokens_per_s:>7.1f}"
              f" {lm_res.steady_tokens_per_s:>13.1f} |"
              f" {'+' + format(ub_res.total_ms - lm_res.total_ms, '.0f'):>12}")
    print()
    print("Model note: ubatch-orthogonal, LM steady capture excludes fill;")
    print("LM total includes one fill/tail penalty (L + R + 2b cycles).")
    print("UB per-ubatch boundary is identical for every ubatch size because")
    print("the prompt streams the same bytes; only RTT counts differ.")
    print()


def print_conclusions() -> None:
    print("--- computed conclusions (model arithmetic only) -------------")
    step = strict_sync_ms()
    ptk_l, ptk_r = per_token_local_ms(), per_token_remote_ms()
    bnd = 2 * sp.boundary_ms(1)
    print(f"  1. Strict sync single-stream decode: {step:.2f} ms/token ="
          f" {1000.0 / step:.1f} tok/s; boundary RTT share"
          f" {100 * bnd / step:.1f}%.")
    print(f"  2. Batching shares one boundary RPC across B streams: gain"
          f" bounded by the RTT share -> at most ~{(1000.0 / (ptk_l + ptk_r) - 1000.0 / step) / (1000.0 / step) * 100:.0f}%"
          f" more tok/s.")
    roof = 1000.0 / max(ptk_l, ptk_r)
    print(f"  3. Two-stage pipelining (ideal) caps at the local device"
          f" roofline {roof:.1f} tok/s ({100 * (roof / (1000.0 / step) - 1):.0f}%"
          f" over MODE S)")
    print("     and requires >= 2 resident streams: VRAM B_max is 1 at")
    print("     128K, 2 at 64K, 4 at 32K (q4 KV, k=53 split, modelled).")
    ub = ubatch_major_prefill(512)
    lm = layer_major_prefill(512)
    print(f"  4. Prefill (512-token ubatches): UB {1000.0 * WORKLOAD.prompt_tokens / ub.total_ms:.1f} tok/s -> LM"
          f" {lm.prompt_tokens_per_s:.1f} tok/s prompt"
          f" ({100 * (1 - lm.total_ms / ub.total_ms):.0f}% faster) by hiding")
    print(f"     the remote tail; remaining serial floor is the local roofline"
          f" {lm.steady_tokens_per_s:.1f} tok/s steady.")
    print()


def main() -> None:
    print_assumptions()
    print_decode_model()
    print_decode_batches()
    print_prefill_model()
    print_conclusions()


if __name__ == "__main__":
    main()
