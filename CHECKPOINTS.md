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

- **GUI Discover feature integrated locally; remote integration push pending** — rebased GUI branch `feat/fastlink-m2-gui` is `264f5d0f1c1c249683a1b573bfa8d8e67d7c13ec`; exact-head feature CI `43846304501` passed Ubuntu and Windows, and prior independent review `23e0772` found no issues. Its full workspace tests, fmt/Clippy, Windows GNU check/Clippy and diff-check passed; it was fast-forwarded locally on backend integration at `264f5d0`. GUI discovery remains explicit-button only; no startup/background scan or fake pairing action. Documented residual: Windows cancellation kills the direct child, while process-group tree cleanup is Linux-only.
- **Backend ETXTBSY test flake isolated and fixed on feature branch** — `feat/fastlink-m2-discovery` remote HEAD `c674ab1cc5352e89f86e8e6a804fcaa1d6be4272`; CI run `43767547207` passed Ubuntu and Windows. Syscall tracing proved Linux `Command::spawn` can clone the process FD table while another Rust test thread has a fixture script open for writing, causing `execve` to fail with `ETXTBSY`. A test-only mutex serializes script-writing/process-spawning tests; AGY reports a 100/100 parallel-contention pass and 139 workspace tests passing. No production retry was added.
- **M2 backend process cleanup reviewed, pushed, CI-green and integrated locally** — final fixture-only review of `4a79227` found no issues; the earlier full review at `8103fb9` also found no issues. Candidate `4a79227` passed local gates and 100/100 deterministic cancellation repetitions; after rebasing on integration tip `e88882a`, commit `acd4b61a9dcda8eb584a3bffeb274efb5d162460` passed full local gates and feature CI `43845541662` on Ubuntu/Windows. It is fast-forwarded locally into `integrate/fastlink-team`; exact integration CI `43846067899` passed Ubuntu/Windows. The redundant test PID kill was removed, so the old test no longer emits `No such process` stderr. Backend+GUI combined state is locally at `264f5d0`; push and exact-head integration CI remain.
- **Task 7/12 secure worker implementation committed locally; independent review pending** — isolated worktree `/home/nightknight/Projects/FASTLINK/.worktrees/team-integrate-v2/.worktrees/feat-secure-linux-worker`, branch `feat/secure-linux-worker`, commit `40c95cec18aca308629eefce1a9f3bc540dc5118` after design `363810a`. Adds experimental `snow` 0.10 Noise `XXpsk3` loopback worker, pairing/removal/trust-store API, bounded frames and tests; no GUI/no-echo pairing UX or public listener. Commander ran fmt, full workspace tests (including 14 secure-worker integration tests), Linux Clippy, Windows GNU check/Clippy and diff-check; all passed. Independent security review is in progress; nothing pushed or integrated, and `snow` remains unaudited. Local Semgrep Rust scan found two predictable-temp-dir matches only in test helpers (`#[cfg(test)]` unit module and test file); `cargo audit` is not installed. No production security claim.
- **Task 11 MoE planner integrated and verified** — feature branch `feat/moe-scheduler` HEAD `0c0c4398d2be14a5c07247b4b3facc2c38275a5d` was independently reviewed (no findings), passed feature CI `43842010207` on Ubuntu/Windows, then fast-forwarded into `integrate/fastlink-team`. Integration HEAD `4a61d535b18f3e1cb7b6a0ecf7bae9cc91bff440` includes the planner; commander reran full workspace tests, fmt, Linux Clippy, Windows GNU check/Clippy, and diff-check successfully, and exact-head CI `43842684310` passed both platforms. CPU-only planning/mapping only; no model inference or throughput claim.
- **Integration checkpoint CI** — prior doc commits `0fcf5ba` (run `43841805629`) and current MoE integration `4a61d53` (run `43842684310`) both passed Ubuntu and Windows.
- **M2 boundaries:** no live LAN/Tailscale discovery action has been run; discovery tests use fixtures/loopback only. No model load, inference, or GPU work. MoE and M2 backend+GUI are integrated locally in `integrate/fastlink-team` at `264f5d0`; `main` is untouched. Secure pairing/worker remains local and unaudited.
- **Historical next action (superseded):** M2 backend+GUI were later integrated into the team branch; see Checkpoint 7.

