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

## In-Flight

- **Wave 1 (OpenCode)** — `feat/heterogeneous-scheduler` in `.worktrees/team-opencode-scheduler`: Heterogeneous 2–16+ device contiguous layer-stage partitioner (`src/scheduler.rs`, `tests/scheduler_test.rs`).
