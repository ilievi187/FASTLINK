# FASTLINKv3 — Task Board (2026-11-08, EET)

Shared state: this file + FASTLINKv3-CHECKPOINT.md (both in qa-mission/), committed to git and mirrored in the Herdr session. File ownership is exclusive per task; no simultaneous conflicting edits.

## O-1 — OPENCODE (Core Dev): CPU-only Rust <-> llama.cpp adapter integration
Status: ASSIGNED (OPENCODE idle/ready).
Branch: `feat/fastlink-v3-rpc-integration` (in /home/nightknight/Projects/FASTLINK).
Objective: Prove the FASTLINK v0.1 Rust engine can drive ONE tensor chunk to the llama.cpp FASTLINK-RPC adapter over loopback, byte-exact, with NO GPU, NO detti, NO changes to running servers (ports 8131, 18927).
Scope / steps:
1. Baseline: confirm Rust repo builds/tests green; confirm CMake adapter builds and its CPU codec tests pass.
2. Add `src/rpc_client.rs` (new): reuse FASTLINK v0.1 content-addressed chunking (BLAKE3, 64KiB) + codec (RAW/RLE) to frame a tensor as a CHUNK payload.
3. Add `tests/rpc_integration.rs` (loopback): Rust sends one 64KiB tensor chunk to the llama.cpp RPC server; server echoes/accepts; assert byte-exact round-trip.
4. Do NOT edit the llama.cpp repo (read-only) except a minimal test harness if needed; do NOT touch CODEXBG files, Heretic, or the two running llama servers.
Acceptance:
- `cargo build --release` ok; `cargo test --all-targets` passes (existing 47 + new test).
- New loopback integration test passes (byte-exact).
- CMake adapter build + its CPU codec tests pass.
- `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings` clean.
PR: open against ilievi187/FASTLINK branch `feat/fastlink-v3-rpc-integration`; CI must pass; merge only when green + in scope.
Checkpoint: update FASTLINKv3-CHECKPOINT.md, commit, push, report back.

## A-1 — ANTIGRAVITY (GPU & Perf): GPU bench harness + detti single-GPU baseline
Status: QUEUED (Antigravity busy with a YouTube/CODEXBG task; pick up after it reports READY).
Branch: `feat/fastlink-v3-gpu-bench`.
Objective: Build a harness to measure VRAM, GPU utilization, network overhead, and tok/s, then run the Qwen3.5-4B-3D-Coder single-GPU baseline on detti (RX 6600 XT) at 8K context.
Model: /mnt/gamez/AI/model-tests/qwen3d-sensenova/Qwen3.5-4B-3D-Coder-Q4_K_M.gguf (Vulkan build at .../llama-sensenova/build-vulkan).
Infra: /home/nightknight/Projects/NightKnight-Distributed-AI (detty via SSH tunnel 127.0.0.1:15005 -> 127.0.0.1:50052).
Acceptance:
- Harness can sample VRAM + utilization on both GPUs (ROCm/rocprof or server-log based; document the method given no amdgpu_top/radeontop).
- detti single-GPU 8K ctx run: tok/s, latency, VRAM, GPU util recorded with byte-exact / verified output.
- Results committed to the branch; checkpoint updated.
Constraints: Do NOT terminate Saluki's 27B server (port 18927) or any unrelated workload. The distributed (7900 XT) part is BLOCKED until the 7900 XT is freed (B1).

## A-2 — ANTIGRAVITY (after B1 cleared): 8K ctx distributed vs single-GPU benchmark
Status: BLOCKED (needs 7900 XT freed; B1).
Objective: Run Qwen3.5-4B-3D-Coder on (RX 7900 XT + RX 6600 XT) vs (single GPU), 8K context; measure tok/s, latency, VRAM, GPU util, network overhead; verify both GPUs demonstrably participate in one workload; then increase context only after stability + memory are verified.
Acceptance:
- Both GPUs verified participating (VRAM + utilization on each during the run).
- Correct, reproducible inference output.
- Single-GPU and distributed numbers recorded in a reproducible table; honest comparison (distributed not assumed to win).
- Relevant integration + regression tests pass; changes merged to main only when green + in scope.

## Handoff protocol
- Each agent writes its task results to qa-mission/ (or its branch) and updates FASTLINKv3-CHECKPOINT.md.
- Saluki reviews the diff, runs the required tests, validates acceptance criteria, then commits/pushes/opens PRs and manages CI + merge.
- On failure: Saluki assigns the repair to the responsible agent; no force-push, no bypassing checks.
