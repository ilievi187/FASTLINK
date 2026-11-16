# FASTLINK Team Checkpoints (AGY + OpenCode)

Integration branch: `integrate/fastlink-team` (based on `origin/main` `8cd1b28`).
Tests may run under owner authorization; the M1 CPU/software test suite and integration CI have passed. Real model loading and GPU inference remain gated on explicit hardware approval.

## Checkpoint 1 — 2026-11-12 (Control Plane, Residency, P2P Pipeline, KV Delta)

- **Wave 1 (AGY)** — `feat/manifest-pairing` (`ae83736`, merged `dc79309`):
  - `src/pairing.rs`: Opt-in peer pairing (`NetworkTrust::{Loopback, Lan, Tailscale, Untrusted}`), keyed BLAKE3 challenge-response MAC, replay/stale-epoch protection, session key derivation.
  - `src/manifest.rs`: Versioned `PartitionManifest` with BLAKE3 digest verification, direct P2P `next_stage_addr` links (no coordinator hairpin), and `ManifestCoordinator` lifecycle (`Joining`, `Active`, `Draining`, `Evacuated`, `Faulted`) + atomic epoch-bumped resharding.
  - `tests/manifest_pairing_test.rs`: 4 tests written (compilation + clippy verified; execution pending permission).
- **Wave 2 (AGY)** — `feat/pipeline-residency` (`f616699`, merged `e2c13b0`):
  - `src/residency.rs`: `ResidencyTable` for stage weights and KV/SSM state, budget enforcement with LRU unpinned eviction, BLAKE3 digest verification, and epoch-based stale-state purge.
  - `src/pipeline.rs`: `ActivationFrame` (`FLAC` magic, 69-byte header, BLAKE3 payload digest, bounded RAW/RLE codec, adjacent-hop & epoch checks) + `summarize_pipeline` (`p50`/`p95` compute & link RTT, byte accounting, `TelemetrySource::{Measured, Simulated}`).
  - `tests/pipeline_residency_test.rs`: 3 tests written (compilation + clippy verified; execution pending permission).
- **Wave 3 (AGY)** — `feat/stage-node-runtime` (`77c19bb`, merged `4eb006a`):
  - `src/node_runtime.rs`: Multi-stage loopback TCP activation relay (`run_loopback_pipeline`) verifying resident weights per stage and forwarding activations point-to-point to `next_stage_addr`.
  - `tests/node_runtime_test.rs`: 1 end-to-end 3-stage loopback test written (compilation + clippy verified; execution pending permission).
- **Wave 4 (AGY)** — `feat/kv-delta-checkpoint` (`bb20b10`, merged):
  - `src/kv_delta.rs`: `KvPageSnapshot`, `diff_kv_pages`, `build_kv_delta_packet`, `apply_kv_delta_packet` for BLAKE3-verified incremental KV/SSM state migration.
  - `tests/kv_delta_test.rs`: 1 test written (compilation + clippy verified; execution pending permission).

## Checkpoint 2 — 2026-11-12 (Heterogeneous Scheduler, Plan CLI, Cluster Orchestrator, Stage Auth, KV Migration Runner)

- **Wave 1 (OpenCode)** — `feat/heterogeneous-scheduler` (`6d2976f`, merged `e37f712`):
  - `src/scheduler.rs`: Heterogeneous 2–16+ device contiguous layer-stage partitioner (`plan_heterogeneous_stages`, `PlanObjective`, `DeviceRole`, `ModelSpec`, `NodeProfile`, `sample_profiles_{1,2,3,4,7,16}`), hard-gating slow/thermal/low-battery nodes off decode critical path.
  - `tests/scheduler_test.rs`: 70B memory-fit, mobile exclusion, no-degradation gating, and contiguous coverage tests (compilation + clippy verified; execution pending permission).
- **Wave 2 (AGY)** — `feat/cluster-orchestrator` (`f5bd1f0`, merged):
  - `src/cluster_orchestrator.rs`: `ClusterOrchestrator` bridging `scheduler::MultiStagePlan`, `pairing::NetworkTrust`, `manifest::ManifestCoordinator` epoch-bumped resharding, `LayerMigration` diffing, and `node_runtime::run_loopback_pipeline`.
  - `tests/cluster_orchestrator_test.rs`: Untrusted rejection, plan/commit/evacuate/loopback pipeline, and fault reshard tests (compilation + clippy verified; execution pending permission).
