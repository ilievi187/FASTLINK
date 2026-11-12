"""FL3-C3: fault injection through the placement cost model (deterministic).

All numbers are MODEL outputs over the declared inputs in sim_common.py and
the FL3-C1 reference split (k=53 local Layers, q4 KV, LOCAL_DEDICATED,
boundary hop LINK_REMOTE). Nothing here is a measurement.

Simulated failure classes
-------------------------
- THROUGHPUT DEGRADATION: the boundary hop keeps its declared rtt, queue and
  serialization but its MiB/s is scaled by f in {1.0, 0.5, 0.1, 0.01}.
  Degradation is bandwidth-only by declaration; latency inflation is the
  wire-time term.
- TRANSIENT TRANSFER LOSS: per-RPC failure probability p, modelled with
  network_sim.transfer retry semantics: a failed RPC is retried up to
  ``budget`` times; when no retries remain the transfer fails terminally.
  Outcomes are drawn from one seeded RNG per scenario cell: every cell is
  byte-identical across runs.
- WORKER CRASH MID-PREFILL: the remote worker disappears after the remote
  device acknowledged ``c`` committed ubatches. The in-flight ubatch (local
  compute, forward boundary, remote compute) is declared fully lost.
  Recovery is compared: full redo (no resume) versus v3-style resume by
  manifest, where committed ubatches' KV is declared durable and only
  remaining/in-flight work is redone.
"""
from __future__ import annotations

import math
import random
from dataclasses import dataclass

import network_sim
import sim_placement as sp
from network_sim import STATUS_OK
from sim_common import (
    LINK_REMOTE,
    LOCAL_DEDICATED,
    REMOTE_DEVICE,
    WORKLOAD,
    Device,
)

REF_SPLIT_K_LOCAL = 53
MC_TRIALS = 2000
MC_SEED = 11

LOSS_GRID_P = (0.001, 0.01, 0.05)
BUDGET_GRID = (0, 1, 2, 4)
DEGRADATION_FACTORS = (1.0, 0.5, 0.1, 0.01)

# Declared operator-defined op classes for the timeout policy table.
OP_CLASSES = (
    ("weight_chunk_64KiB", WORKLOAD.hidden_mib(8)),   # 0.0625 MiB
    ("decode_boundary_8KiB", WORKLOAD.hidden_mib(1)),  # 0.0078125 MiB
    ("ubatch_boundary_4MiB", WORKLOAD.hidden_mib(WORKLOAD.ubatch_tokens)),
)
TIMEOUT_MULTIPLIER = 3.0
TIMEOUT_FLOOR_MS = 50.0
BACKOFF_SCHEDULE_MS = (100, 400, 1600)
MAX_RECONNECT_ATTEMPTS = 3


def _validate_device(device: object) -> Device:
    if not isinstance(device, Device):
        raise ValueError("device must be a Device instance")
    return device


def degraded_link(factor: float, loss_p: float = 0.0) -> network_sim.Link:
    if not math.isfinite(factor) or factor <= 0:
        raise ValueError("degradation factor must be a positive finite float")
    if not 0.0 <= loss_p <= 1.0:
        raise ValueError("loss_p must be within [0, 1]")
    return network_sim.Link(
        rtt_ms=LINK_REMOTE.rtt_ms,
        throughput_mib_s=LINK_REMOTE.throughput_mib_s * factor,
        serialization_ms=LINK_REMOTE.serialization_ms,
        queue_ms=LINK_REMOTE.queue_ms,
        loss_p=loss_p,
    )


# ---------------------------------------------------------------------------
# Core per-split measurements under degradation
# ---------------------------------------------------------------------------


