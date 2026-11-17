# FASTLINKv3 — Task Board (2026-11-08, EET)

Shared state: this file + FASTLINKv3-CHECKPOINT.md (both in qa-mission/), committed to git and mirrored in the Herdr session. File ownership is exclusive per task; no simultaneous conflicting edits.

## O-1 — OPENCODE (Core Dev): CPU-only Rust <-> <MODEL_ID> adapter integration
Status: ASSIGNED (OPENCODE idle/ready).
Branch: `feat/fastlink-v3-rpc-integration` (in <CHECKOUT_05>).
Objective: Prove the FASTLINK v0.1 Rust engine can drive ONE tensor chunk to the adapter over loopback, byte-exact, with NO GPU, NO remote host, and NO changes to running services (private ports omitted).
Scope / steps:
1. Baseline: confirm Rust repo builds/tests green; confirm CMake adapter builds and its CPU codec tests pass.
2. Add `src/rpc_client.rs` (new): reuse FASTLINK v0.1 content-addressed chunking (BLAKE3, 64KiB) + codec (RAW/RLE) to frame a tensor as a CHUNK payload.
3. Add `tests/rpc_integration.rs` (loopback): Rust sends one 64KiB tensor chunk to the <MODEL_ID> RPC server; server echoes/accepts; assert byte-exact round-trip.
4. Do NOT edit the adapter repository except for a minimal test harness; do NOT alter unrelated project files or running services.
Acceptance:
- `cargo build --release` ok; `cargo test --all-targets` passes (existing 47 + new test).
- New loopback integration test passes (byte-exact).
- CMake adapter build + its CPU codec tests pass.
- `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings` clean.
PR: open against ilievi187/FASTLINK branch `feat/fastlink-v3-rpc-integration`; CI must pass; merge only when green + in scope.
Checkpoint: update FASTLINKv3-CHECKPOINT.md, commit, push, report back.

## A-1 — ANTIGRAVITY (GPU & Perf): generic single-device benchmark harness.
Status: QUEUED (agent availability pending).
Branch: `feat/fastlink-v3-gpu-bench`.
Objective: Build a harness to measure VRAM, GPU utilization, network overhead, and tok/s, then run a single-device baseline on a generic host; identifying hardware/model details are omitted.
- Hardware/model identifiers and environment-specific deployment details are redacted.
- Any private tunnel endpoint or local path is represented by a generic placeholder.
Acceptance:
- Harness can sample VRAM + utilization on both GPUs (ROCm/rocprof or server-log based; document the method given no amdgpu_top/radeontop).
- Single-device baseline: tok/s, latency, VRAM, and utilization recorded with verified output.
- Results committed to the branch; checkpoint updated.
Constraints: Do NOT terminate any existing owner-managed model service or unrelated workload. The distributed part is blocked until capacity is explicitly authorized.

## A-2 — ANTIGRAVITY (after B1 cleared): 8K ctx distributed vs single-GPU benchmark
Status: BLOCKED (needs <GPU_MODEL> freed; B1).
Objective: Compare a model on two generic devices with a single-device run; measure throughput, latency, memory, utilization, and network overhead, and verify participation before increasing context.
Acceptance:
- Both GPUs verified participating (VRAM + utilization on each during the run).
- Correct, reproducible inference output.
- Single-GPU and distributed numbers recorded in a reproducible table; honest comparison (distributed not assumed to win).
- Relevant integration + regression tests pass; changes merged to main only when green + in scope.

## Handoff protocol
- Each agent writes its task results to qa-mission/ (or its branch) and updates FASTLINKv3-CHECKPOINT.md.
- The maintainer reviews the diff, runs tests, validates acceptance, and then manages the PR and CI lifecycle.
- On failure, assign repairs to the responsible agent; do not force-push or bypass checks.
