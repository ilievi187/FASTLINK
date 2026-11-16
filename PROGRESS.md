# Development progress

## 2026-11-15 — Post-exit capture drain race (final review)

- Fixed the final P3 finding: `capture_child_unix` used `min(now + POST_EXIT_DRAIN, overall_deadline)` for the post-exit drain. If a completed child was observed after the deadline, buffered valid JSON was skipped and reported as empty/malformed.
- The post-exit drain now gets its own bounded 50 ms grace after the worker observes child exit. A still-running child is still killed at the overall deadline; only an already-exited child can consume the extra drain window. The hard byte cap is enforced during the drain.
- Added deterministic Unix tests by injecting a poll interval longer than the timeout: a child that finishes during the sleep is accepted; an already-finished child with `timeout=0` is accepted; and oversized buffered output is still rejected by the cap.
- Verified: `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo build -p fastlink`, `FASTLINK_BIN`-backed `cargo test --workspace --all-targets` (50 core + 69 GUI tests), `cargo build -p fastlink-gui`, and the Windows-target fallback compile check. No GUI window or GPU use.

**Explicit limitation:** an already-exited child can extend total capture time by at most the 50 ms drain grace beyond the overall deadline. Inherited descendants are not killed; they cannot block the worker or grow capture storage on Linux. The non-Unix temp-file fallback remains best-effort, not hard-bounded.

## 2026-11-15 — Hard-bounded capture and truthful stderr detail (second review)

- Replaced the Linux/Unix temporary-file capture from the previous checkpoint with nonblocking pipe readers owned by the waiting worker. There are no reader threads to leak. Bytes are counted as they are read, and the child is killed when either stream exceeds 1 MiB; capture never writes a growing regular file.
- A fast 64 MiB writer now stops at the capture limit, and an inherited writer is cut off when the read ends close. The worker never waits for pipe EOF, so inherited handles cannot hang it. The direct child is still killed and reaped on timeout/capture-limit/wait errors.
- Made stderr diagnostics truthful: the error header reports bytes captured, bytes retained (and retained characters), plus the actual 900-character / 12-line UI detail cap. `compact_detail` now includes its ellipsis within both limits instead of exceeding 900 characters.
- Expanded the regression matrix: >64 KiB on both streams still parses, 2 MiB and 64 MiB fast writers are rejected promptly, measured fast-writer progress stays near the 1 MiB cap, oversized stderr is rejected, stderr accounting is exact, timeout kills the child, and inherited readers/writers neither block nor keep consuming capture resources.
- Retained a compile-checked non-Unix fallback using temporary-file polling; it is documented as best-effort rather than a hard OS bound. Linux is the supported/runtime-tested path.
- Verified: `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo build -p fastlink`, `FASTLINK_BIN`-backed `cargo test --workspace --all-targets` (50 core + 66 GUI tests), and `cargo build -p fastlink-gui`. Cross-checked the Windows fallback with `cargo check --target x86_64-pc-windows-gnu`. No GUI window or GPU use.

**Explicit limitation:** the Linux cap counts bytes delivered to the reader; the kernel pipe buffer can transiently hold at most one pipe capacity beyond that count, but disk usage cannot grow. Only the direct child is terminated; descendants are not killed cross-platform, though they cannot block the worker or grow capture storage on Linux.

## 2026-11-15 — Desktop shell output-capture hardening (superseded by the entry above)

