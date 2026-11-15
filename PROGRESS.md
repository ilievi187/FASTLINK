# Development progress

## 2026-11-15 — Linux local-device desktop shell (`feat/linux-local-device-ui`)

- Added the `fastlink-gui` workspace crate: native eframe/egui desktop shell for the first local-device milestone.
- Reads the versioned inventory contract by invoking `<FASTLINK_BIN> devices --json` (configurable with `--fastlink-bin` or `FASTLINK_BIN`); the source command is shown in the window.
- Added device cards (hostname/id, OS, architecture, logical CPU cores, reported total/available RAM, backend, GPU and network entries), manual refresh, device selection and explicit session-only opt-in.
- Unknown or `null` inventory values render as `Unknown`; empty peer/model/chat areas are labelled as not implemented instead of being populated with synthetic data.
- Added a bounded worker invocation (20 s timeout), stale-snapshot warning on refresh failure, and honest CLI/JSON/schema error views.
- Added headless tests: JSON/contract parser, state/opt-in/selection transitions, fake-CLI command execution, and UI rendering of idle/loading/error/stale/empty/long-value screens via `egui::Context::run_ui`.
- Verified on Linux: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test --all-targets` (45 existing core tests plus 52 GUI tests) and `cargo build -p fastlink-gui`.
- Added build/run/test documentation in `docs/GUI.md` and a README section.

**Explicit limitations:** Window rendering uses OpenGL (no CUDA/ROCm compute). Linux is the only verified target. No pairing, models, chat, inference or remote controls exist yet. Inventory calls fail honestly until a FASTLINK build with `devices --json` is selected. Opt-in is session-only and never persisted.

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

## 2026-11-16 — Release candidate: passive Linux device inventory

- Added `fastlink devices [--json]` with local-only passive host inventory. Unknown capability values remain explicitly unknown; the command does not perform network discovery, start services, or run GPU compute.
- This release slice contains no LAN/Tailscale discovery, secure worker, llama.cpp inference, or performance claims.