## Checkpoint 7 — 2026-11-16 (M2 discovery integrated; `main` untouched)

- `integrate/fastlink-team` is pushed at `5ef2789c88432b6b851b69453d77de0bcd69e980`; exact-head Rust CI `43846962473` passed Ubuntu and Windows. Its code parent `264f5d0` contains the MoE planner plus the M2 backend and GUI. Commander ran full workspace tests, fmt, Linux Clippy, Windows GNU check/Clippy and diff-check in the integration worktree before pushing; all passed.
- Backend process-leak fix: TDD RED was observed before the cancellation fix (`expected Cancelled at boundary-after-pump, got Ok(status=Some(0), len=17)`). The final backend fixture cleanup and process code were independently reviewed with no findings; the deterministic cancellation stress passed 100/100. Rebased backend commit `acd4b61` passed feature CI `43845541662` and integration CI `43846067899` on Ubuntu/Windows.
- GUI Discover: rebased feature commit `264f5d0` passed feature CI `43846304501`; independent GUI review `23e0772` found no findings. Discovery is button-triggered only; no startup scan or fake pairing UI.
- MoE planner remains CPU-only, independently reviewed with no findings and CI-green; no inference or throughput claim.
- Platform boundary: actual discovery providers run only on Linux; Windows CI checks compile/test behavior where discovery is unsupported. GUI cancellation on Windows kills the direct child only. No live LAN/Tailscale discovery action, model load, inference, or GPU work was performed.
- Task 5 and Task 8 are complete on `integrate/fastlink-team`; no changes were merged to `main`.
- Secure worker Task 7/12 remains local at `40c95ce`; independent security review is pending. `snow` is unaudited, no no-echo pairing UX/public listener is integrated, Semgrep flagged only test temp-directory helpers, and `cargo audit` is unavailable. Do not push/integrate or claim production readiness until review and residuals are resolved.

## Checkpoint 8 — 2026-11-16 (post-exit drain WouldBlock fix integrated; `main` untouched)

- Root cause of CI `43847764815` at `48934bd`: nonblocking `PipeReader::pump_with` treated `EAGAIN/WouldBlock` as completion even during the bounded post-exit drain, so a transient writer pause could truncate late output and make the deadline assertion flaky.
- Strict TDD RED was observed before the behavior fix: `capture_child_post_exit_drain_waits_past_wouldblock_for_delayed_chunk_until_deadline` got `Some(WouldBlock)` instead of `Some(Deadline)`. Commit `3042ac62a1b9211d34b4a4178a7d4b50ea00cb90` adds bounded `ppoll` waits only for post-exit draining; pre-exit capture remains nonblocking. Added delayed-chunk/deadline and cancellation-during-wait coverage.
- Independent review of `3042ac6` found no defects. Reviewer noted no dedicated tests for EOF/read/poll errors; existing EOF/cap/cleanup coverage passed, and focused error-path tests remain useful follow-up coverage.
- Local verification at `3042ac6`: focused discovery tests and 100× stress loop passed; full workspace suite, fmt, Linux Clippy, Windows GNU check/Clippy and diff-check passed. One earlier full local run transiently failed the unchanged GUI child-PID test; it passed 50/50 focused repetitions and the subsequent full suite.
- Feature CI `43851588654` and exact integration CI `43852078790` passed on Ubuntu and Windows. `integrate/fastlink-team` is pushed at `3042ac6`; the old failing run `43847764815` is superseded. Task 8 is complete again. No changes were merged to `main`.
- Secure-worker review completed on local prototype `40c95ce`: P1 pairing proof did not bind the persisted peer static key; P2 findings concern zeroizing the returned session key and enforcing one overall handshake deadline. OpenCode is fixing these plus predictable test temp directories on the isolated secure-worker branch. `snow` remains unaudited; no secure-worker push/integration or production-security claim.
- No live LAN/Tailscale discovery, GPU/model load, or inference was performed. Task 9 remains pending/blocked on explicit safe GPU approval.

## Checkpoint 9 — 2026-11-16 (secure-worker review fixes locally verified; not integrated)