def degraded_steady_state(ref: sp.MatrixRow, factor: float) -> dict:
    """Re-evaluate the reference split's steady state at a degraded link.

    Compute terms are unchanged (they are device-bound); the boundary term
    and the remote cold-load use the degraded link.
    """
    link = degraded_link(factor)
    boundary_per_direction = network_sim.transfer(
        link, WORKLOAD.hidden_mib(1), 1).latency_ms
    wire_boundary = 2 * (1000.0 * WORKLOAD.hidden_mib(1)
                         / link.throughput_mib_s)
    compute = ref.decode_step_ms - 2 * sp.boundary_ms(1)
    step = compute + 2 * boundary_per_direction
    remote_mib = ref.weights_remote_gib * 1024.0
    cold = network_sim.transfer(link, remote_mib, 1).latency_ms / 1000.0
    n_ub = math.ceil(WORKLOAD.prompt_tokens / WORKLOAD.ubatch_tokens)
    prefill = ref.prefill_ms - 2 * n_ub * sp.boundary_ms(
        WORKLOAD.ubatch_tokens)
    prefill += 2 * n_ub * network_sim.transfer(
        link, WORKLOAD.hidden_mib(WORKLOAD.ubatch_tokens), 1).latency_ms
    return {
        "decode_step_ms": step,
        "decode_tokens_per_s": 1000.0 / step if step > 0 else 0.0,
        "cold_load_remote_s": cold,
        "prefill_ms": prefill,
        "prefill_tokens_per_s": 1000.0 * WORKLOAD.prompt_tokens / prefill,
        "boundary_wire_ms": wire_boundary,
    }


@dataclass(frozen=True)
class DegradationRow:
    factor: float
    decode_step_ms: float
    decode_tokens_per_s: float
    cold_load_remote_s: float
    prefill_ms: float
    prefill_tokens_per_s: float
    boundary_wire_ms: float


def degradation_rows(ref: sp.MatrixRow) -> list:
    out = []
    for factor in DEGRADATION_FACTORS:
        steady = degraded_steady_state(ref, factor)
        out.append(DegradationRow(
            factor=factor,
            decode_step_ms=steady["decode_step_ms"],
            decode_tokens_per_s=steady["decode_tokens_per_s"],
            cold_load_remote_s=steady["cold_load_remote_s"],
            prefill_ms=steady["prefill_ms"],
            prefill_tokens_per_s=steady["prefill_tokens_per_s"],
            boundary_wire_ms=steady["boundary_wire_ms"],
        ))
    return out


# ---------------------------------------------------------------------------
# Transient loss + retry budget (seeded, deterministic Monte Carlo)
# ---------------------------------------------------------------------------


def loss_scenario(op_name: str, p_loss: float, budget: int,
                  trials: int = MC_TRIALS, seed: int = MC_SEED) -> dict:
    """Cell of the retry grid: ``op_name`` maps to a byte/RT op class."""
    op = dict(OP_CLASSES).get(op_name)
    if op is None:
        raise ValueError(f"unknown operation class: {op_name!r}")
    if not 0.0 <= p_loss <= 1.0:
        raise ValueError("p_loss must be within [0, 1]")
    if isinstance(budget, bool) or not isinstance(budget, int) or budget < 0:
        raise ValueError("budget must be a non-negative integer")
    if isinstance(trials, bool) or not isinstance(trials, int) or trials <= 0:
        raise ValueError("trials must be a positive integer")
    rng = random.Random(seed + budget * 7919
                        + int(round(p_loss * 1e9)))
    completions = 0
    attempts_total = 0
    latency_total = 0.0
    link = degraded_link(1.0, loss_p=p_loss)
    for _ in range(trials):
        result = network_sim.transfer(link, op, 1, budget, rng)
        if result.status == STATUS_OK:
            completions += 1
        attempts_total += result.attempts
        latency_total += result.latency_ms
    return {
        "op": op_name,
        "p_loss": p_loss,
        "budget": budget,
        "completion_rate": completions / trials,
        "mean_attempts": attempts_total / trials,
        "mean_latency_ms": latency_total / trials,
    }


