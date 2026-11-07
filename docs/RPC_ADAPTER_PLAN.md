# Isolated llama.cpp RPC adapter research plan

Status: design only; no RPC implementation or production integration. FASTLINK
v0.1 synchronizes files using BLAKE3-addressed chunks. It does not carry RPC
commands, tensor lifetimes, inference, or GPU operations.

1. Pin a specific upstream llama.cpp commit and inspect its public RPC client,
   server, framing, built-in caching, build options, tests and license. Record
   the exact revision and compatibility boundaries before choosing an interface.
2. In a separate experimental integration directory or branch, construct a
   CPU-only mock peer and a transparent stock-protocol baseline. Preserve message
   ordering, errors, disconnect behavior and negotiation; never assume arbitrary
   RPC messages are interchangeable with v0.1 file manifests.
3. Design opt-in capability/version negotiation and an explicit stock fallback.
   Restrict deduplication initially to verified immutable payloads with known
   ownership and lifetime. Define hashes, cache scope, quotas, invalidation,
   replay, error handling and transactional behavior. Mutable tensors require
   explicit generations and synchronization semantics before reuse.
4. Test malformed frames, collisions and digest failures, concurrent clients,
   reconnects, peer restarts, cache eviction, ordering and baseline equivalence
   with disposable synthetic data. Authenticate/encrypt all remote links; keep
   raw experimental listeners on loopback behind SSH.
5. Benchmark against the same pinned stock RPC, including its own cache, using
   the same fixtures, hardware and transport. Capture total wire bytes and full
   end-to-end latency, not just payload bytes. Only after explicit authorization
   and CPU validation, measure model loading, prefill and decode on real GPUs.

Exit criteria: deterministic equivalence and failure behavior, supported-version
checks, measurable end-to-end benefit, reproducible results and owner approval.
Until then do not replace RPC or alter existing launchers or services.