- Candidate `d91d1546407290f97e1cf1800288f3fda92e517d` is committed and clean on isolated `feat/secure-linux-worker`, based on prior prototype `40c95ce`; it has not been rebased onto current integration `e29c9ee`, pushed, or merged.
- TDD corrections: proof now MAC-binds the peer static key and KDF; the returned `SessionKey` is non-Clone and zeroize-on-drop; all handshake frames/confirmation share one deadline with per-I/O remaining-time updates; test temp directories use dev-only `tempfile`.
- Commander verification at exact local HEAD `d91d154`: workspace all-targets tests, fmt, Linux Clippy, Windows GNU check/Clippy, diff-check passed. Targeted Semgrep over the four changed Rust files reported 0 findings. `tempfile` resolves only through dev-dependencies. Independent security re-review found no actionable defects; no remote CI has run for this candidate.
- TDD evidence reported in the verified agent transcript: mismatched pin RED; slow-drip handshake previously succeeded in 809 ms under a 500 ms budget and now fails at about 501 ms; normal loopback handshake still succeeds. Reviewer noted that this test delays whole messages rather than partial bytes, so task 15 adds direct partial-frame slow-drip coverage before rebase/push. PairingRegistry's temporary raw secret copy and snow internal zeroization remain residual risks.
- Residuals remain: `snow` 0.10 unaudited; no-echo pairing UX or integrated pairing network exchange; pending offers are in-memory; secrets persist in 0600 files rather than an OS keyring; snow internals are not explicitly zeroized. This is experimental only, not production-security evidence.
- Next: finish task 15 by observing RED on baseline `40c95ce` and GREEN on `d91d154`; if clean, rebase onto current `integrate/fastlink-team`, repeat exact-head gates/CI, then integrate only after green checks. No discovery scan, GPU/model/inference or main-branch change.

## Checkpoint 10 — 2026-11-16 (secure-worker branch rebased; CI green, review pending)

- Secure branch `feat/secure-linux-worker` is rebased onto integration base `65bc310` at `f4e63d0a99b9905340518f274b555c8bdcfa196d`, pushed to origin, and clean. No merge to integration or `main` yet.
- Strict RED/GREEN for the partial-frame timeout test: on baseline `40c95ce`, handshake returned `Ok` after 1.4686 s against a 1.2 s total deadline; on candidate `d91d154` plus test it fails closed at about 1.22 s. The partial frame arrives in 3-byte chunks every 60 ms; focused test passed 10/10 runs. Final rebased HEAD includes this test only after the security-fix commit.
- Commander reran full workspace/all-targets tests, fmt, Linux Clippy, Windows GNU check/Clippy, targeted Semgrep (0 findings across changed Rust files), dev-dependency check (`tempfile` only) and diff-check at exact rebased HEAD; all passed. Exact feature CI `43863887135` passed Ubuntu and Windows.
- Independent review of the new partial-frame test delta is still in progress. Earlier review of `d91d154` found no actionable defects; the test-only delta is awaiting its own review. `integrate/fastlink-team` is still at `65bc310`; merge only after review stays clean and exact-head CI remains green.
- Security residuals tracked, not waived: `snow` 0.10 remains unaudited; `PairingRegistry` keeps a temporary raw secret copy; secure pairing UX/network exchange is incomplete; secret files are 0600 rather than OS-keyring backed; `snow` internal state is not explicitly zeroized. This remains experimental, not production security.
- Parallel tasks are active on isolated worktrees: AGY `fix/gui-cancel-zombie-test` (test-only Linux GUI child-state reliability) and OpenCode `feat/cpu-stage-inference-proof` (tiny CPU-only authenticated loopback MLP correctness foundation, not LLM/multi-machine evidence).
- GPU/resource boundary: `gpu-task status` at 2026-11-16 13:52 +02 reported Voicebox generating; a local `llama-server` process was also using CPU. Neither service was touched; no GPU, model, discovery scan or live network action was started.
- Next: accept the independent test review; if clean, fast-forward the rebased secure branch into `integrate/fastlink-team`, push, and verify exact integration CI. Continue GUI/inference branches under their explicit CPU-only scopes.

## Checkpoint 11 — 2026-11-16 (secure-worker experimental slice integrated; parallel branches progressing)

