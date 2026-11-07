"""Deterministic CPU-only network simulator for FASTLINK v3 planning.

This is a **planning model**, not a measurement tool: every number produced
here is *modelled* arithmetic over declared link and stage parameters. No
number in this module is observed GPU telemetry, real benchmark output, or a
measured hardware rate, and none may be quoted as such.

Model semantics (locked by qa/test_network_sim.py):

- The pipeline is strictly serial: stages run in list order, overlap and
  batching are not modelled, and a failed transfer prevents later stages
  from charging any latency.
- Each RPC round trip is one attempt: it costs ``rtt_ms + serialization_ms``
  and moves exactly its wire share,
  ``1000 * (outbound_mib / round_trips) / throughput_mib_s`` milliseconds.
  Serialization is therefore charged per round trip, not once per transfer.
- A stage with no outbound traffic and no round trips charges only its
  ``compute_ms``: no queueing delay applies to a pure compute stage.
- ``queue_ms`` is a fixed upstream queueing delay charged exactly once per
  transfer, before the first attempt, even if that transfer then exhausts
  its retry budget or disconnects.
- ``retry_budget`` bounds total retry attempts for the whole transfer
  (initial attempts <= round_trips; retries <= retry_budget, so
  attempts <= round_trips + retry_budget). A failed attempt when no retries
  remain is terminal: status ``retry_budget_exhausted``.
- ``loss_p`` is the per-attempt failure probability. ``loss_p == 0.0``
  never consults the RNG; ``loss_p == 1.0`` always fails without RNG;
  in between, a seeded ``random.Random`` decides per attempt, so identical
  seeds reproduce identical outcomes.
- ``disconnect_after_rts`` (optional) aborts the link once that many round
  trips have *succeeded*; the remaining round trips are never attempted and
  the status becomes ``disconnected``.
- ``mib`` must ride at least one round trip: ``mib > 0`` with
  ``round_trips == 0`` is rejected. Ack-only round trips may set ``mib == 0``.
"""
from __future__ import annotations

import math
import random
from dataclasses import dataclass
from typing import Optional

STATUS_OK = "ok"
STATUS_DISCONNECTED = "disconnected"
STATUS_BUDGET_EXHAUSTED = "retry_budget_exhausted"


def _finite_non_negative(value: object, label: str) -> float:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise ValueError(f"{label} must be a number")
    as_float = float(value)
    if not math.isfinite(as_float):
        raise ValueError(f"{label} must be finite")
    if as_float < 0:
        raise ValueError(f"{label} must be non-negative")
    return as_float


def _finite_positive(value: object, label: str) -> float:
    as_float = _finite_non_negative(value, label)
    if as_float == 0:
        raise ValueError(f"{label} must be positive")
    return as_float


def _bounded_probability(value: object, label: str) -> float:
    probability = _finite_non_negative(value, label)
    if probability > 1:
        raise ValueError(f"{label} must be within [0, 1]")
    return probability


def _non_negative_int(value: object, label: str, minimum: int = 0) -> int:
    if isinstance(value, bool) or not isinstance(value, int):
        raise ValueError(f"{label} must be an integer")
    if value < minimum:
        raise ValueError(f"{label} must be >= {minimum}")
    return value


@dataclass(frozen=True)
class TransferResult:
    """Modelled outcome of one transfer across the simulated hop.

    ``latency_ms`` is a model estimate; ``attempts`` counts round trips
    actually attempted (successful or failed); ``failures`` counts failed
    attempts. No field is an observed measurement.
    """

    status: str
    latency_ms: float
    attempts: int
    failures: int


@dataclass(frozen=True)
class Link:
    """Declared hop parameters: model inputs, never measured values."""

    rtt_ms: float
    throughput_mib_s: float
    serialization_ms: float = 0.0
    queue_ms: float = 0.0
    loss_p: float = 0.0
    disconnect_after_rts: Optional[int] = None

    def __post_init__(self) -> None:
        _finite_non_negative(self.rtt_ms, "rtt_ms")
        _finite_positive(self.throughput_mib_s, "throughput_mib_s")
        _finite_non_negative(self.serialization_ms, "serialization_ms")
        _finite_non_negative(self.queue_ms, "queue_ms")
        _bounded_probability(self.loss_p, "loss_p")
        if self.disconnect_after_rts is not None:
            _non_negative_int(self.disconnect_after_rts, "disconnect_after_rts", 1)


