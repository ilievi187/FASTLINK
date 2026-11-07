# QA_REPORT — FASTLINK Milestone 1 (independent validation)

**Role:** HERETIC autonomous QA agent
**Branch:** `agent/heretic-m1-hardening`
**Base audited:** `a3b599f` (M1 correctness hardening)
**HEAD at report time:** `08fe738` (QA additions)
**Scope:** independent validation of Milestone 1 + performance/stress baseline
before Milestone 2. All work local, CPU-only, loopback-only. No push, no merge,
no PR, no deploy.
**Constraint adherence:** no GPU, no paid APIs, no external browsing; the running
llama.cpp RPC / Heretic / jcode sessions were left untouched; all work bounded by
timeouts and resource limits.

---

## 1. Verdict

**MILESTONE 1 IS SOUND.** The core delta-sync engine is correct, memory-safe,
atomic, and lossless across the tested envelope. **No new correctness bugs were
found** by independent QA. 47/47 tests pass, 8/8 benchmark runs verify
byte-exact, and 6/6 stress scenarios pass. Recommend proceeding to Milestone 2
with the caveats in §8.

## 2. Test suite (independently re-run)

All four checks re-run independently on this branch, all green:

| Check | Command | Result |
|---|---|---|
| Format | `cargo fmt --check` | clean |
| Build | `cargo build` | ok |
| Lint | `cargo clippy --all-targets -- -D warnings` | clean |
| Tests | `cargo test --all-targets` | **47 passed, 0 failed, 0 ignored** |

Test breakdown:
- **33** library unit tests (`src/lib.rs`)
- **14** loopback integration tests (`tests/protocol.rs`)
  - 10 pre-existing M1 tests (cold, warm, single-chunk delta, corrupt chunk,
    truncated transfer, malformed manifest, invalid filename, handshake magic
    mismatch, loopback guards, failed-transfer-prior-output-preserved)
  - 4 added by this QA pass (see §3)

**Skipped / unavailable:** none.

## 3. Coverage gaps found and closed (Phase 2)

The mission checklist required coverage that the existing suite lacked. Added to
`tests/protocol.rs` (all loopback, all passing):

| Test | What it locks in |
|---|---|
| `zero_byte_file_transfers_and_reconstructs` | 0-byte file round-trips to a 0-byte output; 0 chunks, 0 bytes moved |
| `concurrent_transfers_all_byte_identical` | 5 parallel clients, distinct 2-chunk files, shared cache — all outputs byte-exact |
| `duplicate_chunks_transferred_once` | two identical 64 KiB chunks → 1 transferred, 1 reused, exactly 1 chunk cached (content dedup) |
| `mid_chunk_disconnect_leaves_no_output` | TCP dropped mid-payload → no output committed |

Plus a `ConcurrentServer` helper (thread-per-connection accept loop, joins
dispatcher + workers) so integration tests exercise the real concurrency path.

**Integrity method:** byte-exact `assert_eq!(received, expected)` — strictly
stronger than any hash. System `sha256sum` used independently in the bench/stress
harnesses (§4, §5). (A hand-rolled SHA-256 was drafted for in-test use, found to
be algorithmically wrong, and removed entirely — see §8 "process note".)

## 4. Benchmarks (Phase 3) — see `BENCHMARKS.md`

8 runs (4 sizes × cold/warm), all sha256-verified byte-exact:

| Size | Cold wall | Cold MB/s | Warm wall | Warm MB/s | Server RSS |
|---|---:|---:|---:|---:|---:|
| 1 KB | 135 ms | 0.008 | 43 ms | 0.024 | ~2.9 MB |
| 1 MB | 55 ms | 18.2 | 2.8 ms | 353 | ~3.0 MB |
| 100 MB | 5.4 s | 18.6 | 279 ms | 358 | ~3.0 MB |
| 1 GB | 51.2 s | 20.0 | 3.4 s | 301 | ~3.1 MB |

**Key observations:**
- Memory is **flat (~3 MB)** from 1 KB to 1 GB — true streaming, no whole-file
  buffering, no leak.
- Cold ≈ 19–20 MB/s (CPU-bound on BLAKE3 chunk hashing, both sides).
- Warm ≈ 300–360 MB/s (0 bytes moved; still re-reads + re-verifies each cached
  chunk).
- 1 GB = 16,384 chunks, well under the 1,000,000-chunk / 64 GiB cap.

## 5. Stress (Phase 4) — all 6 PASS

