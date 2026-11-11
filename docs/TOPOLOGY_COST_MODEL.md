# Topology and benchmark cost model

This document defines what the planner is allowed to compare. Until a value is
measured on the target topology, it is unknown—not a guessed device score.

## Node and edge measurements

Record a stable paired node ID, OS/architecture, inference backend and version,
free usable RAM and VRAM after reservations, model shard and context-state
budgets, sustained per-stage decode compute, prefill throughput, thermal/power
state, and battery state where relevant. For each adjacent link record p50/p95
RTT and jitter, sustained bandwidth, loss/reconnect behavior, and actual
activation bytes per token. Capability snapshots are time-stamped and expire.

For stage i, a first-order decode estimate is:

    stage_i_us = measured_compute_i_us + measured_boundary_transfer_i_us
    decode_token_us = sum(stage_i_us) + synchronization_and_scheduler_us

For a pipelined multi-request workload, separately report pipeline throughput;
it is not equivalent to single-request token latency. Placement must obey
per-node memory constraints including weights, context-dependent KV/SSM,
workspace, runtime overhead, and a configurable safety reserve.

The planner minimizes the selected objective over feasible contiguous stages.
It must include a local-only candidate and never choose a discovered peer solely
because it exists. When a multi-node decode placement is slower or infeasible,
exclude that peer from decode or assign a non-critical role. Missing, stale, or
low-confidence telemetry triggers the deterministic local fallback.

## Reproducible benchmark matrix

For N=1, 2, 3, 4, and 7, run only where that many distinct, available, authorized
nodes exist. Publish unavailable configurations as unavailable; do not fill
them with simulator results. Record:

- model name and cryptographic digest, quantization, runtime/build revision;
- context, exact prompt digest, warmup count, generated token count, sampling;
- topology, OS/backend/driver, usable memory and each shard assignment;
- per-edge p50/p95 RTT, jitter, transfer+compute time, bytes/activation and
  total network bytes/token;
- per-node prefill and stage decode time, RSS/VRAM, temperature, utilization,
  power/battery where measurable;
- TTFT, prefill tok/s, sustained decode tok/s, per-token p50/p95 and tail stalls;
- reconnect/recovery outcomes, errors, and cache/residency state.

Compare same-model performance using identical model digest, precision,
context, prompt, sampling, warmup and decode length. Separately show larger-model
feasibility where single-node execution is impossible. Publish multiple runs,
warm and cold conditions, and dispersion. Label every result `measured`,
`simulated`, `predicted`, or `unknown`; only actual model inference is evidence
for sustained token throughput. The north-star threshold is >=10 tok/s after
warmup on an explicit configuration; 15 tok/s is aspirational.

## Initial profile schema

Profiles should be checked-in JSON fixtures for deterministic planner tests,
not claimed hardware measurements. A profile declares node capabilities,
time-stamped measurement provenance, link edges, model/context requirements,
memory reserves and objective. Test N=1/2/3/4/7, a 70B-class memory-fit
scenario only as declared arithmetic, unknown telemetry fallback, thermal
exclusion, and a slow Wi-Fi phone that is never forced into decode.