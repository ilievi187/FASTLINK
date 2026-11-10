# FASTLINK

**Move less data. Keep reusable state close to compute. Measure the difference.**

FASTLINK is an **independent, Apache-2.0-licensed FOSS research project** for state-aware transfer between machines and, eventually, distributed GPU workloads.

> **Status: pre-alpha.** The first milestone is a working **CPU-only file synchronization prototype**. It does **not yet** accelerate `llama.cpp`, expose a virtual GPU, implement remote GPU execution, or replace any existing RPC driver.

## Working in milestone 0.1

- Content-addressable cache with BLAKE3 integrity checks.
- Lossless fixed-size 64 KiB chunk synchronization; repeated chunks can be reused by reference.
- Changed-chunk transfer, including new data that differs from a prior version.
- Simple adaptive RAW/RLE encoding, selected only when smaller. More general codecs are planned.
- Versioned binary wire format and bounded decode sizes.
- Local-only server, with explicit protection against accidental network exposure.
- Loopback TCP transport for framed tensors with a single-chunk latency and 8 MiB throughput bench.
- Linux and Windows builds from one Rust codebase.
- Unit tests and a three-stage local transfer demonstration.

## Quickstart

Requirements: Rust (stable), Cargo.

```bash
cargo build --release
# Terminal A
cargo run --release -- serve 127.0.0.1:18777 ./.fastlink-data
# Terminal B
cargo run --release -- send ./examples/hello.txt 127.0.0.1:18777
cargo run --release -- send ./examples/hello.txt 127.0.0.1:18777
```

Re-run after editing the file: unchanged chunks are reused and only missing chunks are sent.

The reconstructed file is written to `.fastlink-data/outputs/`; chunks live under `.fastlink-data/chunks/`. Outputs and cache data are ignored by Git.

**Use on one computer only for now.** The server refuses non-loopback bindings. For a remote test use an authenticated encrypted tunnel, not an open port:

```bash
# Example: on the sender, with fastlink running on the receiver's loopback.
ssh -N -L 18778:127.0.0.1:18777 your-authorized-host
cargo run --release -- send ./examples/hello.txt 127.0.0.1:18778
```

No unauthorized network access, automatic device discovery, or credential setup is included.

## Example baseline (synthetic and highly repetitive)

A local test with an 8 MiB mostly-zero file gave:

| Scenario | Unique chunks transmitted | Encoded payload bytes |
| --- | ---: | ---: |
| First upload | 3 | 1,552 |
| Identical upload | 0 | 0 |
| One-byte change | 1 | 518 |

SHA-256 matched the reconstructed file. **These figures exclude chunk manifests, framing, TCP headers, and local hashing costs.** They are NOT representative of high-entropy activations or AI inference throughput. See [Benchmarks](docs/BENCHMARKS.md).

The latest checked-in loopback snapshot (`rpc-latency-linux-2026-11-10-230038-76e77cc`)
measured raw TCP at 8,401.5 MB/s and FASTLINK RAW at 547.2 MB/s for one patterned
8 MiB tensor (15.4× in this single run). FASTLINK added 652 B of framing; 64 KiB
median RTT was 0.020 ms raw / 0.095 ms framed. This is one Linux loopback
measurement, not an inference, LAN/WAN, or performance guarantee; use [the
reproducibility checklist and full table](docs/BENCHMARKS.md).

## Why this exists

A naive distributed execution system may repeatedly transfer inputs, weights, metadata, and intermediate buffers. FASTLINK explores ways to avoid transfers through content-addressing, locality, delta updates, selective compression, and eventually scheduling and graph replay. Compression only wins when it reduces **total execution time**.

See [Wire protocol](docs/PROTOCOL.md), [research and attribution](docs/RESEARCH.md), and [roadmap](docs/ROADMAP.md).

## Upstream compatibility

`llama.cpp` has a separate, experimental RPC backend. FASTLINK does **not** claim to be its drop-in replacement. A future adapter will be developed and benchmarked separately. Do not upload private models, secrets, or existing machine-specific launch configurations to the public repository.

## Independence and licensing

FASTLINK is independent of WiCi and is **not an official or reconstructed implementation** of the proprietary WiCi Protocol. Publicly documented research is cited; no proprietary SDK code is included. Copyright (c) 2026 FASTLINK contributors. Licensed under Apache-2.0; see [LICENSE](LICENSE).

**Contributions welcome**: see [CONTRIBUTING.md](CONTRIBUTING.md).