| Scenario | Result | Evidence |
|---|:--:|---|
| repeated (20 × 10 MB) | PASS | 0 failures; RSS 2,996 kB → 2,996 kB (**Δ 0 kB, no leak**) |
| concurrent (8 × 10 MB distinct) | PASS | all 8 outputs byte-exact; server alive |
| same_name (8 × 10 MB, sequential) | PASS | idempotent re-send; output byte-exact; server alive |
| cancel (kill mid-transfer of 200 MB) | PASS | server alive, **no partial output**, accepts follow-up |
| restart (kill + restart same root) | PASS | pass 2 warm (0 transferred) + byte-exact |
| manyfiles (40 × 1 MB) | PASS | 0 failures, all byte-exact |

## 6. Bugs found & fixed (Phase 5)

**New code bugs: 0.** No defect in the M1 engine required a fix.

Findings recorded (not regressions, no code change):

- **F1 — CLI `serve` is sequential, not concurrent.** The real
  `serve()` loop joins each session thread before accepting the next
  (`src/main.rs:48-59`), so multiple clients are served one at a time. This is
  already listed in `KNOWN_LIMITATIONS.md` ("Single-connection server
  serializes clients"). The library (`server_sync`) *does* support concurrent
  sessions (proven by `concurrent_transfers_all_byte_identical`); only the
  shipped CLI serializes them. **Recommendation:** keep the library concurrency;
  decide in M2 whether the CLI should accept concurrently (it can, by not
  joining inline).
- **F2 — Same-filename concurrent materialization is serialized by the shared
  in-progress path.** `.fastlink-inprogress-<name>` is per-filename; two truly
  concurrent sessions materializing the *same* name would serialize on
  `exclusive_create`. Because the CLI is sequential (F1), this is not reachable
  in the shipped binary today, but it becomes relevant if F1 is changed.
  Documented as a design note for M2.
- **F3 — Cache grows unbounded.** No eviction/quota (already in
  `KNOWN_LIMITATIONS.md`). Over a long session the cache will keep every unique
  chunk ever received.

## 7. Security & safety (independent re-check)

- Server refuses non-loopback bind; client refuses non-loopback target
  (`validate_listen_address` / `validate_connect_address`) — covered by tests.
- Symlink escapes blocked on both cache and output paths
  (`ensure_not_symlink`, `output_is_safe`) — covered by tests.
- Failed/truncated transfers never delete or replace a committed output
  (distinct in-progress name + `create_new` + atomic `rename`) — covered by tests.
- No auth/encryption (pre-alpha, loopback-only) — documented limitation.

## 8. Remaining risks / caveats

- **Sequential CLI (F1/F2)** is the main concurrency caveat for M2.
- **Unbounded cache (F3)** — needs quota/eviction before any long-running use.
- **No auth/TLS** — loopback-only by design today; do not expose the port.
- **CPU-bound cold path** (~20 MB/s) is the realistic throughput ceiling on this
  host with incompressible data; warm path is much faster.
- **No fuzzer** for untrusted streams yet (bounds checks are a first layer).
- **CPU metric is server-side only**; client-side BLAKE3 cost is inside
  wall-clock but not separately itemized in `BENCHMARKS.md`.
- Benchmarks are **single-run, loopback, incompressible data** — not a
  cross-machine or networked measurement.

**Process note (QA tooling, not product code):** an in-test hand-rolled SHA-256
was drafted and found to produce wrong digests (cross-checked against Python
`hashlib` and an independent reimplementation — the transcription was
algorithmically off). It was **removed entirely**; integrity relies on byte-exact
`assert_eq!` (stronger) in Rust tests and the system `sha256sum` binary in the
bench/stress harnesses. No product code was ever affected.

## 9. Deliverables

- `tests/protocol.rs` — +4 integration tests + `ConcurrentServer` helper
- `qa/bench.py` — benchmark harness (cold/warm, sizes, CPU/RSS sampling, sha)
- `qa/stress.py` — stress harness (6 scenarios, all sha-verified)
- `qa/results/bench_summary.json`, `qa/results/stress_summary.json` — raw data
- `BENCHMARKS.md` — measured benchmark tables
- `QA_REPORT.md` — this report
- `PROGRESS.md` — appended QA entry

## 10. Reproduce

```bash
cd FASTLINK-heretic-agent
cargo build --release
cargo test --all-targets          # 47 pass
python3 qa/bench.py               # 8 runs (1GB cold ~51 s)
python3 qa/stress.py              # 6 scenarios
```

## 11. Recommendation for Milestone 2

**Proceed.** M1 is correct, lossless, memory-flat, and atomic. Before/alongside
M2:
1. Decide the CLI concurrency model (F1) and, if concurrent, make
   same-name materialization safe (F2) — add a unique in-progress path per
   session (e.g. include PID + a counter) rather than per-filename.
2. Add cache quota/eviction (F3).
3. Add a stream fuzzer for the wire protocol.
4. Keep the existing byte-exact + sha256sum integrity checks; they are the
   strongest guards in the suite.