- Fixed the P2 review finding in `InventoryCommand::run`: piped stdout/stderr were not drained while waiting, so a CLI writing more than pipe capacity could deadlock until the timeout, and inherited pipe handles could hang `wait_with_output()` past the deadline.
- Replaced pipes with private, restart-safe temporary-file capture. The kernel drains both streams concurrently; regular-file reads never wait for EOF, so a grandchild holding inherited handles cannot block the worker. Each stream is capped at 1 MiB; stderr diagnostics retain 64 KiB with an explicit truncation marker. Capture files are removed after use.
- Kept the deadline over child execution and capture-size monitoring, killed and reaped the owned child on timeout/capture-limit/wait errors, and preserved existing CLI path/error UX.
- Added TDD regression tests: 128 KiB on both stdout and stderr still parses; oversized stdout and oversized stderr produce deterministic capture-limit errors; exit stderr is retained with a truncation marker; timeout kills the owned child; a grandchild holding the output handles does not block after child exit.
- Verified: `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `FASTLINK_BIN`-backed `cargo test --workspace --all-targets` (50 core + 63 GUI tests), and `cargo build -p fastlink-gui`. No GUI window or GPU use.

**Explicit limitation:** only the direct CLI child is terminated on timeout/error; descendants are not killed cross-platform. File-based capture means they cannot block the GUI worker or its deadline. Output beyond 1 MiB per stream is rejected rather than parsed.

## 2026-11-15 — Linux local-device desktop shell (`feat/linux-local-device-ui`)

- Added the `fastlink-gui` workspace crate: native eframe/egui desktop shell for the first local-device milestone, with the GUI kept out of the core crate's dependency set.
- Merged the latest `integrate/fastlink-team` (`558ab65`, merge `e0340be`) and aligned the parser with the implemented `fastlink devices --json` contract: `schema`, `schema_version`, `scope`, `discovery_mode`, `node_count`, and `nodes[]`.
- The sole node in `scope:"local_only"` is treated as the local device. Its opaque hashed `node_id` is preserved exactly; no literal `local` id is expected, and a missing id disables selection/opt-in rather than inventing one.
- Rendered node facts (hostname, OS/release, architecture, logical CPU count, total/available/free RAM), GPU objects, capability fields, and the CLI's explicit `unknown_fields` list. Null values and the backend `"unknown"` connectivity sentinel remain `Unknown`; network speed/connectivity is never inferred.
- Added manual refresh with a bounded 20 s worker invocation, local-node selection, session-only local-node opt-in, stale-snapshot warnings, honest peer/model/chat empty states, and visible CLI/JSON/schema errors.
- Added a sanitized fixture with the exact backend field set (`crates/fastlink-gui/testdata/devices-v1.json`) and headless parser/state/UI tests. Added an environment-gated command-runner test that invokes the actual integrated binary from `FASTLINK_BIN` and proves the GUI parser accepts its live output.
- Verified on Linux without launching a GUI window: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test --all-targets` with `FASTLINK_BIN` pointing at the built CLI (50 core tests plus 57 GUI tests passing), and `cargo build -p fastlink-gui`.
- Documented build/run, the actual contract, fixture handling, headless verification and limitations in `docs/GUI.md` and the README.

**Explicit limitations:** Linux is the only verified target. Window rendering uses OpenGL (no CUDA/ROCm compute), and this work launched no GUI window; tests are CPU/software headless. No pairing, models, chat, inference or remote controls exist yet. Capabilities that the CLI does not measure remain unknown. Opt-in is session-only, local-node-only and never persisted.

## 2026-11-05 — M0

- Public GitHub repository established: `ilievi187/FASTLINK`.
- Apache-2.0 Rust implementation with a documented binary wire protocol.
- BLAKE3 chunk-addressed cache, exact deduplication, changed-chunk transfer.
- RAW/RLE lossless codec with size-based selection and bounded reconstruction.
- Loopback-only TCP CLI: `fastlink serve` and `fastlink send`.
- Five Rust unit tests passed; rustfmt, clippy and release compilation passed locally.
- GitHub Actions cross-platform CI passed on Ubuntu and Windows (run 42098205495).
- 8 MiB synthetic local test: cold payload 1,552 B; warm payload 0 B; one-byte delta 518 B.
- SHA-256 of source and output matched; byte comparison passed.

**Explicit limitation:** This does not yet accelerate GPU inference; no GPU benchmarks exist for FASTLINK's new Rust transport. This log is not a measure of real RPC performance.

## 2026-11-12 — Team Checkpoint 1 (`integrate/fastlink-team`)

- Added `src/pairing.rs` and `src/manifest.rs`: opt-in keyed-BLAKE3 pairing (`Loopback`/`Lan`/`Tailscale` only), versioned `PartitionManifest` with direct adjacent-stage `next_stage_addr` routing, and `ManifestCoordinator` join/evacuation/atomic-reshard state machine.
- Added `src/residency.rs` and `src/pipeline.rs`: `ResidencyTable` (budgeted weights & KV/SSM state, epoch invalidation, BLAKE3 verification) and `ActivationFrame` (`FLAC` bounded P2P framing + `p50`/`p95` boundary telemetry).
- Added `src/node_runtime.rs`: multi-stage loopback TCP P2P activation relay (`run_loopback_pipeline`).
- Added `src/kv_delta.rs`: BLAKE3-verified incremental KV/SSM page delta checkpointing for stage migration.
- Verified `cargo fmt --check`, `cargo check --all-targets`, `cargo clippy --all-targets -- -D warnings`, and `cargo test --all-targets` (42/42 unit and integration tests passing) on `integrate/fastlink-team`.

