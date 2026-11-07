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

---

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

