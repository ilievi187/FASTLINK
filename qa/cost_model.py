"""Explicit-parameter CPU model for a serial remote inference pipeline."""
from dataclasses import dataclass
import math


@dataclass(frozen=True)
class Link:
    rtt_ms: float
    throughput_mib_s: float
    serialization_ms: float = 0.0

    def __post_init__(self) -> None:
        if not all(math.isfinite(value) for value in
                   (self.rtt_ms, self.throughput_mib_s, self.serialization_ms)):
            raise ValueError("link parameters must be finite")
        if self.rtt_ms < 0 or self.serialization_ms < 0:
            raise ValueError("latencies must be non-negative")
        if self.throughput_mib_s <= 0:
            raise ValueError("throughput_mib_s must be positive")

    def transfer_ms(self, mib: float, round_trips: int = 1) -> float:
        if not isinstance(round_trips, int) or isinstance(round_trips, bool):
            raise ValueError("round_trips must be an integer")
        if mib < 0 or round_trips < 0 or not math.isfinite(mib):
            raise ValueError("size and round_trips must be finite and non-negative")
        if mib == 0 and round_trips == 0:
            return 0.0
        serialized_messages = max(1, round_trips)
        return (round_trips * self.rtt_ms +
                1000.0 * mib / self.throughput_mib_s +
                serialized_messages * self.serialization_ms)


@dataclass(frozen=True)
class Stage:
    name: str
    compute_ms: float
    outbound_mib: float = 0.0
    rpc_round_trips: int = 0
    queue_ms: float = 0.0

    def __post_init__(self) -> None:
        if not all(math.isfinite(value) for value in
                   (self.compute_ms, self.outbound_mib, self.queue_ms)):
            raise ValueError("stage values must be finite")
        if not self.name:
            raise ValueError("stage name is required")
        if min(self.compute_ms, self.outbound_mib, self.queue_ms) < 0:
            raise ValueError("stage costs must be non-negative")
        if not isinstance(self.rpc_round_trips, int) or isinstance(self.rpc_round_trips, bool):
            raise ValueError("rpc_round_trips must be an integer")
        if self.rpc_round_trips < 0:
            raise ValueError("rpc_round_trips must be non-negative")


def pipeline_latency_ms(stages: list[Stage], link: Link) -> float:
    """Sum supplied serial costs; overlap and batching are not modeled."""
    if not stages:
        raise ValueError("at least one stage is required")
    return sum(stage.compute_ms + stage.queue_ms +
               link.transfer_ms(stage.outbound_mib, stage.rpc_round_trips)
               for stage in stages)


def decode_tokens_per_second(stages: list[Stage], link: Link) -> float:
    """Calculate modelled single-token rate, not observed hardware performance."""
    total_ms = pipeline_latency_ms(stages, link)
    return 0.0 if total_ms == 0 else 1000.0 / total_ms


def placement_is_feasible(model_mib: float, kv_mib: float, reserve_mib: float,
                          free_vram_mib: float) -> bool:
    """Check caller-supplied memory declarations; performs no device discovery."""
    values = (model_mib, kv_mib, reserve_mib, free_vram_mib)
    if not all(math.isfinite(value) for value in values):
        raise ValueError("memory values must be finite")
    if min(values) < 0:
        raise ValueError("memory values must be non-negative")
    return model_mib + kv_mib + reserve_mib <= free_vram_mib
