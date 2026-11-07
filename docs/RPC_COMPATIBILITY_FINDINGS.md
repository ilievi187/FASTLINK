# RPC compatibility investigation — 2026-11-06

The locally installed llama.cpp checkout used by the existing Heretic launcher
is based on commit `0ec4027a2a0183de87c007069adddc06e665f7de`, with local
uncommitted changes to `ggml/src/ggml-rpc/ggml-rpc.cpp`. This is a local source
inspection, not a claim about current upstream or a compatibility test.

At that revision the RPC request contains a command byte, native `size_t`
length and request bytes; a response-bearing request has a length and response
bytes. HELLO negotiates version and transport capabilities. Commands include
allocation, tensor writes/reads, graph execution and cleanup. The stock source
already has a weight-oriented hash cache and graph caching. Those semantics and
their lifetimes must be preserved; v0.1's single-file SYNC session cannot be
substituted for this message stream.

The existing Heretic launcher selects a locally patched llama-server and a
separate remote RPC server via an SSH tunnel. Its `GGML_RPC_FASTLINK_PACK`
option aggregates small writes while retaining RPC framing. A separate opt-in
local chunking change splits large tensor writes and bypasses hash-cache writes
for those pieces. Neither implements this repository's FASTLINK v0.1. No
performance or safety claims about either patch follow from source inspection.

The existing source tree is dirty and may belong to the user. Do not reset or
reuse it as a clean baseline. Compare a clean, pinned build against a separately
patched build and the matching Windows peer, including cache-enabled stock RPC.
Before deployment verify byte-exact RPC behavior, tensor lifetime/ordering,
disconnect handling and synthetic inference against the baseline. CPU-only
mock or test backends must precede any separately authorized GPU test. Test
actual network-byte counts as well as cold and warm model load, prefill and
decode; a file-sync benchmark is not an inference benchmark.

Existing Hermes/JCode profiles and shortcuts target the older named setup.
Do not silently relabel or modify them to claim integration with v0.1 until an
adapter has passed those checks. See `RPC_ADAPTER_PLAN.md` for the gates.
