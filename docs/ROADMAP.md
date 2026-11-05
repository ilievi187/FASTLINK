# FASTLINK Roadmap

## M0 — CPU proof of concept ✅

- [x] Create public Apache-2.0 project
- [x] Versioned transport handshake
- [x] BLAKE3-addressed chunks, 64 KiB fixed boundaries
- [x] Cached-reference reuse and changed-chunk transfer
- [x] Lossless RAW/RLE encoding and bounded decoding
- [x] Loopback-only prototype and simple CLI
- [x] Tests and byte-identical local end-to-end demonstration

## M1 — Transport and security

- [ ] Authenticated peer pairing and encrypted transport; no public raw RPC exposure
- [ ] Durable cache manifest, quota/eviction and concurrent-writer correctness
- [ ] Resume sessions, crash recovery and disconnect tests
- [ ] Backpressure, bounded queues, robust error signaling
- [ ] Linux/Windows CI and real cross-platform transfer tests
- [ ] LZ4/Zstandard codecs; choose using full elapsed cost
- [ ] Honest wire-byte accounting, transfer-speed and latency benchmarks

## M2 — GPU object cache and scheduling

- [ ] Content-addressed immutable weights and mutable versioned tensors
- [ ] Object handles and device-resident state lifecycles
- [ ] Correctness-preserving layout, dtype and shape negotiation
- [ ] Device affinity and dependency-aware task placement
- [ ] Batch independent calls and reduce synchronization points

## M3 — llama.cpp integration (isolated)

- [ ] Map safe chunked tensor transfer to explicit RPC transport framing
- [ ] Cache integrity and replay support with capability negotiation
- [ ] Build Linux and Windows ggml adapters separately
- [ ] Compare stock RPC (with and without its built-in cache) to FASTLINK
- [ ] Measure cold-load, warm-load, prefill, decode tokens/sec on real dual-GPU hardware
- [ ] Regression tests at short, 65K, 128K and 256K contexts when memory permits

## M4 — FOSS prerelease

- [ ] Fuzz parsers, document wire version policy and audit security
- [ ] Publish reproducible benchmarks and known regressions
- [ ] Distributable CLI packages and clear contributor workflows

FASTLINK does **not** control or automatically modify any user's existing hardware setup.