- Secure-worker branch `feat/secure-linux-worker` was rebased onto integration `6ba1e08` as `3e2137e3357b6960a37921baea4a3bac464864e6`, then fast-forwarded into and pushed on `integrate/fastlink-team`. Exact feature CI `43866507647` and exact integration CI `43867116852` passed on Ubuntu and Windows. `main` remains `8cd1b281a545f1bb0806904c8ae97902d6b62f9b` and was not changed.
- Task 15 strict TDD evidence: the partial-frame slow-drip test produced RED on `40c95ce` (success after 1.4686 s against a 1.2 s total deadline), then GREEN on the absolute-deadline implementation at about 1.22 s, 10/10 repeats. Independent review of the test-only delta reported no findings; the earlier security re-review also reported no actionable defects. Commander reran full workspace tests, fmt, Linux Clippy, Windows GNU check/Clippy, Semgrep (0 findings), dev-dependency check and diff-check at the rebased candidate.
- Security-fix candidate binds the pairing proof/KDF to the pinned static key, uses a non-Clone zeroize-on-drop `SessionKey`, enforces one overall handshake deadline, and uses dev-only `tempfile`. Integrated code is an experimental loopback-only Linux worker prototype, not production security.
- Residual security risks remain tracked: `snow` 0.10 is unaudited (`cargo audit` unavailable); `PairingRegistry` retains an additional temporary raw-secret copy; secure pairing UX/network exchange is incomplete; secrets are stored in 0600 files rather than an OS keyring; Noise internal state is not explicitly zeroized. No production-security claim.
- **AGY GUI branch** `fix/gui-cancel-zombie-test`, commit `b89b830b4562e7196e70d327c9dd7041e8fdc53e`, remains isolated/local. TDD RED reproduced the zombie PID false-positive; the final Linux-only test helper distinguishes zombies from live processes, with RAII child reaping. Commander reran workspace/all-targets tests, fmt, Linux Clippy, Windows GNU check/Clippy and diff-check; all passed. Independent review is pending; no push/merge.
- **OpenCode CPU branch** `feat/cpu-stage-inference-proof` is rebased onto secure integration at `2af90f7cfb4a72d4db7b2c949cca41b4824cfc09`, pushed, and clean. TDD RED showed finite inputs/weights yielding `+inf + -inf = NaN` were converted to zero by ReLU in both monolithic and authenticated-wrapper paths; commit `2af90f7` rejects non-finite accumulators before activation and surfaces stage errors. Commander reran full workspace tests, fmt, Linux Clippy, Windows GNU check/Clippy, Semgrep (0 findings), and diff-check; feature CI `43868198001` passed Ubuntu/Windows. Independent review is pending. Scope remains CPU-only authenticated loopback dense-MLP correctness, not LLM inference, multi-machine execution, or throughput.
- No LAN/Tailscale discovery, GPU work, model loading/download, or inference workload was started; protected services were left untouched.

## Checkpoint 12 — 2026-11-16 (GUI zombie-test integrated; CPU worker-leak regression under review)

- `fix/gui-cancel-zombie-test` was rebased onto the secure-worker integration, independently reviewed with no findings, then fast-forwarded into and pushed on `integrate/fastlink-team` at `9d4f3dd7c0b39f29a811a6fdd088cc08e11f900e`. Feature CI `43870224018` and exact integration CI `43870664756` passed Ubuntu and Windows. Commander reran full workspace tests, fmt, Linux Clippy, Windows GNU check/Clippy and diff-check at the rebased candidate.
- GUI TDD RED reproduced `kill -0` reporting an unreaped zombie as alive. The test-only fix uses `/proc` state when it matches the caller namespace, otherwise a Linux pidfd fallback; a separate live-sleeping-child test preserves strict live-descendant detection and RAII cleanup. The test file is explicitly Linux-gated. Targeted Semgrep produced seven test-file heuristics: a predictable temp-dir helper confirmed pre-existing in the base, plus explicit audited libc `unsafe` sites; no production code changed.
- Independent review of CPU candidate `2af90f7` found a second P2: a non-final stage's empty error payload made the runtime return early and drop later `JoinHandle`s while their threads blocked in `accept()`. Strict TDD RED captured the leak (`NonFiniteActivation`, thread count 2→4). OpenCode fixed it in `05d7fe83f495cbf5a9d78424a174bd458ef762fd` with a private three-byte sentinel (not a valid f32 tensor), downstream sentinel relay, and error return after all stages join; the isolated Linux regression passed 10/10.
- CPU candidate `05d7fe8` is rebased on integration `9d4f3dd`, pushed, and feature CI `43871521672` passed Ubuntu and Windows. Commander reran full workspace tests, fmt, Linux Clippy, Windows GNU check/Clippy, targeted Semgrep (0 findings), and diff-check. Fresh independent review of this final candidate is pending; do not integrate until it is clean.
- CPU work remains only authenticated loopback dense-MLP correctness, not LLM, multi-machine generation, or throughput evidence. No model/GPU/server/network work was started; `main` remains unchanged. Secure-worker production residuals from Checkpoint 11 remain open.