**Explicit limitation:** No multi-device hardware or GPU inference benchmarks have been run (CPU-only loopback and synthetic profile tests).

## 2026-11-13 — Team Checkpoint 3 (`integrate/fastlink-team`)

- Wired `migration_runner` into `ClusterOrchestrator::evacuate_with_kv_migration` with `ClusterKvStore` (`tests/orchestrator_kv_migration_test.rs`).
- Wired `stage_auth` into `node_runtime::run_authenticated_loopback_pipeline` (`tests/auth_node_runtime_test.rs`) and `ClusterOrchestrator::run_authenticated_inference`.
- Verified `cargo fmt --check`, `cargo check --all-targets`, `cargo clippy --all-targets -- -D warnings`, and `cargo test --all-targets` (42/42 unit and integration tests passing) on `integrate/fastlink-team`.

**Explicit limitation:** No multi-device hardware or GPU inference benchmarks have been run (CPU-only loopback and synthetic profile tests).

## 2026-11-12 — Team Checkpoint 2 (`integrate/fastlink-team`)

- Merged `src/scheduler.rs` (`plan_heterogeneous_stages` for 2–16+ heterogeneous nodes) and `src/cli_plan.rs` (`fastlink plan` dry-run CLI subcommand).
- Added `src/cluster_orchestrator.rs`: bridges `scheduler` plans, `pairing::NetworkTrust`, `manifest::ManifestCoordinator` evacuation/fault resharding, `LayerMigration` computation, and `node_runtime::run_loopback_pipeline`.
- Added `src/stage_auth.rs`: epoch- and hop-scoped keyed-BLAKE3 MAC (`seal_stage_frame`, `open_stage_frame`) for P2P `ActivationFrame`s.
- Added `src/migration_runner.rs`: `execute_layer_migration` and `execute_reshard_migrations` using `kv_delta` for verified incremental KV/SSM state migration.
- Verified `cargo fmt --check`, `cargo check --all-targets`, `cargo clippy --all-targets -- -D warnings`, and `cargo test --all-targets` (42/42 unit and integration tests passing) on `integrate/fastlink-team`.

**Explicit limitation:** No multi-device hardware or GPU inference benchmarks have been run (CPU-only loopback and synthetic profile tests).

## 2026-11-13 — Team Checkpoint 3 (`integrate/fastlink-team`)

- Wired `migration_runner` into `ClusterOrchestrator::evacuate_with_kv_migration` with `ClusterKvStore` (`tests/orchestrator_kv_migration_test.rs`).
- Wired `stage_auth` into `node_runtime::run_authenticated_loopback_pipeline` (`tests/auth_node_runtime_test.rs`) and `ClusterOrchestrator::run_authenticated_inference`.
- Verified `cargo fmt --check`, `cargo check --all-targets`, `cargo clippy --all-targets -- -D warnings`, and `cargo test --all-targets` (42/42 unit and integration tests passing) on `integrate/fastlink-team`.

**Explicit limitation:** No multi-device hardware or GPU inference benchmarks have been run (CPU-only loopback and synthetic profile tests).


## 2026-11-15 — Linux Local Device Inventory (`feat/linux-device-inventory`)