# ---------------------------------------------------------------------------
# Worker crash mid-prefill + recovery policies
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class CrashScenario:
    committed_ubatches: int
    in_flight_lost_ms: float
    wasted_work_ms: float
    resume_recovery_ms: float
    full_redo_ms: float
    saving_ms: float
    saving_percent: float


def _ubatch_step_ms(link: network_sim.Link,
                    kv_label: str = "q4") -> tuple:
    """One strictly-serial ubatch step (compute + both boundaries)."""
    kv_bpl = sp.kv_bytes_per_token_per_layer(kv_label)
    local = (WORKLOAD.ubatch_tokens * REF_SPLIT_K_LOCAL
             * sp.layer_ms(LOCAL_DEDICATED, kv_bpl, 2))
    remote = (WORKLOAD.ubatch_tokens * (WORKLOAD.layers - REF_SPLIT_K_LOCAL)
              * sp.layer_ms(REMOTE_DEVICE, kv_bpl, 2))
    io_boundaries = 2 * network_sim.transfer(
        link, WORKLOAD.hidden_mib(WORKLOAD.ubatch_tokens), 1).latency_ms
    return local, remote, io_boundaries


def crash_scenarios(kv_label: str = "q4",
                    committed_values: tuple = (2, 4, 6)) -> list:
    """Crash after ``c`` committed ubatches; recovery comparison.

    Wasted work = the in-flight ubatch step is lost in full (declared),
    on top of the time already spent on the committed steps.
    Resume-by-manifest redo = reconnect (1 ack RT) + all ubatches from the
    failed one onward. No-resume redo restarts the whole prompt.
    """
    out = []
    n_ub = math.ceil(WORKLOAD.prompt_tokens / WORKLOAD.ubatch_tokens)
    for c in committed_values:
        if (isinstance(c, bool) or not isinstance(c, int)
                or not 0 < c < n_ub):
            raise ValueError(
                f"committed ubatches must be integers within (0, {n_ub})")
        link = degraded_link(1.0)
        local, remote, boundaries = _ubatch_step_ms(link, kv_label)
        step = local + remote + boundaries
        in_flight_lost = step
        wasted = c * step + in_flight_lost
        reconnect = network_sim.transfer(
            link, WORKLOAD.hidden_mib(1), 1).latency_ms
        resume = reconnect + (n_ub - c) * step
        full_redo = reconnect + n_ub * step
        saving = full_redo - resume
        out.append(CrashScenario(
            committed_ubatches=c,
            in_flight_lost_ms=in_flight_lost,
            wasted_work_ms=wasted,
            resume_recovery_ms=resume,
            full_redo_ms=full_redo,
            saving_ms=saving,
            saving_percent=100.0 * saving / full_redo,
        ))
    return out


# ---------------------------------------------------------------------------
# Recommended retry/timeout budget policy (computed, declared constants)
# ---------------------------------------------------------------------------


def attempt_timeout_ms(op_mib: float, factor: float) -> float:
    """Declared policy formula: max(floor, 3x) x (queue+rtt+ser+wire/f)."""
    link = degraded_link(factor)
    expected = (link.queue_ms + link.rtt_ms + link.serialization_ms
                + 1000.0 * op_mib / link.throughput_mib_s)
    return max(TIMEOUT_FLOOR_MS, TIMEOUT_MULTIPLIER * expected)


def policy_rows() -> list:
    rows = []
    for op_name, op_mib in OP_CLASSES:
        row = {"op": op_name, "mib": op_mib, "timeouts": {}}
        for factor in DEGRADATION_FACTORS:
            row["timeouts"][factor] = attempt_timeout_ms(op_mib, factor)
        rows.append(row)
    return rows


