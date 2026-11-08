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

**Explicit limitation:** This does not yet accelerate GPU inference; no GPU benchmarks exist for FASTLINK's new Rust transport. This log is not a measure of real RPC performance.

## 2026-11-08 — O-1 adapter chunk framing

- Added `src/rpc_client.rs`: FASTLINK v0.1 64 KiB/BLAKE3 chunking with RAW/RLE
  selection, rendered as the llama.cpp adapter's FLR1/FLR3 per-chunk payload
  (`codec:u8`, `encoded_size:u32 LE`, encoded bytes).
- Added `tests/rpc_integration.rs`: byte-exact 64 KiB golden vector
  (BLAKE3 `b01eb900…bb7984`, payload `535a74d2…99d7c0`), determinism,
  multi-chunk ordering, RAW fallback, decode round-trips and malformed-payload
  rejection.
- `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings`
  and `cargo test --all-targets` pass (5 library + 5 integration tests).
- No dependency added. The adapter repository was read-only and was not built
  or modified.

**Explicit limitation:** Only the adapter's per-chunk payload is produced. The
outer FLR1/FLR3 envelope (SHA-256, original RPC header, ≥128 KiB raw tensor) is
not generated, and no adapter process was started, so this is framing
correctness — not an end-to-end RPC transfer or a performance claim.
