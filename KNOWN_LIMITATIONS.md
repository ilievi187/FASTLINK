# Known limitations — pre-alpha

- **CPU-only prototype:** no distributed GPU compute, no llama.cpp adapter, no GPU tensor API.
- No authentication or encryption. Loopback-only listener enforces safe-by-default behavior.
- Fixed 64 KiB chunks may resend entire blocks for shifted data; content-defined chunking is future work.
- Simple RLE handles long runs but is ineffective for high-entropy tensors; Zstd/LZ4 experiments are planned.
- No resume token, on-disk cache quota, or peer discovery. `MemoryBudgetCache` bounds an in-memory payload cache with FIFO eviction and does not delete durable chunk files. The on-disk content-addressed cache still grows without a quota. The next session deletes uncommitted `.partial-*` and `.fastlink-inprogress-*` files and does not promote them. A dropped transfer must be sent again.
- Full-file manifest hashing and cache integrity checks add disk reads and CPU overhead.
- Metrics count encoded payload bytes, not total on-the-wire traffic. Benchmarks use synthetic data only.
- Windows target `x86_64-pc-windows-gnu` was tested under Wine 11.20 on this Linux host (2026-11-07, commit `cdfc5f3`): 42 library tests and 15 integration tests passed, exit 0. Log: `qa/results/windows-gnu-cargo-test.txt`. This was not a native Windows machine and not a GitHub Actions run of that commit. `.github/workflows/ci.yml` already lists `windows-latest`. The Unix symlink-output test is not compiled for the Windows target (43 library tests on Linux). Directory metadata sync after rename is Unix-only; the Windows build syncs the file before rename.
- 64 GB transfer cap, 1,000,000 chunks, 240-byte file-name cap.
- Single-connection server serializes clients; untrusted clients can hold a session.
- No production security audit; do not expose raw endpoints or send confidential files.

See `docs/ROADMAP.md` for planned remediation.