- Added `src/devices.rs` and wired `fastlink devices [--json]` in `src/main.rs` and `src/lib.rs`.
- Reports exactly one local node using passive host evidence only: hashed stable local ID (`/etc/machine-id` / `/var/lib/dbus/machine-id` with `hostname-derived` fallback), hostname, OS/kernel release/architecture, logical CPU count, RAM total/available/free bytes (`/proc/meminfo`), and directly discoverable DRM GPU metadata/VRAM (`/sys/class/drm/card*` + `pci.ids`).
- Uses versioned JSON schema `fastlink.devices.v1` (`schema_version: 1`) with explicit `null`/empty values and `unknown_fields` for anything not directly discoverable; never fabricates capacity, throughput, network connectivity, or remote peers, and performs no network scans, port binds, or GPU compute.
- Added `tests/devices_test.rs` covering populated host evidence, missing/unknown fields, corrupt/malformed inputs, CLI arg validation, and live binary execution.
- Verified `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and `cargo test --all-targets`.

**Explicit limitation:** Local-only passive Linux sysfs/procfs inventory; non-amdgpu GPUs without sysfs VRAM counters report `vram_total_bytes: null` / `vram_free_bytes: null`, and capacity/throughput/network connectivity remain explicitly `unknown` until measured.

## 2026-11-15 — M2 Opt-In Discovery Feature Branches (not integrated)

- The opt-in/privacy contract is committed on `integrate/fastlink-team` (`73d0661`): discovery only on explicit user action, LAN/Tailscale scope, no scanning/probing outside that action, no raw private addresses/endpoints in public output, and candidates remain untrusted/unpaired.
- GUI Discover flow: `feat/fastlink-m2-gui` remote HEAD `ab7c5301eb54e8c0dd18402f72d6b9b8b6bfa442`; GitHub Actions run `43767443495` passed Ubuntu and Windows. Commander verified format, workspace Clippy, builds, 147 workspace tests, Windows GNU cross-check, and diff check. Independent final review at `23e0772` reported no findings. The GUI branch is not yet merged into integration.
- Backend discovery: remote `feat/fastlink-m2-discovery` HEAD `c674ab1cc5352e89f86e8e6a804fcaa1d6be4272`; CI `43767547207` passed Ubuntu and Windows. A Linux `ETXTBSY` flake in parallel fake-provider tests was traced to a sibling `Command::spawn` child inheriting an open writable FD; test-only serialization fixed it. AGY reports 100/100 contention runs and 139 full workspace tests passed.
- Backend branch local HEAD `4a79227b1f6626fd064ec0415dfe97bcacefdc47` includes reviewed cleanup code from `8103fb9` plus a test-only cleanup that removes the redundant kill causing benign stderr. The process-cleanup review at `8103fb9` found no issues; final fixture-diff review is pending. At `4a79227`, both focused fixture tests pass without noise; fmt, full workspace tests, Linux Clippy, Windows GNU check/Clippy and diff-check pass; deterministic post-exit cancellation stress passed 100/100. Branch remains unpushed, so exact-head GitHub CI is still pending.
- Task 7/12 secure-worker implementation is committed locally at `40c95cec18aca308629eefce1a9f3bc540dc5118` on isolated `feat/secure-linux-worker` (design base `363810a`). It adds an experimental `snow` 0.10 Noise PSK loopback transport, trust/pair/remove APIs and bounded framing; full local Linux/Windows cross-check gates pass. Independent security review is running; no push/integration. Semgrep Rust scan returned two temp-directory findings only in test helpers (`#[cfg(test)]` unit code and integration test); the production path had no other findings in this scan. `cargo audit` is unavailable. `snow` is unaudited, and no no-echo pairing UX/public listener is integrated.
- Task 11 MoE planner is complete and integrated into `integrate/fastlink-team`: feature branch `feat/moe-scheduler` at `0c0c439` had independent review with no findings and CI `43842010207` passed Ubuntu/Windows. Integration HEAD `4a61d53` includes it; full local gates passed and exact-head integration CI `43842684310` passed Ubuntu/Windows. Planner only—no inference or throughput claims.
- Integration CI `43841805629` passed at `0fcf5ba`; latest exact integration CI `43842684310` passed at `4a61d53`.
- No real LAN/Tailscale discovery action, model load, inference, or GPU work has been started. MoE is integrated into `integrate/fastlink-team`; discovery GUI/backend remain unintegrated, and pairing/removal plus encrypted worker lifecycle remain pending. `main` is untouched.

**Next:** finish the independent review of backend fixture-only cleanup at `4a79227`; if clean, push the validated backend branch and verify exact-head Ubuntu/Windows CI, then integrate discovery backend and GUI into `integrate/fastlink-team`. Review secure-worker commit `40c95ce` and address findings before any push/integration; keep `snow` and pairing UX explicitly experimental.