## Checkpoint 13 — 2026-11-16 (CPU dense-MLP loopback foundation integrated)

- Final CPU feature branch `feat/cpu-stage-inference-proof` at `69cdcf757fbd1d2105cb997d9c1577850d5733b7` was independently re-reviewed with no findings and fast-forwarded into `integrate/fastlink-team`. Feature CI `43872823024` and exact integration CI `43873323917` passed Ubuntu and Windows. The CPU source was fast-forwarded at `69cdcf7`; `main` remains `8cd1b281a545f1bb0806904c8ae97902d6b62f9b`.
- Commander reran workspace/all-targets tests, fmt, Linux Clippy, Windows GNU check/Clippy, targeted Semgrep (0 findings), and diff-check at the rebased candidate. The isolated Linux thread-leak regression had strict RED (safe error returned but thread count rose 2→4) and GREEN after the private three-byte sentinel relay; 10/10 focused repeats passed.
- The final independent review confirmed prior CPU P2s closed: final-stage errors are surfaced before decode, and non-final CPU errors relay through downstream stages so their listener threads join. Residual: broader failures in the generic runtime before the join loop are outside this CPU-specific change and remain a separate runtime-hardening area.
- Scope is still only CPU-authenticated loopback dense-MLP correctness, not LLM inference, multi-machine execution, or throughput. No model/GPU/server/network work was started. Secure-worker production residuals from Checkpoint 11 remain open; no production-security claim.

## Checkpoint 14 — 2026-11-16 (final-push audit; security and CI work dispatched)

- Baseline audited: `integrate/fastlink-team` is clean at `acf85df2b65e9b74c4e0553d476022737c6c55fa`; exact integration CI `43874530591` passed Ubuntu/Windows. Remote `main` remains `8cd1b281a545f1bb0806904c8ae97902d6b62f9b`; no main merge. Do not touch the separate dirty local-main worktree.
- Enumerated all 44 Git worktrees and branch refs. Five pre-existing worktrees contain modified/untracked content (detached root, V4 QA artifacts, planner CLI, local-main tests, llama-RPC timeout edit); all were preserved. No blanket cleanup/reset was performed.
- GitHub open PR audit: #3 docs-only and #4 RPC/cache transport have no submitted reviews; their displayed checks pass but are old. #5 is a large draft V4 implementation with no submitted review. #8 targets the V4 branch, has a base conflict and a Windows failure (`adaptive_selector_picks_lower_cost_codec_per_workload`, expected LZ4/ZSTD but got RAW); OpenCode is fixing that PR branch only. None were merged to main.
- Herdr audit: FASTLINK agents in the existing `wV` workspace were assigned isolated FASTLINK work. Other Herdr workspaces were left untouched. The selected Kanban board is NightKnight Player, not FASTLINK; it was left unchanged.
- Security work: AGY is implementing zeroize-on-drop custody for the extra `PairingRegistry` secret copy on `feat/pairing-secret-hardening`, with strict TDD and no new dependency. This does not close the separate OS-keyring, no-echo pairing/network-exchange, or `snow` audit gaps.
- Protected-workload boundary: read-only status showed existing local generation/inference work active and it was left untouched. No new GPU/model inference or live network discovery was started. Such work still requires explicit approval.
- Independent read-only audits are in progress for PRs #3/#4/#5 and generic `node_runtime` failure paths. Continue to keep `main` gated on independent review, integration tests, exact-head Linux/Windows CI, and a release-safety decision.

## Checkpoint 15 — 2026-11-16 (legacy PR blockers recorded; runtime hardening dispatched)

