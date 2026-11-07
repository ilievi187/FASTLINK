# BENCHMARKS — FASTLINK Milestone 1 (QA baseline)

Independent, reproducible performance baseline captured by the HERETIC QA agent.
All runs are **CPU-only, loopback TCP (127.0.0.1), release build**, on the same
machine that produced the test suite. Numbers are measured, not estimated.

## Environment

| Item | Value |
|---|---|
| Host | NightKnightPC, Linux x86-64 |
| Cores | 16 (no GPU used; no llama.cpp/RPC interference observed) |
| RAM | 30 GiB total |
| Free disk at test time | 24–29 GiB (fixtures generated under `qa/fixtures`) |
| Build | `cargo build --release` (`target/release/fastlink`) |
| Protocol | v0.1 wire, 64 KiB chunks, BLAKE3 content-addressed cache |
| Fixtures | Incompressible `os.urandom` data (worst-case for RLE, realistic for tensors) |
| Integrity | Independent `sha256sum` of source vs. materialized output (all matched) |

Harness: `qa/bench.py`. Per run it (1) starts `fastlink serve` on a fresh cache
root (cold) or the cold root (warm), (2) runs `fastlink send`, (3) verifies the
output sha256, and (4) samples `/proc/<pid>/stat` + `/proc/<pid>/status` for
server CPU time (fields 14/15, survives worker-thread exit) and RSS (VmRSS).

> **What "CPU" and "RSS" measure:** the **server** process only. The short-lived
> `send` client process (which also runs BLAKE3 over the source) is not
> separately instrumented; wall-clock time includes both sides.

## Cold transfer (fresh cache — every chunk transferred)

| Size | Chunks | Wall | Throughput | Server CPU | Server RSS | sha |
|---|---:|---:|---:|---:|---:|:--:|
| 1 KB | 1 | 134.7 ms | 0.008 MB/s | < 10 ms | 2.90 MB | OK |
| 1 MB | 16 | 55.1 ms | 18.2 MB/s | < 10 ms | 3.00 MB | OK |
| 100 MB | 1,600 | 5,377 ms | 18.6 MB/s | 380 ms | 3.02 MB | OK |
| 1 GB | 16,384 | 51,165 ms | 20.0 MB/s | 3,750 ms | 3.08 MB | OK |

## Warm transfer (cache warm — 0 bytes moved)

| Size | Chunks reused | Wall | Throughput | Server CPU | Server RSS | sha |
|---|---:|---:|---:|---:|---:|:--:|
| 1 KB | 1 | 42.5 ms | 0.024 MB/s | < 10 ms | 2.88 MB | OK |
| 1 MB | 16 | 2.83 ms | 353 MB/s | < 10 ms | 2.96 MB | OK |
| 100 MB | 1,600 | 279 ms | 358 MB/s | 30 ms | 3.02 MB | OK |
| 1 GB | 16,384 | 3,406 ms | 301 MB/s | 610 ms | 2.98 MB | OK |

## Findings

- **Memory is flat.** Server RSS holds at ~2.9–3.1 MB across 1 KB → 1 GB. The
  engine streams chunks one at a time (bounded by `MAX_ENCODED_BYTES`, 128 KiB);
  it never buffers the whole file. No leak observed in the 20-iteration stress
  run (RSS first 2,996 kB → last 2,996 kB, Δ 0 kB).
- **Cold throughput ≈ 19–20 MB/s**, dominated by BLAKE3 hashing of each 64 KiB
  chunk (done by both client and server) plus cache writes. Throughput is
  roughly constant with size, i.e. CPU-bound, not size-bound.
- **Warm throughput ≈ 300–360 MB/s** — 0 bytes transferred, but each chunk is
  still re-read from the cache and re-verified (BLAKE3) before materialization,
  so warm is I/O + verify bound, not free.
- **Cold → warm speedup ≈ 15×** at 1 GB (51.2 s → 3.4 s).
- **1 GB is well within the protocol cap** (16,384 chunks vs. `MAX_CHUNKS`
  1,000,000 / 64 GiB).

## Caveats

- Single-session, single-file, loopback. No network latency, no multi-client
  saturation (see `stress.py`), no disk under contention.
- Incompressible fixtures; real tensors may compress better via the RLE path,
  changing the byte count (not the hashing cost).
- Reported CPU is server-side only; the client-side BLAKE3 cost is inside
  wall-clock but not separately itemized.
- One run each (not averaged). Re-run `python3 qa/bench.py` to reproduce.