- **Wave 2 (OpenCode)** — `feat/plan-cli` (`57f5ef5`, merged `9f0a6e4`):
  - `src/cli_plan.rs` + `src/main.rs`: Dry-run `fastlink plan [--nodes 1|2|3|4|7|16] [--objective largest|latency|throughput] [--model small|70b] [--json]` subcommand.
  - `tests/cli_plan_test.rs`: Arg parsing, invalid flags, synthetic cluster sizes, and JSON/text output tests (compilation + clippy verified; execution pending permission).
- **Wave 3 (AGY)** — `feat/authenticated-stage-handshake` (`bf0c252`, merged):
  - `src/stage_auth.rs`: Epoch- and hop-scoped keyed-BLAKE3 authentication (`derive_hop_key`, `seal_stage_frame`, `open_stage_frame`) for `ActivationFrame` packets.
  - `tests/stage_auth_test.rs`: Valid MAC verification, tampered frame, wrong key, stage mismatch, and short packet tests (compilation + clippy verified; execution pending permission).
- **Wave 3 (OpenCode)** — `feat/kv-migration-runner` (`8873737`, merged):
  - `src/migration_runner.rs`: `execute_layer_migration` and `execute_reshard_migrations` driving `kv_delta` packet transfer and BLAKE3 digest verification over `LayerMigration`s.
  - `tests/migration_runner_test.rs`: Cold migration, warm incremental delta migration, and range mismatch rejection tests (compilation + clippy verified; execution pending permission).

## Checkpoint 3 — 2026-11-13 (Authenticated Loopback Runtime & End-to-End Orchestrator Wiring)

- **Wave 4 (AGY)** — `feat/orchestrator-kv-migration` (`23ec050`, merged):
  - `src/cluster_orchestrator.rs`: Added `ClusterKvStore` and `ClusterOrchestrator::evacuate_with_kv_migration` driving `migration_runner::execute_layer_migration` during evacuation resharding.
  - `tests/orchestrator_kv_migration_test.rs`: End-to-end evacuation + incremental KV delta migration + digest verification test (compilation + clippy verified; execution pending permission).
- **Wave 4 (OpenCode)** — `feat/auth-node-runtime` (`19e381c`, merged):
  - `src/node_runtime.rs`: Refactored into shared `run_pipeline_inner` with `run_authenticated_loopback_pipeline` using `stage_auth::derive_hop_key`, `seal_stage_frame`, and `open_stage_frame`.
  - `tests/auth_node_runtime_test.rs`: 3-stage and single-stage authenticated loopback pipeline tests (compilation + clippy verified; execution pending permission).
- **Wave 5 (AGY)** — `integrate/fastlink-team`:
  - `src/cluster_orchestrator.rs` + `tests/cluster_orchestrator_test.rs`: Added `ClusterOrchestrator::run_authenticated_inference` wiring `run_authenticated_loopback_pipeline` into the cluster orchestrator.

## Checkpoint 4 — 2026-11-15 (Linux Local Inventory; Desktop GUI In Progress)

- GitHub issue #7 confirmed open: heterogeneous one-model runtime; Linux desktop GUI is the first delivery priority. Keep all large-model, device-count, and tok/s claims unproven until real inference measurements exist.
- **M1 backend subtask (AGY)** — `feat/linux-device-inventory`, commit `2cbcc843c735307762c5b38d9b777314ccd63f22`, fast-forwarded into `integrate/fastlink-team` and pushed; remote branch verified at the same SHA.
  - Added passive, local-only `fastlink devices [--json]` with versioned `fastlink.devices.v1`, host CPU/RAM and available Linux DRM GPU metadata, explicit unknown fields, and no network discovery or GPU compute.
  - Added `tests/devices_test.rs`; commander independently verified `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test --all-targets` (47 passed, 0 failed), and actual `cargo run --quiet -- devices --json` output.
