# Benchmark log

## 2026-11-05 — CPU-only synthetic smoke

**Configuration:** local loopback Rust CLI; 8,388,608-byte mostly-zero file with two isolated nonzero bytes; 65,536-byte chunks; BLAKE3; RAW/RLE codec; one-byte modification before third transfer.

| Scenario | Total chunks | Transferred chunks | Raw missing bytes | Encoded payload bytes | Wall-clock reported |
| --- | ---: | ---: | ---: | ---: | ---: |
| Cold transfer | 128 | 3 | 196,608 | 1,552 | 132 ms |
| Warm transfer | 128 | 0 | 0 | 0 | 50 ms |
| One-byte edit | 128 | 1 | 65,536 | 518 | 135 ms |

The reconstructed file matched SHA-256 after first and third transfers and `cmp` passed after the edit.

**Limitations:** highly compressible synthetic input; local-only test; no GPU, no remote WAN/LAN test, no traffic capture, no measurement of server CPU / cache storage overhead; counts are **encoded payload only**, excluding manifests and framing. No inference acceleration claim is supported.

## 2026-11-10 — FASTLINK-RPC/1 loopback transport latency

**Configuration:** `cargo bench --bench rpc_latency` (optimized bench profile); Linux loopback `127.0.0.1`; one TCP connection per scenario; `TCP_NODELAY` on both sockets; 64 KiB chunks; BLAKE3 content addressing; RAW/RLE codec; 8 MiB tensors (128 chunks). Latency and throughput exclude connection setup; setup is reported separately.

| Measurement | Result |
| --- | ---: |
| Single-chunk RTT (64 KiB patterned, 500 rounds) | min 0.092 ms / median 0.100 ms / mean 0.103 ms |
| 8 MiB patterned throughput (RAW) | 597.2 MB/s raw, 597.3 MB/s wire |
| 8 MiB uniform throughput (RLE) | 1,957.9 MB/s raw, 15.6 MB/s wire |
| 8 MiB patterned wire bytes | 8,389,260 B vs 8,388,608 B raw (+652 B) |
| 8 MiB uniform wire bytes | 66,700 B vs 8,388,608 B raw (−8,321,908 B) |
| Connection setup (client connect) | 0.039 ms |

The patterned case is effectively incompressible, so its wire overhead is the
12-byte tensor header plus 5 bytes per chunk (128 × 5 = 640 B). The uniform case
uses RLE and sends 66,700 B instead of 8,388,608 B; its wire throughput is low
because there is almost no network data to move, while raw throughput still
reflects framing, hashing and decode work on the full tensor.

**Limitations:** loopback only; unauthenticated and unencrypted; single
connection; no `llama.cpp` adapter process was run, so this is a transport
measurement, not an end-to-end RPC or GPU result. Numbers come from one machine
and are not a performance guarantee. `cargo test --all-targets` also executes
the bench in the debug profile and reports much slower numbers; use
`cargo bench --bench rpc_latency` for the optimized measurements.

## 2026-11-10 (refresh) — Raw TCP vs FASTLINK framed, single connection

Refreshed the single-connection `send_tensor` transport measurements and added a
raw-streaming comparison against a bare `write_all` of the same tensor. This
refresh was a historical Linux loopback run; a newer named snapshot is recorded
below. Configuration: `cargo bench --bench rpc_latency` (optimized),
`127.0.0.1` loopback, `TCP_NODELAY` on both sockets, one TCP connection per
measurement.

| Measurement | Result |
| --- | ---: |
| Single-chunk RTT (64 KiB, 500 rounds) — Raw TCP | min 0.018 ms / median 0.020 ms / mean 0.021 ms |
| Single-chunk RTT (64 KiB, 500 rounds) — FASTLINK framed (RAW) | min 0.093 ms / median 0.099 ms / mean 0.107 ms |
| 8 MiB patterned — Raw TCP streaming | 8,671.2 MB/s |
| 8 MiB patterned — FASTLINK framed (RAW) | 603.5 MB/s raw / 603.5 MB/s wire; 8,389,260 wire B vs 8,388,608 raw (+652 B) |
| 8 MiB uniform — Raw TCP streaming | 7,120.3 MB/s |
| 8 MiB uniform — FASTLINK framed (RLE) | 1,998.3 MB/s raw / 15.9 MB/s wire; 66,700 wire B vs 8,388,608 raw (−8,321,908 B) |
| Connection setup (client connect) | 0.031 ms |

