# Known limitations — pre-alpha

- **CPU-only prototype:** no distributed GPU compute, no llama.cpp adapter, no GPU tensor API.
- No authentication or encryption. Loopback-only listener enforces safe-by-default behavior.
- Fixed 64 KiB chunks may resend entire blocks for shifted data; content-defined chunking is future work.
- Simple RLE handles long runs but is ineffective for high-entropy tensors; Zstd/LZ4 experiments are planned.
- No resumable transaction, quota/eviction, concurrency or peer discovery.
- Full-file manifest hashing and cache integrity checks add disk reads and CPU overhead.
- The file-sync CLI counts encoded payload bytes; the transport bench additionally reports wire bytes. Benchmarks use synthetic data only.
- The experimental TCP transport is loopback-only, unauthenticated, unencrypted and single-connection; it is a measurement path, not a secure channel.
- Windows compatibility is intended via Rust standard library but requires CI verification.
- 64 GB transfer cap, 1,000,000 chunks, 240-byte file-name cap.
- Single-connection server serializes clients; untrusted clients can hold a session.
- No production security audit; do not expose raw endpoints or send confidential files.

See `docs/ROADMAP.md` for planned remediation.