- **M1 Linux desktop GUI — integrated** — `integrate/fastlink-team` merge commit `ef7968e` includes the validated `feat/linux-local-device-ui` branch at `aeebc5248081877b79208497dabeb4d38af576c7`. Added the native `fastlink-gui` eframe/egui shell, actual `fastlink.devices.v1` inventory integration, truthful CPU/RAM/GPU fields, refresh, node selection, session-only local opt-in, and explicit no-peers/models/chat states. No LAN scan, listener, model load, inference, or GPU compute.
  - Commander verified on the integrated worktree: `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo build -p fastlink`, `FASTLINK_BIN=$PWD/target/debug/fastlink cargo test --workspace --all-targets` (119 passed, 0 failed), `cargo build -p fastlink-gui`, Windows GNU target `cargo check`, and Linux desktop-entry/SVG validators. AGY re-reviewed all GUI fixes and reported `No findings.` Feature-branch GitHub CI run `43756067094` passed Ubuntu and Windows.
- **M1 packaging — integrated** — commit `b937c612f2600bd4db186239192abdf3b7f8a4f7` from `feat/linux-desktop-packaging` was merged in integration commit `6fe7341`. Includes a Freedesktop desktop entry, original SVG icon, and manual per-user install/remove guide; `desktop-file-validate` and XML validation pass.
- **Milestone boundary:** M1 now has a real local Linux desktop shell and passive local-node management. Secure LAN/Tailscale discovery/pairing, add/remove remote peers, model selection/loading, and chat/inference remain unimplemented; no multi-device inference or tok/s claim is made. No live GUI window was launched during final verification because VRAM was heavily occupied; the GUI target and headless UI tests passed.
- **Verification state:** GUI and packaging are merged in integration commits `ef7968e` and `6fe7341` on the pushed `integrate/fastlink-team` branch. GitHub CI run `43757244143` passed on Ubuntu and Windows for integration tip `ae8876b68cc9b67f9beac1fae94950eb8773f598`; this checkpoint update contains documentation/policy only. No merge to `main`.
- Protected services/work: no model load, inference, GPU compute or model downloads were started, and Heretic was not interrupted. At `2026-11-15 22:28 +02:00`, `gpu-task status` showed 19,695/20,464 MiB VRAM used, Heretic not running, and 15,959 MiB RAM available; do not allocate occupied GPU memory without explicit approval.
- **M2 authorization and privacy boundary:** Owner authorized user-opted FASTLINK discovery/pairing only on their LAN/Tailscale and explicitly prohibited publishing raw addresses. `AGENTS.md` allows private addresses only transiently as local routing data: never log, persist, display, commit or publish them; no scan/probe outside an explicit discovery action. No actual LAN/Tailscale discovery has been run. The implementation contract is in `docs/M2_DISCOVERY_CONTRACT.md`; it requires an explicit GUI/CLI action, redacted output, unknowns left unknown, and no unverified candidate entering the scheduler. Secure encrypted pairing/worker traffic is required before model/prompt/KV data. Real multi-device inference remains gated on explicit GPU approval and safe model/device availability.

## Checkpoint 5 — 2026-11-15 (M2 Opt-In Discovery Implementation Started)

- Shared contract committed on `integrate/fastlink-team` as `73d0661` (`docs/M2_DISCOVERY_CONTRACT.md`); privacy-rule update is recorded in `AGENTS.md`. No LAN/Tailscale discovery has been run by the implementation team.
- **AGY backend assignment:** branch `feat/fastlink-m2-discovery`; worktree `/home/nightknight/Projects/FASTLINK/.worktrees/team-integrate-v2/.worktrees/feat-fastlink-m2-discovery`; owns `src/discovery.rs`, CLI registration in `src/main.rs`/`src/lib.rs`, and focused backend tests. Implement explicit `fastlink discover --json` with bounded providers; no network action in tests.
- **OpenCode GUI assignment:** branch `feat/fastlink-m2-gui`; worktree `/home/nightknight/Projects/FASTLINK/.worktrees/team-integrate-v2/.worktrees/feat-fastlink-m2-gui`; owns `crates/fastlink-gui/**` and `docs/GUI.md`. Implement explicit Discover-button flow against `fastlink.discover.v1`; no discovery on startup/local refresh and no fake pairing action.
- At Checkpoint 5, both prompts had been accepted and both agents were working in FASTLINK Space `wV` (tabs `AGY` and `OPENCODE`); neither feature branch had yet been reviewed or tested. See Checkpoint 6 for current branch status. The worker security/pair-remove slice follows after the discovery interfaces are proven.
- Hardware gate remains: no GPU/model load/inference until explicit safe approval; last `gpu-task` status at `2026-11-15 22:28 +02:00` showed 19,695/20,464 MiB VRAM used.