def transfer(
    link: Link,
    mib: float,
    round_trips: int,
    retry_budget: int = 0,
    rng: Optional[random.Random] = None,
) -> TransferResult:
    """Modelled serial transfer latency in milliseconds over ``link``.

    Every value returned is model arithmetic per the module docstring; it is
    not an observed latency, probe result, or GPU telemetry.
    """
    _finite_non_negative(mib, "mib")
    _non_negative_int(round_trips, "round_trips")
    _non_negative_int(retry_budget, "retry_budget")
    if mib > 0 and round_trips == 0:
        raise ValueError("mib must ride at least one round trip")
    if mib == 0 and round_trips == 0:
        return TransferResult(STATUS_OK, 0.0, 0, 0)
    if rng is None:
        rng = random.Random(0)

    wire_share_ms = 1000.0 * (mib / round_trips) / link.throughput_mib_s
    attempts = 0
    failures = 0
    successful = 0
    retries_used = 0
    latency = link.queue_ms
    for _required_round_trip in range(round_trips):
        if (
            link.disconnect_after_rts is not None
            and successful >= link.disconnect_after_rts
        ):
            return TransferResult(
                STATUS_DISCONNECTED, latency, attempts, failures
            )
        while True:
            attempts += 1
            latency += link.rtt_ms + link.serialization_ms + wire_share_ms
            if link.loss_p == 0.0:
                failed = False
            elif link.loss_p == 1.0:
                failed = True
            else:
                failed = rng.random() < link.loss_p
            if not failed:
                successful += 1
                break
            failures += 1
            if retries_used >= retry_budget:
                return TransferResult(
                    STATUS_BUDGET_EXHAUSTED, latency, attempts, failures
                )
            retries_used += 1
    return TransferResult(STATUS_OK, latency, attempts, failures)


@dataclass(frozen=True)
class Stage:
    """One serial pipeline stage; all fields are modelled declarations."""

    name: str
    compute_ms: float
    outbound_mib: float = 0.0
    rpc_round_trips: int = 0

    def __post_init__(self) -> None:
        if not isinstance(self.name, str) or not self.name:
            raise ValueError("stage name must be a non-empty string")
        _finite_non_negative(self.compute_ms, "compute_ms")
        _finite_non_negative(self.outbound_mib, "outbound_mib")
        _non_negative_int(self.rpc_round_trips, "rpc_round_trips")


@dataclass(frozen=True)
class PipelineResult:
    """Modelled serial pipeline outcome: every field is a model estimate.

    ``decode_tokens_per_second`` is the modelled single-token rate
    ``1000 / latency_ms`` (defined only when the pipeline status is ``ok``);
    it is not observed hardware performance.
    """

    status: str
    latency_ms: float
    stage_results: tuple
    decode_tokens_per_second: Optional[float]


def simulate_pipeline(
    stages: list[Stage],
    link: Link,
    retry_budget: int = 0,
    seed: int = 0,
) -> PipelineResult:
    """Sum declared serial costs across ordered stages over one ``link``.

    A stage whose transfer fails (retry budget exhausted or disconnect)
    stops the pipeline: later stages charge nothing. All returned numbers
    are modelled per the module docstring.
    """
    _non_negative_int(retry_budget, "retry_budget")
    stage_list = list(stages)
    if not stage_list:
        raise ValueError("at least one stage is required")
    total_ms = 0.0
    results = []
    status = STATUS_OK
    rng = random.Random(seed)
    for stage in stage_list:
        if not isinstance(stage, Stage):
            raise ValueError("pipeline stages must be Stage instances")
        total_ms += stage.compute_ms
        result = transfer(
            link,
            stage.outbound_mib,
            stage.rpc_round_trips,
            retry_budget,
            rng,
        )
        total_ms += result.latency_ms
        results.append((stage.name, result))
        if result.status != STATUS_OK:
            status = result.status
            break
    decode = (
        1000.0 / total_ms if status == STATUS_OK and total_ms > 0 else None
    )
    return PipelineResult(status, total_ms, tuple(results), decode)
