# Known limitations — pre-alpha

- **CPU-only prototype:** no distributed GPU compute, no llama.cpp adapter, no GPU tensor API.
- **Desktop shell (pre-alpha):** Linux-verified local-only inventory (`fastlink.devices.v1`) for the sole `local_only` node; the opaque hashed `node_id` is preserved and never replaced with `local`. Opt-in is session-only, local-node-only and not persisted; no pairing, models, chat, inference or remote controls. Unreported network speed/connectivity remains unknown. Linux/Unix enforces a reader-counted 1 MiB per-stream capture cap with bounded pipes; an already-exited child gets a bounded 50 ms post-exit drain, while a still-running child is killed at the deadline. The non-Unix fallback only polls a temporary file, so its cap is best-effort. Only the direct CLI child is killed on timeout, though inherited pipes cannot block the worker or grow capture disk.
- No authentication or encryption. Loopback-only listener enforces safe-by-default behavior.
- Fixed 64 KiB chunks may resend entire blocks for shifted data; content-defined chunking is future work.
- Simple RLE handles long runs but is ineffective for high-entropy tensors; Zstd/LZ4 experiments are planned.
- No resumable transaction, cross-process/multi-instance quota lock, concurrency or peer discovery. The opt-in `ChunkCache::with_max_bytes` bound applies to successful sequential operations on one instance only and is not wired into the file-sync CLI/server. `store()` may temporarily stage/rename data before eviction; an eviction failure can leave the cache above quota, and stale `.partial` files are excluded from quota eviction.
- Full-file manifest hashing and cache integrity checks add disk reads and CPU overhead.
- The file-sync CLI counts encoded payload bytes; the transport bench additionally reports wire bytes. Benchmarks use synthetic data only.
- The experimental TCP transport is loopback-only, unauthenticated, unencrypted and single-connection; it is a measurement path, not a secure channel.
- The feature-gated `rpc-v3-transport` client/pool (`AsyncClient` / `Pool`) is synchronous blocking `std::net::TcpStream` loopback measurement code (`AsyncClient` is a historical name); `Pool::batch_send` is mutex-serialized, and no `async_recv`, concurrent chunk-read API, TCP keep-alive, adaptive batch sizing, pool scaling, or connection warmup exists.
- Windows compatibility is intended via Rust standard library but requires CI verification.
- General file sync allows up to 1,000,000 chunks and 240-byte file names.
- Each buffered RPC tensor is separately capped at 64 MiB of raw tensor payload because receive-side reassembly allocates the complete tensor. This is not an aggregate batch or process-memory cap: `batch_send` concatenates all tensors into one buffer, and sender framing copies plus concurrent receives can exceed 64 MiB.
- Single-connection server serializes clients; untrusted clients can hold a session.
- No production security audit; do not expose raw endpoints or send confidential files.

See `docs/ROADMAP.md` for planned remediation.
