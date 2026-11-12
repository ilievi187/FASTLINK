# Plan — FASTLINK

**2026-11-05 · public pre-alpha start**

1. Publish an original, reproducible CPU transfer protocol implementation with clear security limitations. **Complete.**
2. Test repeat-file caching and changed-chunk synchronization with byte-identical reconstruction. **Complete.**
3. Add cross-platform CI and strict lint tests. **Complete — GitHub Actions Linux and Windows jobs passed (run 42098205495).**
4. Harden protocol sessions: authentication, encrypted transport, cache quotas, concurrency, resume, malformed-input fuzzing. **Not started.**
5. Compare adaptive compression codecs (LZ4/Zstd/RAW/RLE) using throughput, CPU cost and entropy. **Not started.**
6. Design a llama.cpp adapter as an isolated integration; upstream v8 RPC cache behavior must be compared fairly. **Not started.**
7. Only after owner approval: test GPU-resident state and two-machine Q4 inference. **Not started.**

**2026-11-07 · v3 isolated kickoff**

8. Preserve v2 on port 18879; inventory actual service state before each test. **Active; untouched.**
9. Deliver CPU-only RPC cost model and baseline tooling before GPU integration. **Serial placement and fault-simulator models implemented on `feat/fastlink-v3-cpu-baseline`; 34 Python tests pass (models only).**
10. Build security/correctness/cache/telemetry gates and mock worker before any backend deployment. **Not started.**
11. Compare the pinned stock RPC, existing adapter, and v3 on isolated ports/workers. **Requires safe maintenance window for GPU benchmarking.**
12. Only after controlled A/B verifies correctness, stability, security and benefit: stage a separate release candidate and migration/rollback procedure. **Not started.**

Real GPU test policy: repository `AGENTS.md` requires explicit owner approval; the user has now explicitly requested comparable GPU benchmarks as part of this mission. Still do not interrupt current active v2 inference; schedule GPU A/B only when service is idle or with operator-approved maintenance window.