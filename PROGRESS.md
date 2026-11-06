# Development progress

## 2026-11-06 — M1 (issue #1: correctness hardening)

Branch: `agent/heretic-m1-hardening` (committed locally only; not pushed, no PR, no merge).

**What was done**
- Moved the entire session engine (handshake, client + server SYNC, content-addressed
  cache, atomic output materialization) from `src/main.rs` into the library so it can be
  exercised by integration tests over a real loopback TCP connection. `src/main.rs` is now
  a thin CLI shell over `fastlink::{client_sync, server_sync}`.
- Added `tests/protocol.rs` (10 integration tests, all on `127.0.0.1`):
  - cold transfer, warm transfer (cache reuse, 0 bytes moved), single-chunk delta
    (only the missing chunk transfers), corrupt chunk digest (rejected, no output),
    truncated transfer (no partial output), malformed manifest frame (rejected, no output),
    invalid/traversal filename (rejected client-side, no output), handshake magic mismatch
    (rejected), loopback-only bind + connect guards, and **a failed transfer never replaces
    a previously completed output**.
- Added ~30 more unit tests in `src/lib.rs` covering chunk bounds, RLE/raw codec edge cases,
  `safe_basename`, loopback address guards, `in_progress_path` collision safety,
  `materialize_file` (atomic replace, missing chunk, collision-name failure preserves prior
  output, symlink output rejected), `read_manifest` bounds, `read_chunk` index/digest/size
  checks, and the versioned handshake.

**Real correctness bugs found by the tests and fixed**
1. **Failed transfer deleted a previously committed output.** The old
   `Path::with_extension("fastlink-partial")` in-progress path collided with the final
   output for names like `x.fastlink-partial`; combined with truncate-on-create +
   remove-on-failure this deleted the prior output. Fixed by giving the in-progress file a
   guaranteed-distinct name (`.fastlink-inprogress-<name>`, same directory) and opening it
   with `create_new` so an existing file is never truncated; on failure only the private
   file is removed, never the committed output. Covered by
   `materialize_collision_name_failure_preserves_prior_output` (unit) and
   `failed_transfer_never_replaces_prior_output` (integration).
2. **Symlink escape in the data root.** Cache and output paths now refuse symlinks
   (`ensure_not_symlink`, `output_is_safe`) so a planted link cannot redirect I/O outside
   the cache/output root. Covered by `materialize_symlink_output_rejected`.
3. **Non-exhaustive / wrong control flow in `materialize_file`** in-progress metadata
   handling (compile-time defect) and a `char`/`u64` cast error in `client_sync` — both fixed.
4. **Handshake was only verified one-way.** `handshake` now both sends and validates the
   peer's `FLNK`+version; a bad magic or version is rejected.

**Wire compatibility.** The v0.1 wire format (magic, version, command, manifest, missing
list, chunk payload, status byte) is unchanged. The session engine was relocated into the
library but behaves identically; no protocol change is required.

**Verification (all green, CPU-only, loopback only)**
- `cargo fmt --check` → clean
- `cargo build` → ok
- `cargo clippy --all-targets -- -D warnings` → clean
- `cargo test --all-targets` → 33 lib + 10 integration, all passed
- Server cannot bind beyond loopback; client cannot target non-loopback; rejected input
  never creates files outside the cache/output root (asserted by tests).

**Still experimental / not done**
- No authenticated or encrypted transport yet (still an unauthenticated pre-alpha
  loopback-only protocol).
- No cache quota/eviction; cache grows unbounded.
- Codec selection is still the conservative RAW/RLE heuristic; LZ4/Zstd + honest
  wall-clock benchmarking is a later milestone.
- No fuzzer for untrusted streams yet; the new bounds checks are a first layer.
- No GPU/llama.cpp RPC integration; FASTLINK remains a CPU file-sync proof of concept.

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
