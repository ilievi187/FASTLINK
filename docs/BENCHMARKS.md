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