def recommended_policy_text() -> list:
    ub_mib = WORKLOAD.hidden_mib(WORKLOAD.ubatch_tokens)
    rows = policy_rows()
    by_name = {row["op"]: row for row in rows}
    ub = by_name["ubatch_boundary_4MiB"]
    worst_ub = ub["timeouts"][0.01]
    budget_total = (MAX_RECONNECT_ATTEMPTS
                    * (1 + len(BACKOFF_SCHEDULE_MS))) * worst_ub
    return [
        f"R1. Per-attempt timeout = max({TIMEOUT_FLOOR_MS} ms,"
        f" {TIMEOUT_MULTIPLIER:g} x (queue + rtt + serialization + wire))",
        f"    recomputed from the CURRENT link estimate; sample values:"
        f" ubatch transfer {worst_ub:.0f} ms at f=0.01 vs"
        f" {ub['timeouts'][1.0]:.0f} ms at f=1.0.",
        f"R2. Retries per transfer: 2 (3 attempts) for transient loss"
        f" p <= 0.05: >= {1 - 0.05 ** (2 + 1):.4f} completion per RPC"
        f" (binomial, modelled).",
        f"R3. Disconnect (worker crash): reconnect with backoff"
        f" {BACKOFF_SCHEDULE_MS} ms, max {MAX_RECONNECT_ATTEMPTS} attempts",
        "    then resume by manifest (delta): redo only non-acknowledged",
        "    ubatches; never restart the whole prompt after recoverable",
        "    loss (declaring committed KV durable). Modelled saving:",
        f"    {crash_saving_percent_range()}.",
        f"R4. Transfer budget cap: {budget_total / 1000.0:.1f} s total"
        f" per 4 MiB ubatch at f=0.01 worst case (3 reconnects x 2"
        f" retries x timeout).",
        "R5. Terminal failure (retry budget exhausted after reconnects):",
        "    report status, stop the step, do NOT commit partial output",
        "    (matches v0.1 no-partial-output rule), request operator",
        "    attention; recommended fallback placement from FL3-C1:",
        "    all-local at <= 32K ctx q4 KV as the degradation-free point.",
    ]


def crash_saving_percent_range() -> str:
    scenarios = crash_scenarios()
    percents = [s.saving_percent for s in scenarios]
    return f"{min(percents):.2f}..{max(percents):.2f}%"


# ---------------------------------------------------------------------------
# Output
# ---------------------------------------------------------------------------


def reference_setup() -> sp.MatrixRow:
    return sp.compute_row(REF_SPLIT_K_LOCAL, 131072, "q4", LOCAL_DEDICATED)


def print_assumptions(ref: sp.MatrixRow) -> None:
    print("=== FL3-C3: fault injection through the cost model (SIMULATED) ===")
    print("All values are MODEL outputs. Reference split (FL3-C1):")
    print(f"  {REF_SPLIT_K_LOCAL}/{WORKLOAD.layers - REF_SPLIT_K_LOCAL} layers"
          f" local/remote, q4 KV, LOCAL_DEDICATED; nominal boundary hop:")
    print(f"  rtt {LINK_REMOTE.rtt_ms} ms, {LINK_REMOTE.throughput_mib_s}"
          f" MiB/s, ser {LINK_REMOTE.serialization_ms} ms,"
          f" queue {LINK_REMOTE.queue_ms} ms (declared).")
    print(f"  Baseline model: decode {ref.decode_step_ms:.2f} ms/token"
          f" ({1000.0 / ref.decode_step_ms:.1f} tok/s),"
          f" prefill {ref.prefill_ms:.0f} ms for {WORKLOAD.prompt_tokens}"
          f" tokens, cold load {ref.cold_load_remote_s:.1f} s.")
    print(f"  Monte Carlo: {MC_TRIALS} trials per cell, seed {MC_SEED}"
          f" (loss cells are byte-identical across runs).")
    print()


