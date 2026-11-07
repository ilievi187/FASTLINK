# Known limitations — pre-alpha

- **CPU-only prototype:** no distributed GPU compute, no llama.cpp adapter, no GPU tensor API.
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

- **v3 status (2026-11-07):** only CPU-side serial placement and fault simulators exist on `feat/fastlink-v3-cpu-baseline`; no v3 RPC execution, worker service, GPU scheduling, telemetry, OpenAI API or JCode integration has been implemented.
- Both simulators consume declared link/device numbers; they are not device discovery, network measurements, GPU benchmarks or utilization telemetry.
- Live Heretic v2 is the preserved rollback baseline. Do not benchmark concurrently with active inference or modify its launcher/profile.
- The public repository's v0.1 file-sync protocol is separate from the experimental llama.cpp RPC adapter; neither alone constitutes production-ready distributed inference.
- The separate llama.cpp adapter's CPU codec CTest passes in the existing configured Linux release build, but socket-level RPC tests, Windows rebuild/parity, actual GPU graph execution through the adapter, and comparative inference benchmarks remain unverified in this session.