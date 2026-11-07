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
- No GPU benchmarks, model inferences, remote network tests, services, profiles or launchers were changed. The live Heretic v2 baseline remains untouched.
- Branch: `feat/fastlink-v3-cpu-baseline`; worktree: `.worktrees/fastlink-v3-cpu-baseline`.
- This is only a serial cost model, not FastLink v3 inference transport. No performance claim.
