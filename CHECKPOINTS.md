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
- **M2 authorization and privacy boundary:** The owner authorized opt-in FASTLINK discovery/pairing only on their LAN/Tailscale and explicitly prohibited publishing raw addresses. Repo `AGENTS.md` now permits those private addresses only transiently as local routing data: never log, persist, display, commit or publish them; no scan/probe outside an explicit discovery action. No real LAN/Tailscale discovery has been run yet. Next: implement an explicit user-triggered discovery flow and secure paired Linux worker. Actual multi-device model inference remains gated on explicit GPU permission and a safe available model/device; no performance claim is made.
