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
- NOTE: the adapter borrows FASTLINK's chunk/codec concept but is NOT yet wired to the Rust v0.1 repo (core M3 gap). 4 files currently uncommitted in that repo (Antigravity in progress) — OPENCODE restricted to the Rust repo.

Real distributed GPU inference (PRIMARY EXPERIMENT — NOT done):
- Target: Qwen3.5-4B-3D-Coder Q4_K_M across NightKnightPC (RX 7900 XT 20GB) + detti (RX 6600 XT 8GB).
- Model present: /mnt/gamez/AI/model-tests/qwen3d-sensenova/Qwen3.5-4B-3D-Coder-Q4_K_M.gguf (2.6GB) + mmproj-Qwen3.5-4B-BF16.gguf (645MB).
- Vulkan build: /mnt/gamez/AI/model-tests/qwen3d-sensenova/llama-sensenova/build-vulkan/bin/.
- Existing dual-GPU cluster: /home/nightknight/Projects/NightKnight-Distributed-AI (STATUS: WORKING, GPU inference not started; 15005 tunnel down).

## Blockers
- B1 (user decision 2026-11-08): RX 7900 XT is occupied by Saluki's 27B "Underdog-Saluki" server (port 18927, ctx 128K, 99 GPU layers). USER DECISION: keep that server RUNNING; do NOT stop it. The distributed (7900 XT) benchmark (A-2) is DEFERRED until the user frees the 7900 XT; Saluki will run A-2 once free.
- B2: No amdgpu_top/radeontop for live GPU-utilization measurement (harness gap — Antigravity must build a measurement path via ROCm/rocprof or server logs).
- B4: OPENCODE + Antigravity + Grok share the "Youtube Production CodexBG" Herdr workspace (w9); FASTLINK coordination lives in Fastlinkv3 (w8). Tasks reference absolute paths so workspace is irrelevant.

## User decision (2026-11-08)
- Keep the 27B server RUNNING (port 18927). Do NOT stop it.
- OPENCODE (O-1) + ANTIGRAVITY (A-1: harness + detti baseline) proceed NOW (no 7900 XT needed).
- A-2 (distributed 7900+6600) is DEFERRED until the user frees the 7900 XT; Saluki will run A-2 then.

## Active tasks
- O-1 (OPENCODE — working): CPU-only FASTLINK v0.1 (Rust) <-> llama.cpp adapter tensor framing; deterministic BLAKE3 chunk-ID + byte-exact unit test. Branch feat/fastlink-v3-rpc-integration; PR to open.
- A-1 (ANTIGRAVITY — working): GPU benchmarking harness (VRAM + util + network overhead + tok/s) + Qwen3.5-4B single-GPU baseline on detti (RX 6600 XT, 8K ctx). Branch feat/fastlink-v3-gpu-bench; PR to open. Distributed part blocked on B1.
- A-2 (DEFERRED): 8K ctx distributed (7900+6600) vs single-GPU benchmark; runs only after the user frees the 7900 XT. Saluki will execute it then.

## Next priorities
1. O-1 (OPENCODE) — in progress; Saluki reviews the diff, runs tests, validates acceptance, then commits/pushes/PRs/merges per protocol.
2. A-1 (ANTIGRAVITY) — in progress; same review protocol.
3. A-2 — deferred until the 7900 XT is freed by the user.

## Resume commands
- Rust engine: cd /home/nightknight/Projects/FASTLINK && git checkout qa/fastlink-v3-mission-qa && cargo test --all-targets
- Rust alt: cd /home/nightknight/Projects/FASTLINK-heretic-agent && git checkout agent/heretic-m1-hardening && cargo test --all-targets
- Adapter: cd /mnt/gamez/AI/llama.cpp-fastlink-rpc-adapter && git checkout feat/fastlink-rpc-adapter
- Model (Vulkan): /mnt/gamez/AI/model-tests/qwen3d-sensenova/llama-sensenova/build-vulkan/bin/llama-server -m /mnt/gamez/AI/model-tests/qwen3d-sensenova/Qwen3.5-4B-3D-Coder-Q4_K_M.gguf
- Dual-GPU cluster: /home/nightknight/Projects/NightKnight-Distributed-AI/scripts/start_dual_gpu_ai.sh (requires explicit permission to start GPU processes)
- Coordination: cd /home/nightknight/Projects/FASTLINK && git checkout feat/fastlink-v3-coordination && cat qa-mission/FASTLINKv3-{CHECKPOINT,TASKS}.md