## Checkpoint 6 — 2026-11-15 (M2 GUI verified; backend privacy follow-up in progress)

- **GUI Discover feature branch verified, not integrated** — `feat/fastlink-m2-gui` remote HEAD `ab7c5301eb54e8c0dd18402f72d6b9b8b6bfa442`. The branch was rebased onto integration tip `52c2596` and pushed. GitHub CI run `43767443495` passed Ubuntu and Windows after a behavior-preserving Clippy cleanup. Commander independently ran formatting, workspace Clippy, CLI/GUI builds, all 147 workspace tests, Windows GNU cross-check, and `git diff --check`; all passed. Final independent GUI review at `23e0772` reported no findings. It noted the documented non-Unix cancellation limitation: Windows kills the direct child only; Linux process-group cancellation is the supported path.
- **Backend ETXTBSY test flake isolated and fixed on feature branch** — `feat/fastlink-m2-discovery` remote HEAD `c674ab1cc5352e89f86e8e6a804fcaa1d6be4272`; CI run `43767547207` passed Ubuntu and Windows. Syscall tracing proved Linux `Command::spawn` can clone the process FD table while another Rust test thread has a fixture script open for writing, causing `execve` to fail with `ETXTBSY`. A test-only mutex serializes script-writing/process-spawning tests; AGY reports a 100/100 parallel-contention pass and 139 workspace tests passing. No production retry was added.
- **Backend local commit `4a79227b1f6626fd064ec0415dfe97bcacefdc47`; not pushed** — it contains the reviewed process-group cleanup/cancellation fix from `8103fb9` plus test-only removal of a redundant PID kill that caused benign `kill: ... No such process` stderr. Independent review of `8103fb9` reported no findings; a second read-only review of the final fixture-only diff is pending. At exact HEAD `4a79227`, commander ran focused fixture tests (both pass, no stderr), fmt, full workspace tests (all pass), Linux Clippy, Windows GNU check/Clippy, diff-check, and 100/100 repetitions of the deterministic post-exit cancellation test. No latest-head GitHub CI yet; do not push until the final fixture review completes.
- **Task 7 design complete; implementation assigned, local-only** — isolated worktree `/home/nightknight/Projects/FASTLINK/.worktrees/team-integrate-v2/.worktrees/feat-secure-linux-worker`, branch `feat/secure-linux-worker`, based on integration tip `523269aff87af2c9e65c659c8ed28c1b7a3cd61a`; design commit `363810a74c32f0dd95cce224dd59ac5f15af9468`. The design recommends Noise `XXpsk3_25519_ChaChaPoly_BLAKE2s` via `snow` 0.10 with a high-entropy user-transferred PSK, explicit remove/re-pair, and a 65,535-byte message cap with chunking. OpenCode is now implementing loopback-only worker/pair/remove APIs and tests on this branch; no push/merge until independent security review, and no live LAN/Tailscale, GPU, model, or inference work. `snow` is not formally audited; no production claim is allowed.
- **Task 11 MoE planner independently reviewed and CI-validated; integration local, remote push pending** — feature branch `feat/moe-scheduler` is `0c0c4398d2be14a5c07247b4b3facc2c38275a5d`, rebased on integration `0fcf5ba`; the independent review of original source commit `316b0d7` found no findings. Feature CI `43842010207` passed Ubuntu and Windows. Commander reran full workspace tests, fmt, Linux Clippy, Windows GNU check/Clippy, and diff-check on the rebased branch; all passed. Fast-forwarded locally into `integrate/fastlink-team` at `0c0c439` and reran full integration gates successfully. No inference/model execution; waiting to push integration checkpoint and verify exact integration-head CI.
- **Integration checkpoint CI** — Rust CI run `43839454952` passed at exact integration HEAD `a2dc16b4398ebb9d4c10ba0f70721cc9bd584966`; subsequent integration doc checkpoint at `0fcf5ba` has a separate CI run pending.
- **M2 boundaries:** no LAN/Tailscale discovery action has been run; all tests use fixtures/loopback only. No model load, inference, or GPU work. MoE is integrated locally into `integrate/fastlink-team`; `main` is untouched, and backend/GUI discovery are not yet integrated.
- **Next:** push the validated MoE integration plus checkpoint and verify exact-head CI. Separately await final fixture-diff review for backend `4a79227`; then push and verify Linux/Windows CI before integrating backend/GUI. Secure worker remains local and unaudited.