- The audit checkpoint commit `a2a5b27ee732b24b83eabceecdeb304cfaaa41f5` passed exact-head Ubuntu/Windows CI `43882412272`. Remote `main` remains `8cd1b281a545f1bb0806904c8ae97902d6b62f9b`; no feature PR was merged.
- Independent read-only review found PR #3 publishes private machine/model/hardware/tunnel details in committed docs (P1); PR #4 has an unbounded-by-practical-resource receive allocation (P1) plus unenforced cache quota, false async/keep-alive claims, unchecked ACK, formula-only benchmark and non-shared Windows mmap fallback (P2); draft PR #5 has worker-unscoped tensor residency (P1), operation timeouts misrepresented as attempt deadlines (P2), and a non-standard codec labeled ZSTD (P2).
- Review comments were posted and read back on PRs #3, #4 and #5 (comment IDs `6919068250`, `6919068813`, `6919069307`). PR #5 remains draft. None of these branches is eligible for integration until fixes, independent re-review and exact-head CI.
- Generic runtime audit found driver-side early returns and first-worker-error propagation can detach sibling listeners; empty input can leave all stage threads blocked in `accept()`. OpenCode is implementing deterministic TDD coverage and coordinated shutdown on isolated branch `fix/runtime-failure-join` from the current integration head.
- AGY's PairingRegistry zeroize-on-drop commit `77f5bec6e160cd0e83a2e14a0012d2f999186d75` is pushed; commander-run Linux workspace tests, fmt/Clippy, Windows GNU check/Clippy and targeted Semgrep passed. Its exact feature CI is still running and independent review is pending; no integration yet.
- PR #8 Windows test fix is pushed at `24a39d637a59d913d4e0e722142700ebc7a4fb6c`; exact-head Linux/Windows CI runs `43882189493` and `43882179475` passed and GitHub reports it mergeable. Independent review remains pending; no merge to main or integration.
- No GPU/model inference or live network discovery was started; existing protected workloads remained untouched.

## Checkpoint 16 — 2026-11-16 (PairingRegistry secret copy hardened and integrated)

- Feature commit `c9698f204c48306f21776a3bf4e5cd8905dedf24` rebased cleanly onto integration, was independently reviewed with no findings (reviewed code commit `77f5bec`), and fast-forwarded into `integrate/fastlink-team`. Feature CI `43885822332` and exact integration CI `43886390304` both passed Ubuntu/Windows.
- Commander reran workspace/all-target tests, fmt, Linux Clippy, Windows GNU check/Clippy, diff-check and targeted Semgrep (0 findings) at the rebased code head. `RegistrySecret::drop` explicitly zeroizes its backing array; constructor-local copies are zeroized as well.
- Scope is limited to the retained PairingRegistry copy. Residual: caller-owned/temporary copies and compiler/register remnants are not guaranteed erased; OS-keyring storage, no-echo pairing UX/network exchange, Noise internal-state zeroization and `snow` supply-chain review remain open. This is not a production-security declaration.
- Remote `main` remains `8cd1b281a545f1bb0806904c8ae97902d6b62f9b`; no main merge. No model/GPU inference or live discovery was performed.

## Checkpoint 17 — 2026-11-16 (PR #8 follow-up coverage gap)

- Independent review of PR #8 head `24a39d637a59d913d4e0e722142700ebc7a4fb6c` found a P2 test-coverage gap: the robust tiled fixture now exercises adaptive selection, while the original tensor-like fixture is only round-tripped. The finding was posted and read back as PR comment `6919477540`.
- AGY is restoring a separate selector exercise for the original fixture in isolated branch `fix/pr8-original-tensor-coverage`; the robust fixture's strict LZ4/ZSTD assertions must remain unchanged. The original fixture may legitimately choose RAW under runtime-measured CPU costs.
- PR #8 remains open and unmerged pending follow-up, independent re-review, and exact-head CI. `main` remains unchanged; no GPU/model/server/network work was started.

## Checkpoint 18 — 2026-11-16 (release-safe Linux M1 candidate prepared)

