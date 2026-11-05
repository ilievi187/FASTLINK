# Plan — FASTLINK

**2026-11-05 · public pre-alpha start**

1. Publish an original, reproducible CPU transfer protocol implementation with clear security limitations. **Complete.**
2. Test repeat-file caching and changed-chunk synchronization with byte-identical reconstruction. **Complete.**
3. Add cross-platform CI and strict lint tests. **Complete — GitHub Actions Linux and Windows jobs passed (run 42098205495).**
4. Harden protocol sessions: authentication, encrypted transport, cache quotas, concurrency, resume, malformed-input fuzzing. **Not started.**
5. Compare adaptive compression codecs (LZ4/Zstd/RAW/RLE) using throughput, CPU cost and entropy. **Not started.**
6. Design a llama.cpp adapter as an isolated integration; upstream v8 RPC cache behavior must be compared fairly. **Not started.**
7. Only after owner approval: test GPU-resident state and two-machine Q4 inference. **Not started.**

Do not modify or interrupt an already-running 256K Heretic session during project development.
