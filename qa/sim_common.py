"""Shared DECLARED model inputs for FL3-C1..C3 deterministic simulations.

Every constant below is a declared planning input, chosen and documented for
a necessity to keep the simulations reproducible and CPU-only. None of these
numbers is an observed measurement, GPU probe, or benchmark result. Deviations
from declared reality (driver overhead, kernel scheduler, network stack,
Windows reservations) are outside the model's possible range by design.

Hardware identity is public product spec for the JSON protocol: nodes are
declared as generic capacity/bandwidth figures, no private host addresses are
encoded anywhere in this module.
"""
from __future__ import annotations

from dataclasses import dataclass

from network_sim import Link


# ---------------------------------------------------------------------------
# Workload: "27B dense Q4_K_M at 131072-token context"
# ---------------------------------------------------------------------------

KV_LABELS: dict[str, float] = {
    "fp16": 1.0,  # KV cache stored at fp16: 2 K + 2 V bytes/elem/layer
    "q8": 0.5,    # declared approx. 8-bit KV scale (1/2 of fp16)
    "q4": 0.25,   # declared approx. 4-bit KV scale (1/4 of fp16)
}


@dataclass(frozen=True)
class Workload:
    name: str
    params_b: float            # billions of parameters (metric, 1e9)
    bits_per_weight: float     # declared average quantized bits per weight
    layers: int                # decoder layer count (block/layer boundary unit)
    hidden_dim: int            # residual-stream dimension (bf16 activations)
    activation_bytes: int      # bytes per hidden element across the split
    ctx_tokens: int            # context (KV) length for placement analysis
    prompt_tokens: int         # prefill workload analysed end-to-end
    ubatch_tokens: int         # physical ubatch: prefill RPC granularity

    @property
    def weights_gib(self) -> float:
        """Declared quantized weight bytes converted to GiB."""
        return self.params_b * 1e9 * self.bits_per_weight / 8.0 / 2**30

    @property
    def layer_weights_gib(self) -> float:
        return self.weights_gib / self.layers

    def hidden_mib(self, tokens: int) -> float:
        """Residual-stream bytes carried once per boundary, in MiB."""
        if tokens < 0:
            raise ValueError("tokens must be non-negative")
        return tokens * self.hidden_dim * self.activation_bytes / 2**20


WORKLOAD = Workload(
    name="27B dense Q4_K_M-like, 131072-token ctx",
    params_b=27.0,
    bits_per_weight=4.7,       # declared Q4_K_M average bits/weight
    layers=64,
    hidden_dim=4096,
    activation_bytes=2,        # bf16 boundary stream
    ctx_tokens=131072,
    prompt_tokens=4096,
    ubatch_tokens=512,
)

# KV cache bytes per token, per layer, at fp16 (GQA: 2 * n_kv_head * head_dim).
KV_BYTES_PER_TOKEN_PER_LAYER_FP16 = 4096


def kv_bytes_per_token_per_layer(kv_label: str) -> int:
    """KV bytes per token, per layer, at the declared KV mode."""
    if kv_label not in KV_LABELS:
        raise ValueError(f"unknown KV mode: {kv_label!r}")
    return int(KV_BYTES_PER_TOKEN_PER_LAYER_FP16 * KV_LABELS[kv_label])


# ---------------------------------------------------------------------------
# Devices: declared VRAM capacity/occupancy and declared bandwidth figures.
# Occupancy is a declared planning input ("what fits beside the existing
# workload"), NOT probed VRAM free values from the runtime hosts.
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class Device:
    name: str
    total_vram_gib: float
    occupied_gib: float        # declared memory not available to a v3 worker
    bandwidth_gib_s: float     # declared device bandwidth for roofline reads
    workspace_reserve_gib: float   # declared transient activation/workspace

    @property
    def usable_vram_gib(self) -> float:
        return self.total_vram_gib - self.occupied_gib

    @property
    def budget_gib(self) -> float:
        """Usable VRAM minus permanent workspace reserve."""
        return self.usable_vram_gib - self.workspace_reserve_gib

    def compute_ms_per_gib(self) -> float:
        """Declared roofline: milliseconds to read 1 GiB from VRAM."""
        return 1000.0 / self.bandwidth_gib_s


LOCAL_MIXED = Device(
    name="LOCAL_MIXED: RX 7900 XT 20 GiB (existing 27B service resident)",
    total_vram_gib=20.0,
    occupied_gib=6.0,          # declared: OS/DE + resident llama.cpp service
    bandwidth_gib_s=800.0,
    workspace_reserve_gib=0.5,
)
LOCAL_DEDICATED = Device(
    name="LOCAL_DEDICATED: RX 7900 XT 20 GiB (OS reserve only)",
    total_vram_gib=20.0,
    occupied_gib=0.5,          # declared: OS/session tail only
    bandwidth_gib_s=800.0,
    workspace_reserve_gib=0.5,
)
REMOTE_DEVICE = Device(
    name="REMOTE: RX 6600 XT 8 GiB",
    total_vram_gib=8.0,
    occupied_gib=1.0,          # declared: Windows desktop + driver
    bandwidth_gib_s=256.0,
    workspace_reserve_gib=1.0,
)


# ---------------------------------------------------------------------------
# Links. throughput = line-rate * declared 0.8 protocol efficiency.
# These are declared planning hops, no physical hosts are encoded here.
# ---------------------------------------------------------------------------

LINK_REMOTE = Link(
    rtt_ms=1.0,
    throughput_mib_s=round(1000e6 / 8 / 2**20 * 0.8, 4),   # ~95.3674 MiB/s
    serialization_ms=0.05,
    queue_ms=0.2,
)
LINK_LOCAL = Link(
    rtt_ms=0.5,
    throughput_mib_s=round(2500e6 / 8 / 2**20 * 0.8, 4),   # ~238.4186 MiB/s
    serialization_ms=0.05,
    queue_ms=0.1,
)


def kv_gib_total(ctx_tokens: int, kv_label: str) -> float:
    """Whole-model KV GiB at ``ctx_tokens`` for the declared KV mode."""
    if ctx_tokens < 0:
        raise ValueError("ctx_tokens must be non-negative")
    if kv_label not in KV_LABELS:
        raise ValueError(f"unknown KV mode: {kv_label!r}")
    return ctx_tokens / 2**30 * (
        KV_BYTES_PER_TOKEN_PER_LAYER_FP16 * KV_LABELS[kv_label] * WORKLOAD.layers
    )