- Created a clean release branch from current `main` (`8cd1b281a545f1bb0806904c8ae97902d6b62f9b`) and selectively ported only passive local inventory, the pre-alpha Linux desktop shell, and desktop packaging. Candidate head: `f4154ab7e01189a40a9c668968b1d1711b097f91`.
- The candidate excludes LAN/Tailscale discovery, secure worker, distributed inference, llama.cpp/GPU execution, and V4 experimental code. Local Linux workspace tests, fmt, Clippy, GUI build, Windows GNU check/Clippy, and diff-check passed; no GUI window or GPU workload was launched.
- Draft PR #12 targets `main`; exact-head GitHub CI `43891839756` passed Ubuntu/Windows. PR #12 remains draft pending independent review and Semgrep triage (six candidate matches in Unix pipe syscalls, non-Unix temp-file fallback, and CLI argument parsing).
- No changes have been merged to `main`; do not promote PR #12 until the independent review confirms release safety and all scan findings are resolved or substantiated as false positives.

## Checkpoint 19 — 2026-11-16 (main candidate review blockers)

- Independent review of PR #12 head `f4154ab7e01189a40a9c668968b1d1711b097f91` found two P2 issues: the devices CLI reports a successful inventory on unsupported OSes despite probing Linux-only paths, and the session opt-in label implies real compute participation despite being a UI-only no-op. Findings were posted and verified in PR comment `6920376423`.
- OpenCode is addressing both in an isolated follow-up branch `fix/m1-release-p2`. Keep PR #12 draft; do not merge until tests, independent follow-up review, exact-head CI, and Semgrep triage are clean.
- `main` remains `8cd1b281a545f1bb0806904c8ae97902d6b62f9b`; no inference, GPU workload, or live discovery was started.

## Checkpoint 20 — 2026-11-16 (generic runtime shutdown/join fix integrated)

- Runtime candidate `f1276a5` was independently reviewed against `a2a5b27`: no findings. The reviewer verified driver-side early-return cleanup, listener wake-up, worker errors/panics, successful joins, and regression quality; residual: thread-leak regression is Linux-only and reviewer did not rerun Windows checks.
- Rebased the isolated branch onto latest integration as `379577f61459ba366cc3bc456826ca3564eff4cd`. Commander verified the complete workspace tests, fmt, Linux Clippy, Windows GNU workspace check/Clippy, targeted Semgrep (0 findings), and diff-check. Exact feature CI `43894615269` and exact integration CI `43895224086` both passed on Ubuntu and Windows.
- Fast-forward integrated and pushed to `integrate/fastlink-team`. Remote `main` remains `8cd1b281a545f1bb0806904c8ae97902d6b62f9b`; no GPU/model inference, live discovery, or service interruption occurred.

## Checkpoint 21 — 2026-11-16 (PR #12 follow-up review and Semgrep triage)

- The PR #12 follow-up candidate `19fc8e6030331dad07e3b8a189545c85ee0fd5a5` passed commander-run Linux workspace tests/FASTLINK_BIN, fmt, Clippy, GUI build, Windows GNU check/Clippy, and diff-check; exact branch CI `43895251495` is green. The non-Linux false-success inventory issue is closed.
- Independent re-review found one remaining P2: stale “Opted in this session” text in `crates/fastlink-gui/src/ui.rs:765` and “Session-only consent” wording in `docs/GUI.md:259` still imply operational consent. Finding posted and verified in PR comment `6921143930`.
- Full Semgrep over the M1 candidate reports eight candidate matches across unsafe pipe syscalls, temporary-file paths, and CLI argument parsing. OpenCode is triaging/hardening them plus the stale copy on isolated branch `fix/m1-semgrep-tempfiles`; PR #12 is not updated or merged yet.
- `main` remains `8cd1b281a545f1bb0806904c8ae97902d6b62f9b`. No GUI window, live discovery, model/inference, GPU work, or service interruption.

## Checkpoint 22 — 2026-11-16 (keyring storage reviewed; P2 fixes dispatched)

- Experimental keyring candidate `e41e88e96ddaa72336c75ca25ca0d79b9e16514b` was pushed on `feat/secure-keyring-storage`; commander verified full Linux workspace tests/FASTLINK_BIN, fmt, Clippy, GUI build, Windows GNU check/Clippy, diff-check, and targeted Semgrep (0 findings). Exact feature CI `43904715725` passed.
- Independent security review found two P2s: when the legacy identity file is absent, a keyring private key is not checked against persisted `identity_public`; and `remove_peer` ignores failure to persist `trusted=false`, with an early-return path before keyring deletion. AGY is fixing both with deterministic TDD on isolated branch `fix/secure-keyring-p2`; do not integrate until re-review and exact CI.
- Tests use injected in-memory keyring backends; no live OS keyring, network discovery, model, GPU, or service activity occurred. The candidate remains experimental and makes no production-security claim. `main` remains `8cd1b281a545f1bb0806904c8ae97902d6b62f9b`.

