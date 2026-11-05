# FASTLINK — agent work instructions

You are contributing to an independent Apache-2.0 FOSS protocol project. This document is authoritative for code and research tasks inside this repository.

## Mission

Improve distributed compute performance by **reducing unnecessary data movement**, keeping verified reusable objects near their consumer, applying lossless/adaptive compression when it reduces wall-clock time, and avoiding unnecessary synchronization. Measure application throughput, not only compression ratio.

## Begin with the actual state

- The repository contains a working CPU-only local transfer prototype.
- The Rust CLI and protocol are *not* yet a virtual GPU, a GPU RPC backend, or an official WiCi protocol.
- Follow `docs/PROTOCOL.md`, `docs/RESEARCH.md`, `docs/BENCHMARKS.md`, and `docs/ROADMAP.md`.
- Respect upstream licenses and acknowledge publicly documented ideas; do not reproduce non-public WiCi or third-party internals.

## Immediate development loop

1. Read all documentation and inspect the code/tests.
2. Implement the next safe item from `TASKS.md` on a short-lived branch.
3. Add tests for both normal and malformed frames.
4. Run `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test --all-targets`.
5. Benchmark end-to-end with cold/warm caches and incompressible data.
6. Update docs and make a reviewable pull request or commit.

## Safety and privacy

- Do **not** stop or restart the user's existing Heretic 256K, llama.cpp RPC, GPU Task Manager, or unrelated video/AI services.
- Do **not** access or publish private GPUs, internal network addresses, machine paths, SSH keys, tokens, private logs, or user model weights.
- Run CPU-only local tests by default. Real GPU tests require explicit owner approval.
- Never expose the experimental unauthenticated TCP port beyond loopback.
- Never spend paid API credits or introduce heavyweight model downloads without explicit approval.

## Implementation priorities

Authenticated encrypted transport, cache quotas/eviction, Zstd/LZ4 codec selection with honest benchmarking, bounded parsing/fuzz tests, a safe llama.cpp-compatible adapter, and GPU telemetry. Document trade-offs and regressions.

## Reporting

Always state what is proven, measured, and still experimental. Keep `PROGRESS.md` updated. Do not promise performance gains before equivalent tests support them.
