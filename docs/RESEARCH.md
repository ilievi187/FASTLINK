# Public research and independent implementation

FASTLINK's implementation is original and independent. These sources provide context and comparison, not source code to copy:

- [WiCi technology overview](https://wici.ai/technology): publicly describes keeping work near GPU resources, avoiding unnecessary round trips, and reducing network payloads. These are high-level systems concepts, **not** a public WiCi wire-format specification.
- [WiCi developer overview](https://wici.ai/developers): API/SDK/driver integration paths described for the WiCi offering; FASTLINK does not implement their APIs or drivers.
- [WiCi Developer Portal](https://developer.wici.ai/): public developer documentation, separate from this project.
- [llama.cpp RPC README](https://github.com/ggml-org/llama.cpp/blob/master/tools/rpc/README.md): experimental ggml RPC, GPU offloading, optional server-side cache, and security warnings.
- [BLAKE3](https://github.com/BLAKE3-team/BLAKE3): content-integrity hash algorithm.
- [Zstandard](https://facebook.github.io/zstd/) and [LZ4](https://lz4.org/): potential future adaptive codecs.

## Non-goals

- Reproducing non-public WiCi internals or implementing a purported official WiCi Protocol.
- Claiming that reducing bytes automatically reduces latency or raises tokens/sec.
- Treating compression of highly repetitive synthetic data as equivalent to compression of high-entropy tensors.
- Shipping a public, unauthenticated RPC execution endpoint.

## Measurements required before GPU claims

1. Complete bytes on the wire, not payload alone.
2. RTT and blocking RPC calls per inference step.
3. Compression, hashing, and reconstruction CPU time.
4. Correctness under cache misses, modified chunks, and network failure.
5. Model cold/warm load times, prompt processing and tokens/sec, under equivalent model/context settings.
6. Both GPU memory budgets and host RAM/swap pressure.
7. Comparisons with stock llama.cpp RPC and its own caching support.

The first milestone establishes lossless chunk reuse. GPU work is subsequent research, not a reported result.
