# FASTLINKv3 — Mission Checkpoint (2026-11-08, EET)

Owner: repository maintainer. Agent coordination details omitted.
Repository: github.com/ilievi187/FASTLINK (Rust v0.1 engine) + <CHECKOUT_01> (C++ <MODEL_ID> fork).

## Latest verified state (last known-good)

Rust engine (FASTLINK v0.1 — CPU delta-sync, 64KiB BLAKE3 chunks, RAW/RLE codec):
- `<CHECKOUT_06>` HEAD 074b248 — memory-budget cache (cache_budget.rs) + placement scheduler (scheduler.rs, WP-C formulas).
- FL3-E5 Step 3 Run 4: all four checkouts passed Rust fmt, Clippy, and cargo tests; the network-sim checkout also passed Python simulation (57/0).
- 47 Rust tests pass (33 lib + 14 integration); cross-branch regression diffs (FL3-E3) show 0 regressions.
- FL3-D1..D5: fault harness (10/10), bounded parsing + crash recovery, Windows GNU target (42 lib + 15 int under Wine), chunk-frame write_vectored microbench.
- Cross-machine validation: Windows/Linux test path, 16 MiB transfers; cold/warm/delta measurements were byte-exact. Hostnames and endpoints omitted.

<MODEL_ID> RPC adapter (C++ fork):
- `feat/fastlink-rpc-adapter` HEAD 7951636 — FL4-INT (Antigravity): persistent tensor residency in RPC backend (warm reuse, byte-exact, measured reduction).
- FLR3 v3 codec selection metadata (RAW/RLE/LZ4/ZSTD) + CPU round-trip tests; --chunk-sweep (<MODEL_SCALE>..256K) + 5-strategy comparison.
- 1 MiB FLR1/FLR3 overhead profile + CPU codec benchmark; fl3e1_adapter_check.sh + qa/adapter-matrix.md.
- 128K Vulkan inference + JCode verification recorded; opt-in FASTLINK-RPC tensor codec with Vulkan QA.
- NOTE: the adapter borrows FASTLINK's chunk/codec concept but is NOT yet wired to the Rust v0.1 repo (core M3 gap). 4 files currently uncommitted in that repo (Antigravity in progress) — OPENCODE restricted to the Rust repo.

Real distributed GPU inference (PRIMARY EXPERIMENT — NOT done):
- Target: a model variant on two generic devices; the distributed experiment was not performed.
- Environment-specific paths, hardware/model identifiers, and tunnel endpoints are redacted.
- Reproduction notes use repository-relative paths and generic placeholders only.
- No raw private endpoint or local host value is required for the documented CPU-only checks.

## Blockers
- B1 (owner decision 2026-11-08): the existing model service must remain running. Do not stop it. Distributed GPU benchmarking is deferred until the owner explicitly frees capacity.
- B2: No amdgpu_top/radeontop for live GPU-utilization measurement (harness gap — Antigravity must build a measurement path via ROCm/rocprof or server logs).
- B4: Agents coordinate in separate workspaces; explicit branch/file ownership prevents conflicts.

## User decision (2026-11-08)
- Keep the existing owner-managed model service running. Do not stop it.
- O-1 and A-1 proceed with CPU-only work; no GPU access is needed.
- A-2 distributed benchmarking is deferred until owner-authorized GPU capacity is available.

## Active tasks
- O-1 (OPENCODE — working): CPU-only FASTLINK v0.1 (Rust) <-> <MODEL_ID> adapter tensor framing; deterministic BLAKE3 chunk-ID + byte-exact unit test. Branch feat/fastlink-v3-rpc-integration; PR to open.
- A-1 (ANTIGRAVITY — working): CPU-safe benchmark harness and a single-device baseline on a generic host; identifying details omitted.
- A-2 (DEFERRED): 8K ctx distributed versus single-device benchmark; runs only after owner-authorized capacity is available.

## Next priorities
1. O-1 — in progress; maintainer reviews the diff, runs tests, validates acceptance, then commits/pushes/opens PRs per protocol.
2. A-1 — in progress; same review protocol.
3. A-2 — deferred until the owner authorizes capacity.

## Resume commands
- Rust engine: cd <CHECKOUT_05> && git checkout qa/fastlink-v3-mission-qa && cargo test --all-targets
- Rust alt: use `<CHECKOUT_06>` and run `cargo test --all-targets`.
- Adapter: cd <CHECKOUT_01> && git checkout feat/fastlink-rpc-adapter
- Model (Vulkan): <CHECKOUT_07> -m <CHECKOUT_02>
- Dual-GPU cluster: <CHECKOUT_08> (requires explicit permission to start GPU processes)
- Coordination: cd <CHECKOUT_05> && git checkout feat/fastlink-v3-coordination && cat qa-mission/FASTLINKv3-{CHECKPOINT,TASKS}.md
