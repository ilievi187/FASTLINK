# FASTLINKv3 — Mission Checkpoint (2026-11-08, EET)

Owner: SALUKI (hermes, Fastlinkv3 workspace w8). Orchestrating OPENCODE + ANTIGRAVITY via Herdr.
Repository: github.com/ilievi187/FASTLINK (Rust v0.1 engine) + /mnt/gamez/AI/llama.cpp-fastlink-rpc-adapter (C++ llama.cpp fork).

## Latest verified state (last known-good)

Rust engine (FASTLINK v0.1 — CPU delta-sync, 64KiB BLAKE3 chunks, RAW/RLE codec):
- `agent/heretic-m1-hardening` HEAD 074b248 — memory-budget cache (cache_budget.rs) + placement scheduler (scheduler.rs, WP-C formulas).
- `qa/fastlink-v3-mission-qa` HEAD d121521 — FL3-E5 Step 3 Run 4 (all 4 cross-branch checkouts PASS: fmt, clippy, cargo test, Python sim 57/0).
- 47 Rust tests pass (33 lib + 14 integration); cross-branch regression diffs (FL3-E3) show 0 regressions.
- FL3-D1..D5: fault harness (10/10), bounded parsing + crash recovery, Windows GNU target (42 lib + 15 int under Wine), chunk-frame write_vectored microbench.
- Cross-machine validation (2026-11-06): Linux→detti (Windows) 16 MiB cold 1.8582s / warm 0.1464s / delta 0.1778s, sha256-verified, SSH tunnel, both endpoints loopback.

llama.cpp RPC adapter (C++ fork):
- `feat/fastlink-rpc-adapter` HEAD 7951636 — FL4-INT (Antigravity): persistent tensor residency in RPC backend (warm reuse, byte-exact, measured reduction).
- FLR3 v3 codec selection metadata (RAW/RLE/LZ4/ZSTD) + CPU round-trip tests; --chunk-sweep (512B..256K) + 5-strategy comparison.
- 1 MiB FLR1/FLR3 overhead profile + CPU codec benchmark; fl3e1_adapter_check.sh + qa/adapter-matrix.md.
- 128K Vulkan inference + JCode verification recorded; opt-in FASTLINK-RPC tensor codec with Vulkan QA.
- NOTE: the adapter borrows FASTLINK's chunk/codec concept but is NOT yet wired to the Rust v0.1 repo (core M3 gap).

Real distributed GPU inference (PRIMARY EXPERIMENT — NOT done):
- Target: Qwen3.5-4B-3D-Coder Q4_K_M across NightKnightPC (RX 7900 XT 20GB) + detti (RX 6600 XT 8GB).
- Model present: /mnt/gamez/AI/model-tests/qwen3d-sensenova/Qwen3.5-4B-3D-Coder-Q4_K_M.gguf (2.6GB) + mmproj-Qwen3.5-4B-BF16.gguf (645MB).
- Vulkan build: /mnt/gamez/AI/model-tests/qwen3d-sensenova/llama-sensenova/build-vulkan/bin/.
- Existing dual-GPU cluster: /home/nightknight/Projects/NightKnight-Distributed-AI (STATUS: WORKING, GPU inference not started; 15005 tunnel down).

## Blockers
- B1 (hard): RX 7900 XT is occupied by Saluki's 27B "Underdog-Saluki" server (port 18927, ctx 128K, 99 GPU layers). The distributed (7900 XT) benchmark is BLOCKED until the owner frees the 7900 XT. DO NOT stop that server.
- B2: No amdgpu_top/radeontop for live GPU-utilization measurement (harness gap — Antigravity must build a measurement path via ROCm/rocprof or server logs).
- B3: ANTIGRAVITY (argon) is busy with an unrelated YouTube/CODEXBG task. FASTLINK task A-1 is queued for after it reports READY.
- B4: OPENCODE + Antigravity currently share the "Youtube Production CodexBG" Herdr workspace; FASTLINK coordination lives in Fastlinkv3 (w8). Tasks reference absolute paths so workspace is irrelevant.

## Team (Herdr)
- SALUKI (hermes) — orchestrator / lead architect (w8 Fastlinkv3).
- OPENCODE (opencode-codexbg, w9:t3) — Core Dev — IDLE, ready. Task O-1 assigned.
- ANTIGRAVITY (argon, w9:t1) — GPU & Perf — BUSY (YouTube). Task A-1 queued.
- GROK (grok-codexbg, w9:t2) — not a FASTLINK team member (YouTube research/creative). Do not assign FASTLINK work.

## Active tasks
- O-1 (OPENCODE): CPU-only FASTLINK v0.1 (Rust) <-> llama.cpp adapter integration; loopback byte-exact tensor-chunk test. No GPU, no running-server changes.
- A-1 (ANTIGRAVITY, when free): GPU benchmarking harness + Qwen3.5-4B single-GPU baseline on detti (RX 6600 XT). Distributed (7900 XT) BLOCKED on B1.
- A-2 (ANTIGRAVITY, after B1): 8K ctx distributed (7900+6600) vs single-GPU benchmark; increase ctx when stable.

## Next priorities
1. O-1 (OPENCODE) — unblocks everything; CPU-only so no GPU conflict.
2. A-1 (ANTIGRAVITY) — harness + detti baseline; can proceed without the 7900 XT.
3. A-2 (ANTIGRAVITY) — needs B1 cleared.

## Resume commands
- Rust engine: cd /home/nightknight/Projects/FASTLINK && git checkout qa/fastlink-v3-mission-qa && cargo test --all-targets
- Rust alt: cd /home/nightknight/Projects/FASTLINK-heretic-agent && git checkout agent/heretic-m1-hardening && cargo test --all-targets
- Adapter: cd /mnt/gamez/AI/llama.cpp-fastlink-rpc-adapter && git checkout feat/fastlink-rpc-adapter
- Model (Vulkan): /mnt/gamez/AI/model-tests/qwen3d-sensenova/llama-sensenova/build-vulkan/bin/llama-server -m /mnt/gamez/AI/model-tests/qwen3d-sensenova/Qwen3.5-4B-3D-Coder-Q4_K_M.gguf
- Dual-GPU cluster: /home/nightknight/Projects/NightKnight-Distributed-AI/scripts/start_dual_gpu_ai.sh (requires explicit permission to start GPU processes)
