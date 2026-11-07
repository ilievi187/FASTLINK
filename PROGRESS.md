# Development progress

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

**Explicit limitation:** This does not yet accelerate GPU inference; no GPU benchmarks exist for FASTLINK's Rust transport. This log is not a measure of real RPC performance.

## 2026-11-07 — v3 isolated CPU model

- Created `qa/cost_model.py`: explicit link RTT/throughput/serialization and per-stage compute/queue/transfer calculator plus declared VRAM feasibility check. It performs no device discovery and is not GPU telemetry.
- Added twelve model tests, including finite inputs, integer RTT count, per-round-trip serialization, empty-pipeline rejection and memory declaration validation. OpenCode's read-only review found edge cases; regressions were run RED, fixed, then verified green.
- `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test --all-targets`: pass (5 Rust tests).
- OpenCode implemented `qa/network_sim.py` and 22 tests on separate branch `feat/fastlink-v3-network-sim`; orchestrator reviewed ownership and verified all tests, then integrated it to the CPU branch in merge commit `a578ecd`. Simulator models declared RTT, throughput, serialization, queue, loss, retry and disconnect; no live network was used.
- Combined Python suite: 38/38 pass. Rust fmt/clippy/test suite remains green (5 Rust tests).
- Added CPU-only localhost TCP byte-counting benchmark harness. Three repetitions each for random-cold, random-warm, one-byte-delta, and sparse-cold 1 MiB fixtures; all reconstructed SHA256s matched. Medians and stream/payload bytes are documented in `BENCHMARKS.md`; raw JSON at `qa/results/loopback-cpu-1mib-repeats-20261107.json`. These are file-sync measurements, not GPU or inference metrics.
- No GPU benchmark, model inference, live LAN experiment, service, profile or launcher was changed. Live Heretic v2 remains the untouched rollback baseline.
- This is only a serial cost model, not FastLink v3 inference transport. No performance claim.
