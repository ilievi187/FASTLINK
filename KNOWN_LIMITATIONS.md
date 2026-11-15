# Known limitations — pre-alpha

- **CPU-only prototype:** no distributed GPU compute, no llama.cpp adapter, no GPU tensor API.
- **Desktop shell (pre-alpha):** Linux-verified local-only inventory (`fastlink.devices.v1`) for the sole `local_only` node; the opaque hashed `node_id` is preserved and never replaced with `local`. Opt-in is session-only, local-node-only and not persisted; no pairing, models, chat, inference or remote controls. Unreported network speed/connectivity remains unknown. CLI output capture is capped at 1 MiB per stream; only the direct CLI child is killed on timeout, though inherited descendants cannot block the file-based capture worker.
- No authentication or encryption. Loopback-only listener enforces safe-by-default behavior.
- Fixed 64 KiB chunks may resend entire blocks for shifted data; content-defined chunking is future work.
- Simple RLE handles long runs but is ineffective for high-entropy tensors; Zstd/LZ4 experiments are planned.
- No resumable transaction, quota/eviction, concurrency or peer discovery.
- Full-file manifest hashing and cache integrity checks add disk reads and CPU overhead.
- Metrics count encoded payload bytes, not total on-the-wire traffic. Benchmarks use synthetic data only.
- Windows compatibility is intended via Rust standard library but requires CI verification.
- 64 GB transfer cap, 1,000,000 chunks, 240-byte file-name cap.
- Single-connection server serializes clients; untrusted clients can hold a session.
- No production security audit; do not expose raw endpoints or send confidential files.

See `docs/ROADMAP.md` for planned remediation.