Observations: the framed transport adds ~652 B of per-tensor overhead (12-byte
tensor header + 128 × 5-byte chunk headers) over the raw path for incompressible
data, matching the earlier measurement. The framed RLE case is deliberately slow
in wall-clock because decode + hashing work dominates while almost no network
data is moved. Raw TCP streaming was ~14.4× faster in patterned throughput in
that particular run; it skips framing, content addressing and decode. Pooling
amortizes connection setup, not per-tensor headers; the separate batched bench
is still WIP.

**Limitations:** loopback only; unauthenticated, unencrypted; single connection;
one machine, so not a performance guarantee. The batched `Pool` measurement is
in progress (see PROGRESS.md).

## 2026-11-10 22:48 +02:00 — Canonical loopback snapshot

Snapshot ID: `rpc-latency-linux-2026-11-10-224820-8fc776c`; machine-readable
results: [`benches/results_rpc_latency.json`](../benches/results_rpc_latency.json);
raw stdout: [`benches/results_rpc_latency_20261110.txt`](../benches/results_rpc_latency_20261110.txt).
This run used Linux x86_64, AMD Ryzen 7 7800X3D, rustc 1.99.0, source commit
`8fc776cfd49389abfd54160b1819369aa1361038`.

| Measurement | Result |
| --- | ---: |
| Single-chunk RTT, 64 KiB, raw TCP (min / median / mean) | 0.016 / 0.018 / 0.019 ms |
| Single-chunk RTT, 64 KiB, framed RAW (min / median / mean) | 0.093 / 0.102 / 0.108 ms |
| 8 MiB patterned raw TCP throughput | 8,605.5 MB/s |
| 8 MiB patterned FASTLINK RAW (raw-equivalent / wire) | 564.9 / 565.0 MB/s |
| Patterned wire bytes per tensor (raw / FASTLINK) | 8,388,608 / 8,389,260 B (+652 B, +0.0078%) |
| 8 MiB uniform raw TCP throughput | 7,758.1 MB/s |
| 8 MiB uniform FASTLINK RLE (raw-equivalent / wire) | 1,869.6 / 14.9 MB/s |
| Uniform wire bytes per tensor (raw / FASTLINK) | 8,388,608 / 66,700 B (−99.20%) |
| TCP connection setup (client connect) | 0.033 ms |

Raw TCP's patterned throughput was 15.2× the FASTLINK framed path in this one
run. This is not a speedup guarantee: raw TCP skips chunk hashing, framing,
codec/decode work. RLE raw-equivalent MB/s divides original uncompressed bytes
by elapsed time; wire MB/s divides encoded wire bytes by elapsed time. Decimal
MB/s (10^6 B/s) is used. The suite makes no GPU, inference, LAN/WAN, or
multi-client claim.

## Benchmark reproducibility checklist

For a comparable loopback remeasurement:

1. Record the source commit, local timestamp/time zone, OS/kernel, CPU, and
   `rustc -V`; save stdout beside the JSON result.
2. Run exactly `cargo bench --bench rpc_latency` (optimized profile). Do not
   substitute `cargo test --all-targets`: it executes the custom bench in a
   debug/test context and is not comparable.
3. Verify the source constants: 65,536-byte chunks, 8,388,608-byte tensors,
   20 latency warmups + 500 RTT samples, and 3 throughput warmups + 32 measured
   round trips. Patterned input is deterministic splitmix64; uniform input is
   0xAB bytes.
4. The raw and framed variants each use one fresh loopback TCP connection per
   scenario, set `TCP_NODELAY` on both endpoints, and exclude connect time from
   RTT/throughput windows. Connection setup is a separate one-shot metric.
5. Preserve the raw output and structured result. Compare wire bytes and the
   documented measurement definitions; never gate on a fixed MB/s threshold.
6. Repeat at least three times on an otherwise idle host before describing a
   change as a repeatable performance improvement. The checked-in snapshot is a
   single run and should be presented as such.
7. Keep transport results distinct from the detti GPU/inference benchmark in
   `RESULTS.md`. This loopback test does not include GPU work or end-to-end RPC.
8. `cargo test --all-targets` includes a CI smoke test that checks the canonical
   snapshot against README, this file, PROGRESS.md, RESULTS.md, and the harness
   constants. CI also runs `cargo bench --bench rpc_latency` on Ubuntu to verify
   the loopback harness executes; its timing is not compared against this host.
