# Engineering tasks

## Next tasks (ordered)

- [ ] **P0 security:** Design authenticated, encrypted transport or SSH-only operational integration; do not expose raw TCP.
- [ ] **P0 correctness:** Add malformed frame, truncated stream, invalid codec, bad digest and interrupted-session integration tests.
- [ ] **P0 storage:** Add bounded cache size, atomic content-addressable writes and safe crash recovery.
- [ ] **P1 compression:** Benchmark LZ4 and Zstd against RAW/RLE using incompressible, sparse and tensor-like synthetic data.
- [x] **P1 networking:** Added CPU-only localhost TCP stream byte counts and end-to-end timing for cold/warm/delta/sparse sync, three repeats each. Does not count TCP/IP/Ethernet headers or provide inference timing.
- [ ] **P1 portability:** Verify Windows tests and sample CLI run in GitHub Actions.
- [ ] **P2 integration:** Build an experimental ggml/llama.cpp RPC adapter in its own integration path.
- [ ] **P2 execution:** Model verified remote tensor references, generations, lifetimes and dependency fences.
- [ ] **P3 GPU:** Run the stock/v2/v3 real-model A/B benchmark. Owner has explicitly requested it; defer until the live Heretic v2 service is verified idle or a maintenance window is available. Never interrupt or compete with active inference.
- [x] **V3 CPU model:** Serial placement model committed on `feat/fastlink-v3-cpu-baseline`; 12 tests pass; limitations documented.
- [x] **V3 network simulator:** Deterministic serial transfer/retry/disconnect model on isolated worktree, integrated into CPU branch; 22 tests pass. Model only.
- [ ] **V3 instrumentation:** CPU file-sync stream-byte/timing harness is implemented and verified; add per-stage hash/manifest/cache timing and RPC prefill/decode/network measurements when integration exists.
- [ ] **V3 RPC:** Verify behavior of the existing local adapter against a clean pinned stock build; do not edit its dirty llama.cpp source tree.
- [ ] **V3 telemetry:** Expose real worker/backend metrics only where APIs provide measured values; report unavailable metrics as unavailable.
- [ ] **V3 reliability:** Add mock worker fault injection for truncation, disconnect, retry, backpressure and cache invalidation.
- [ ] **V3 A/B:** Compare stock/v2/v3 with identical prompts, sampling, context and cache state; serialize GPU workloads and avoid active-service interference.

Each implementation should have correctness tests, honest benchmarks, and a scoped PR.
