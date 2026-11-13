# FASTLINK Team Checkpoints (AGY + OpenCode)

Integration branch: `integrate/fastlink-team` (based on `origin/main` `8cd1b28`).
Note: Test execution (`cargo test`) is deferred pending explicit owner permission; all merged commits below pass `cargo fmt --check`, `cargo check --all-targets`, and `cargo clippy --all-targets -- -D warnings`.

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
