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
