# Development progress

## 2026-11-06 — M1 QA baseline (independent validation + perf/stress)

Branch: `agent/heretic-m1-hardening` (committed locally only; not pushed, no PR, no merge).
Audited base `a3b599f`. See `QA_REPORT.md` (full findings) and `BENCHMARKS.md` (raw tables).

**Independent re-run (all green, CPU-only, loopback-only):**
- `cargo fmt --check` clean · `cargo build` ok · `cargo clippy --all-targets -- -D warnings` clean
- `cargo test --all-targets` → **47 passed, 0 failed** (33 lib + 14 integration)

**Coverage gaps closed (this pass):** added 4 loopback integration tests + a
`ConcurrentServer` helper — `zero_byte_file_transfers_and_reconstructs`,
`concurrent_transfers_all_byte_identical` (5 parallel clients, all byte-exact),
`duplicate_chunks_transferred_once` (dedup), `mid_chunk_disconnect_leaves_no_output`.
Integrity via byte-exact `assert_eq!` in Rust and system `sha256sum` in the
bench/stress harnesses.

**Benchmark baseline (release, incompressible fixtures, sha256-verified):**
- Cold ≈ 19–20 MB/s (CPU-bound on BLAKE3); warm ≈ 300–360 MB/s (0 bytes moved).
- 1 GB: cold 51.2 s / warm 3.4 s; **server RSS flat at ~3 MB across 1 KB → 1 GB**
  (true streaming, no leak).
- Harness: `qa/bench.py`; raw data `qa/results/bench_summary.json`.

**Stress (6/6 PASS, sha-verified):** `repeated` (20×, RSS Δ 0 kB), `concurrent`
(8 distinct), `same_name` (8 sequential), `cancel` (kill mid-transfer → no partial
output, server alive), `restart` (pass 2 warm + byte-exact), `manyfiles` (40).
Harness: `qa/stress.py`; raw data `qa/results/stress_summary.json`.

**Bugs found & fixed (M1 engine): 0.** Findings recorded (not regressions):
- F1 CLI `serve` is **sequential** (joins each session before next accept) —
  already in `KNOWN_LIMITATIONS.md`; library concurrency is proven separately.
- F2 same-filename concurrent materialization would serialize on the shared
  `.fastlink-inprogress-<name>` path — not reachable via the sequential CLI today;
  make the in-progress path per-session before enabling concurrent CLI (M2).
- F3 cache is unbounded (no quota/eviction) — already documented.

**Milestone 2 recommendation: PROCEED.** See `QA_REPORT.md` §11.

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
# Cross-machine validation — 2026-11-06

At `4a5179e`, Linux sender to Windows receiver on detti over authenticated SSH
local forwarding, both FASTLINK endpoints bound to loopback. One 16 MiB random
fixture: cold 1.8582 s, 256 chunks and 16,777,216 encoded payload bytes; warm
0.1464 s, zero chunks; one-chunk delta 0.1778 s, one 65,536-byte chunk. Source
and receiver SHA-256 matched independently after each transfer. Logical rates
were 8.61, 109.30 and 89.98 MiB/s respectively; warm/delta rates are logical
file size divided by wall time, not network throughput. Counts exclude framing,
SSH and TCP overhead. One run per case, not a general performance claim.
Temporary data, receiver and tunnel were removed. An initial detached Windows
receiver attempt failed; keeping its SSH session open allowed successful tests.
47 tests, cargo fmt and clippy passed locally. No GPU or inference was used.
See `docs/RPC_ADAPTER_PLAN.md` for the design-only next steps.

# RPC investigation — 2026-11-06

Inspected the local Heretic launcher and its pinned, locally modified llama.cpp
source. Its existing FastLink-named packet aggregation and tensor chunking are
not this project's v0.1 file protocol. Stock RPC already includes a weight hash
cache, and the source checkout has uncommitted modifications. The adapter and
production integration remain unimplemented; details and test gates are in
`docs/RPC_COMPATIBILITY_FINDINGS.md`. No services or launchers were changed.

