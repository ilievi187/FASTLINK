# FASTLINK multi-device north star

FASTLINK aims to run one model larger than any participant's usable memory by
placing model stages and their mutable inference state across explicitly
opted-in heterogeneous nodes. Linux/Windows PCs and servers come first; mobile
devices may later contribute only roles that fit their foreground, thermal,
power, and operating-system constraints.

## Non-negotiable properties

- Discovery advertises capability; it does not grant participation. Pairing and
  per-session opt-in are explicit. A node may withdraw and must be evacuated
  safely.
- A coordinator exchanges versioned manifests and placement plans. Activation
  payloads flow directly between adjacent stages where the authenticated
  topology permits; do not hairpin data through the coordinator by default.
- Keep weights and per-stage KV/SSM state resident. Never retransmit full model
  weights or the full state for each token. Version state and validate digests.
- Assign contiguous layer stages to minimize the decode critical path while
  satisfying usable RAM/VRAM, context-dependent state, thermal limits, link
  bandwidth, p95 latency, and measured stage compute. Use the minimum node count
  that fits the objective. If a candidate hurts the critical path, leave it out
  of decode (or give it prefill, draft, or staging work).
- Support explicit objectives: largest feasible model, lowest latency for a
  fixed model, or aggregate multi-user throughput. Do not present batching
  throughput as single-stream decode speed.
- Authenticate paired peers and encrypt transport before non-loopback use.
  Never expose unauthenticated RPC publicly or send prompts, weights, or state
  to an untrusted peer. Work devices require owner/employer authorization.
- Join, leave, restart, and reshard only through versioned, integrity-checked
  plans and safe boundaries; invalidate stale state and recover deterministically.

## Staged delivery

1. CPU-only protocol and deterministic planner; local/loopback tests.
2. Linux and Windows workers with capability reporting, secure pairing, bounded
   RPC, persistent residency, and failure recovery.
3. A real three-node proof using NightKnightPC, Detti, and the Ubuntu server,
   only after availability and workload permission are confirmed. Report both
   same-model speed and larger-model feasibility.
4. Additional laptops and measured 4-/7-node profiles.
5. Mobile companion/storage/draft roles; native foreground compute only after
   platform-specific implementation and measurement.

The 10–15 sustained decode tokens/s target is an aspiration. It is not a claim
until reproduced after warmup on a precisely identified model digest,
quantization, context, prompt, decode length, software revision, and hardware.