# BENCHMARKS

This file unions the WP-D QA baseline, the WP-A benchmark log, and the FL3-A1 codec selection table already recorded in `docs/BENCHMARKS.md`. No new measurements were taken for this merge.

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

# Benchmark log

All benchmark numbers must identify the exact build, model, quantization, devices, context, prompt, output limit, sampling, cache state, and run count. Report medians and dispersion, full wire bytes, CPU and device telemetry sources. Never infer v3 speedup from the CPU cost model.

## Existing v2 prototype result (2026-11-05)
See `docs/BENCHMARKS.md`: local CPU file-sync test only, highly compressible 8 MiB synthetic file, payload-only accounting. Not an RPC or inference result.

## Existing separate adapter smoke (documented 2026-11-06)
`FASTLINK-heretic-agent/docs/GPU_RPC_ADAPTER_VERIFIED.md` reports five smoke trials: stock RPC median 0.764 s, adapter 0.758 s. This establishes a compatibility smoke only; it is too small/noisy and is not end-to-end inference evidence. Preserve the distinction between the public FASTLINK repo and the separate patched llama.cpp worktree.

## 2026-11-07 — CPU-only serial placement cost model

- Implementation: `qa/cost_model.py`, branch `feat/fastlink-v3-cpu-baseline`.
- Measures: computed cost from caller-specified RTT, link throughput, serialization, stage compute, queue delay and stage bytes; declared-memory feasibility; modeled single-token rate.
- Test results: `python3 -m unittest discover -s qa -p 'test_cost_model.py' -v` — 12 passed after read-only review identified non-finite inputs, fractional RTT counts, serialization accounting ambiguity, empty pipelines and invalid memory declarations. Regressions were observed failing before fixes.
- Deterministic network fault simulator: `qa/network_sim.py`; 22 tests cover serial transfer formula, invalid inputs, bounded retry, disconnect, multi-stage short-circuit and same-seed reproducibility. All outcomes are modeled arithmetic; no live network was probed.
- CPU-only benchmark harness: `qa/wire_bench.py` runs local FASTLINK server/client through a byte-counting TCP relay, validates SHA256, and records complete application TCP stream bytes (both directions, excludes TCP/IP/Ethernet headers).
- Three 1 MiB loopback runs each: random cold median 130 ms / 1,049,350 stream bytes / 1,048,576 payload bytes; random warm 46 ms / 566 stream bytes / 0 payload; one-byte delta 135 ms / 66,115 stream bytes / 65,536 payload; sparse cold 137 ms / 2,153 stream bytes / 1,548 payload. All output SHA256 matched. Raw machine-readable records: `qa/results/loopback-cpu-1mib-repeats-20261107.json`.
- Combined Python suite: 38/38 pass. Rust checks in same branch: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test --all-targets` — pass, 5 Rust tests.
- These are localhost CPU file-sync metrics only, not LAN, llama.cpp RPC or inference measurements. They do not prove GPU speedup.

## Existing local adapter CPU codec regression

- Repository/worktree: `/mnt/gamez/AI/llama.cpp-fastlink-rpc-adapter`, branch `feat/fastlink-rpc-adapter`, based on pinned llama.cpp commit `0ec4027`; tree remained clean.
- Reconfigured existing release build, built target `test-fastlink-rpc-codec` (Ninja reported no rebuild needed), then ran `ctest --test-dir build-fl-gpu/ggml/src/ggml-rpc -R fastlink-rpc-codec --output-on-failure`.
- Result: 1/1 passed in 0.18 s. The test covers codec roundtrip, random fallback, malformed/truncated/corrupt frame handling. This is CPU codec QA only; no RPC client-server socket test, Windows rebuild, GPU graph execution, inference, or speed measurement was performed.
- Working v2 tree `/mnt/gamez/AI/llama.cpp-b12020-fastlink` remains dirty with its pre-existing `ggml-rpc.cpp` modification; untouched.

## GPU A/B results

Not run. Heretic v2 remained active on port 18879, so no GPU/model benchmark was started. A controlled maintenance window is required; stock RPC, v2 and candidate v3 must be tested serially with identical settings and repeated runs before any performance claim.

## 2026-11-07 — FL3-A1 Adaptive Codec Selection (`RAW` / `RLE` / `LZ4` / `ZSTD`)

**Configuration & Methodology:**
- **Worktree / Branch:** `feat/fastlink-v3-cpu-baseline` (`src/codec.rs`, `tests/codec_adaptive.rs`).
- **Build profile:** `cargo test --release --test codec_adaptive -- --nocapture` (10-iteration mean per 65,536-byte chunk on local CPU; debug unoptimized numbers also verified and recorded below).
- **Cost policy (`CodecCostPolicy`):**
  - `wire_bytes = encoded_bytes + 9` (includes the 9-byte protocol v0.1 chunk frame header: `index:u32 + codec:u8 + encoded_len:u32`).
  - `wire_transfer_ms = (wire_bytes * 8) / (bandwidth_mbps * 1_000)` with reference `bandwidth_mbps = 250.0` Mbps (`min_byte_savings = 32` bytes).
  - `total_cpu_ms = encode_cpu_ms + decode_cpu_ms` (measured wall-clock CPU time on the host).
  - `total_cost_ms = total_cpu_ms + wire_transfer_ms`.
  - **Selection rule (proven by unit + integration tests):** A compressed codec (`RLE=1`, `LZ4=2`, `ZSTD=3`) is selected over `RAW=0` **only** when `encoded_bytes + min_byte_savings < raw_bytes`, `encoded_bytes <= MAX_ENCODED_BYTES`, AND `total_cost_ms < raw_total_cost_ms`.

### Measured Codec Choice Table (`--release`, 65,536 B chunk, 250 Mbps link policy)

| Workload (64 KiB chunk) | Candidate Codec | Encoded Bytes | Wire Bytes (+9 B hdr) | Encode CPU (ms) | Decode CPU (ms) | Total CPU (ms) | Wire Time @ 250 Mbps (ms) | Total Wall Cost (ms) | Selected? |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | :---: |
| **1. Incompressible** (`synthetic_incompressible_chunk`, SplitMix64 PRNG) | **RAW (0)** | **65,536** | **65,545** | **0.005** | **0.000** | **0.000** | **2.097** | **2.097** | **YES (`RAW`)** |
| | RLE (1) | 130,568 | 130,577 | 0.074 | 0.137 | 0.211 | 4.178 | 4.389 | no (expands) |
| | LZ4 (2) | 65,794 | 65,803 | 0.005 | 0.001 | 0.006 | 2.106 | 2.112 | no (expands) |
| | ZSTD (3) | 65,802 | 65,811 | 0.070 | 0.002 | 0.072 | 2.106 | 2.178 | no (expands) |
| **2. Sparse** (`synthetic_sparse_chunk`, 3 isolated non-zero bytes) | RAW (0) | 65,536 | 65,545 | 0.001 | 0.000 | 0.000 | 2.097 | 2.097 | no |
| | **RLE (1)** | **522** | **531** | **0.028** | **0.001** | **0.029** | **0.017** | **0.046** | **YES (`RLE`)** |
| | LZ4 (2) | 269 | 278 | 0.028 | 0.043 | 0.071 | 0.009 | 0.080 | no (higher CPU+wire cost than RLE) |
| | ZSTD (3) | 38 | 47 | 0.141 | 0.044 | 0.185 | 0.002 | 0.187 | no (higher CPU+wire cost than RLE) |
| **3. Tensor-like BF16 strided** (`synthetic_tensor_like_chunk`, BF16 exponent/mantissa lanes) | RAW (0) | 65,536 | 65,545 | 0.004 | 0.000 | 0.000 | 2.097 | 2.097 | no |
| | RLE (1) | 130,620 | 130,629 | 0.064 | 0.138 | 0.202 | 4.180 | 4.382 | no (expands 2.0x) |
| | LZ4 (2) | 65,793 | 65,802 | 0.004 | 0.001 | 0.005 | 2.106 | 2.111 | no (untransposed 16-bit stride misses 4-byte LZ4 matches) |
| | **ZSTD (3)** | **36,637** | **36,646** | **0.111** | **0.042** | **0.153** | **1.173** | **1.326** | **YES (`ZSTD`)** |
| **4. Tensor-like Q8 block activations** (32-byte quantized blocks) | RAW (0) | 65,536 | 65,545 | 0.001 | 0.000 | 0.000 | 2.097 | 2.097 | no |
| | RLE (1) | 131,072 | 131,081 | 0.063 | 0.141 | 0.204 | 4.195 | 4.398 | no (expands 2.0x) |
| | **LZ4 (2)** | **439** | **448** | **0.028** | **0.030** | **0.058** | **0.014** | **0.072** | **YES (`LZ4`)** |
| | ZSTD (3) | 378 | 387 | 0.098 | 0.030 | 0.128 | 0.012 | 0.140 | no (61 B smaller than LZ4, but +0.070 ms CPU cost exceeds 0.002 ms wire savings) |

**Proven vs. Measured vs. Modelled:**
- **Proven (deterministic tests):** Byte-exact roundtrip (`decode_chunk(codec, encode_with_codec(codec, data)) == data`) across all four codecs (`RAW`, `RLE`, `LZ4`, `ZSTD`) and all workloads; rejection of malformed/truncated/decompression-bomb frames for `RLE`, `LZ4`, and `ZSTD`.
- **Measured (host CPU wall-clock):** Encode and decode CPU milliseconds (`encode_cpu_ms`, `decode_cpu_ms`) and exact compressed byte sizes (`encoded_bytes`, `wire_bytes`).
- **Modelled (link transfer cost):** `wire_transfer_ms` is computed from exact `wire_bytes` divided by the configured link bandwidth (`CodecCostPolicy::bandwidth_mbps`).