def print_degradation(ref: sp.MatrixRow) -> None:
    print("--- throughput degradation of the boundary hop --------------")
    header = (f"{'f':>5} | {'step ms':>8} {'tok/s':>6} | {'cold s':>8}"
              f" {'cold min':>8} | {'prefill ms':>10} {'t/s':>6} |"
              f" {'wire ms':>8}")
    print(header)
    print("-" * (len(header) + 2))
    for row in degradation_rows(ref):
        print(f"{row.factor:>5} | {row.decode_step_ms:>8.2f}"
              f" {row.decode_tokens_per_s:>6.1f} |"
              f" {row.cold_load_remote_s:>8.1f}"
              f" {row.cold_load_remote_s / 60.0:>8.1f} |"
              f" {row.prefill_ms:>10.0f} {row.prefill_tokens_per_s:>6.1f} |"
              f" {row.boundary_wire_ms:>8.3f}")
    print("  (wire ms = one-direction 8 KiB wire term; degradation is")
    print("   bandwidth-only by declaration: rtt/queue/ser unchanged.)")
    print()


def print_retry_grid() -> None:
    print("--- transient loss retry grid (2000 trials/cell, seeded) ----")
    header = (f"{'op':>22} {'p':>6} |" + "".join(
        f"{'b=' + str(b) + ' done%':>10}" for b in BUDGET_GRID)
        + " |" + f"{'E[try]@b0':>10}")
    print(header)
    print("-" * (len(header) + 2))
    for op_name, op_mib in OP_CLASSES:
        for p in LOSS_GRID_P:
            cells = []
            attempts_first = None
            for budget in BUDGET_GRID:
                scenario = loss_scenario(op_name, p, budget)
                cells.append(100.0 * scenario["completion_rate"])
                if budget == BUDGET_GRID[0]:
                    attempts_first = scenario
            tail = f" | {attempts_first['mean_attempts']:>10.2f}"
            print(f"{op_name:>22} {p:>6} |" + "".join(
                f"{cell:>10.2f}" for cell in cells) + tail)
    print("  (done% = completion rate; E[try] shown for budget 0.)")
    print()


def print_crash_scenarios() -> None:
    print("--- worker crash mid-prefill (4096 tok, 8 ubatches) ----------")
    header = (f"{'committed':>9} | {'wasted ms':>10} | {'resume ms':>10}"
              f" {'full ms':>10} {'saving ms':>10} {'saving %':>9}")
    print(header)
    print("-" * (len(header) + 2))
    for case in crash_scenarios():
        print(f"{case.committed_ubatches:>9} | {case.wasted_work_ms:>10.0f}"
              f" | {case.resume_recovery_ms:>10.0f} {case.full_redo_ms:>10.0f}"
              f" {case.saving_ms:>10.0f} {case.saving_percent:>9.2f}")
    print("  (wasted = committed steps + in-flight ubatch lost in full;")
    print("   resume = reconnect + unfinished ubatches; full = restart.)")
    print()


def print_policy_table() -> None:
    print("--- recommended retry / timeout budget policy (computed) -----")
    header = f"{'op':>22} {'MiB':>8} | " + "".join(
        f"{'f=' + str(int(f * 100)) + '%':>9}" for f in DEGRADATION_FACTORS)
    print(header)
    print("-" * (len(header) + 2))
    for row in policy_rows():
        print(f"{row['op']:>22} {row['mib']:>8.4f} | " + "".join(
            f"{row['timeouts'][f]:>9.0f}" for f in DEGRADATION_FACTORS))
    print("  (attempt timeout in ms; retries=2; floor 50 ms; backoff")
    print(f"   {BACKOFF_SCHEDULE_MS} ms reconnect; max"
          f" {MAX_RECONNECT_ATTEMPTS} reconnects then terminal.)")
    print()
    for line in recommended_policy_text():
        print(line)
    print()


def main() -> None:
    ref = reference_setup()
    print_assumptions(ref)
    print_degradation(ref)
    print_retry_grid()
    print_crash_scenarios()
    print_policy_table()


if __name__ == "__main__":
    main()
