# Engineering tasks

## Next tasks (ordered)

- [ ] **P0 security:** Design authenticated, encrypted transport or SSH-only operational integration; do not expose raw TCP.
- [ ] **P0 correctness:** Add malformed frame, truncated stream, invalid codec, bad digest and interrupted-session integration tests.
- [ ] **P0 storage:** Add bounded cache size, atomic content-addressable writes and safe crash recovery.
- [ ] **P1 compression:** Benchmark LZ4 and Zstd against RAW/RLE using incompressible, sparse and tensor-like synthetic data.
- [ ] **P1 networking:** Add actual transport byte counters and time measurements excluding setup/including setup.
- [ ] **P1 portability:** Verify Windows tests and sample CLI run in GitHub Actions.
- [ ] **P2 integration:** Build an experimental ggml/llama.cpp RPC adapter in its own integration path.
- [ ] **P2 execution:** Model verified remote tensor references, generations, lifetimes and dependency fences.
- [ ] **P3 GPU:** Benchmark against stock RPC with cache for real models, only after explicit authorization.
- [ ] **V3 CPU model:** Build deterministic placement cost model for compute, queue, transfer, RTT and explicit VRAM reserve; six tests currently pass on isolated branch `feat/fastlink-v3-cpu-baseline`.
- [ ] **V3 instrumentation:** Add complete wire-byte accounting and separate sender hash, manifest, cache validation, reconstruction, prefill/decode and network timings.
- [ ] **V3 RPC:** Verify behavior of the existing local adapter against a clean pinned stock build; do not edit its dirty llama.cpp source tree.
- [ ] **V3 telemetry:** Expose real worker/backend metrics only where APIs provide measured values; report unavailable metrics as unavailable.
- [ ] **V3 reliability:** Add mock worker fault injection for truncation, disconnect, retry, backpressure and cache invalidation.
- [ ] **V3 A/B:** Compare stock/v2/v3 with identical prompts, sampling, context and cache state; serialize GPU workloads and avoid active-service interference.

Each implementation should have correctness tests, honest benchmarks, and a scoped PR.
