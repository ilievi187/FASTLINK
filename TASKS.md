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

Each implementation should have correctness tests, honest benchmarks, and a scoped PR.