## Checkpoint 23 — 2026-11-16 (keyring test-seam API gate)

- Commander code inspection found a third keyring-slice issue against the task's explicit API constraint: `KeyringBackend`, `KeyringLocator`, and `TrustStore::open_with_keyring_backend` are `pub` (only `#[doc(hidden)]`), so production callers can select an arbitrary alternate backend. This must not be a default-build public escape hatch for tests.
- AGY has been instructed to restrict the injection API to a non-default test-support feature or private test seam, keep the in-memory integration tests executing in CI, and fold this into isolated branch `fix/secure-keyring-p2` with the two independent-review P2 fixes. Do not integrate until all findings, fresh review, and exact CI pass.
- `main` remains unchanged; no live keyring, network, model/inference, GPU, or service activity.

## Checkpoint 24 — 2026-11-16 (M1 candidate temp-file and consent wording follow-up)

- OpenCode's isolated M1 follow-up `fix/m1-semgrep-tempfiles` is at `1aab6d5a079052d62be50a6acda131f428d81df4`. It fixes the remaining session-marker footer/docs wording and replaces both custom predictable test temp directories plus the non-Unix capture fallback with RAII `tempfile` handling. A real parallel-test ETXTBSY flake found during verification was fixed by serializing the actual-binary contract test under the existing script-test mutex; 20 focused repeats passed.
- Exact feature CI `43906258084` passed. Commander reran FASTLINK_BIN-backed workspace tests, fmt, Linux Clippy, GUI build, Windows GNU check/Clippy, diff-check and Semgrep over all 13 Rust files changed from main. Semgrep now reports only three audited Unix pipe syscall sites and two CLI argv-parsing heuristics; no temp-dir or secret findings remain. Independent final review is pending.
- PR #12 was fast-forwarded to the reviewed candidate, marked ready, and merged without deleting the release branch. PR exact-head CI runs `43907795006` and `43907803373` passed on head `1aab6d5a079052d62be50a6acda131f428d81df4`; the independent full-diff review found no findings. Merge commit is `3606637ca41d07f60562d536a8b9559f751c1f24` (merged 2026-11-16 16:33:27 UTC). Exact merge-commit CI `43908805208` is still in progress; await its result before calling the main release gate fully green.
- `main` now points to `3606637ca41d07f60562d536a8b9559f751c1f24`. No GUI window, live discovery/network scan, model/inference, GPU workload, or service interruption occurred.

## Checkpoint 25 — 2026-11-16 (M1 merge and exact main CI verified)

- Exact merge-commit CI `43908805208` succeeded on Ubuntu and Windows for `main` commit `3606637ca41d07f60562d536a8b9559f751c1f24` (PR #12, merged 2026-11-16 16:33:27 UTC). PR state is verified `MERGED`; the release branch was retained.
- Integration documentation checkpoint `d4b4a65b820c8e91d9a7e264894270aeadf9387e` passed exact Ubuntu/Windows CI `43909097463`. `main` and integration both have verified green CI at their recorded heads.
- Remaining work is experimental keyring/pairing security and larger inference/transport tasks; none were included in this M1 merge. No GPU inference, model load, live discovery, or service interruption occurred.

## Checkpoint 26 — 2026-11-16 (keyring security follow-up candidate pushed)

- Follow-up commit `60c2fa149bad3f575d37f7fb83353e866c0b7b9e` is pushed on isolated branch `fix/secure-keyring-p2`, based on keyring candidate `e41e88e`. It derives/verifies the static public identity on keyring reopen, hardens removal failure ordering/propagation, and makes the injected backend seam private in production builds with compile-fail API tests.
- Commander verified 244 workspace tests, five doctests, FASTLINK_BIN, fmt, Linux Clippy, GUI build, Windows GNU check/Clippy, diff-check and targeted Semgrep (0 findings). Exact feature CI `43910640643` is running; independent security re-review `deleg_371f57ab` is pending.
- Do not integrate until re-review and exact CI are green. No live Secret Service/keyring, network discovery, model/inference, GPU work, or service interruption occurred; no production-security claim.
