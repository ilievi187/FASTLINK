# Development progress

## 2026-11-17 — Streaming review-proof follow-up (`fix/luna-stream-review-test-proofs`)

- Independent review of streaming HEAD `e99742f` confirmed the queue-cancellation, event-budget, chunk-budget, and write-deadline implementation changes, and found three P3 test-proof gaps.
- Added a lib unit test that pre-fills a `sync_channel(1)` and verifies both internal and external cancellation release `send_event_or_cancel`; a test-only notification from the `TrySendError::Full` branch now synchronizes cancellation after the worker actually observes the full queue (the prior start signal plus sleep was insufficient). Added the SSE exact joined-byte boundary/separator-overflow pair and strengthened the write-deadline test to capture a partial request after cancellation.
- The independent follow-up review found the queue test could cancel before the worker entered the full-queue branch. The corrected test was RED until the `#[cfg(test)]` hook was wired, then passed five consecutive runs.
- Verification after the correction: 69 library tests, 35 `llama_rpc_test` tests, workspace lib/bin/integration tests and doctests, fmt, Linux workspace Clippy, Windows GNU all-target check and Clippy, and `git diff --check` all passed.

**Status:** commander verification is green; independent re-review of the corrected follow-up is pending. The hook is compiled only for tests; no production behavior changed.

## 2026-11-17 — GGUF opened-file identity follow-up (`fix/luna-gguf-file-identity`)

- Strict TDD reproduced a same-path replacement bug: the regression test failed because a replacement file at the same pathname retained the old canonical-path identity and stayed confirmed.
- Added `OpenedFileIdentity`, captured from the opened handle's device/inode metadata on Unix. Catalog deduplication and load confirmation now compare the submitted path, canonical display path, and opened-file identity. Non-Unix targets retain a documented canonical-path fallback.
- The regression proves a distinct inode atomically replacing the registered path clears confirmation; explicit re-registration updates the same row and keeps confirmation cleared. No file contents or weights are read.
- Verified focused RED then GREEN; `cargo test -p fastlink-gui --offline --locked --lib` (66 passed), `cargo test -p fastlink-gui --offline --locked --test model_catalog` (38 passed), `cargo fmt --all -- --check`, `cargo clippy -p fastlink-gui --all-targets --offline --locked -- -D warnings`, and `git diff --check`.

**Explicit limitation:** the identity is filesystem-object metadata, not a content fingerprint. In-place edits to the same object are not detected; non-Unix identity is best-effort path-based.

## 2026-11-17 — GGUF catalog recovery review fixes (`fix/luna-gguf-model-ui-recovery`)

- Windows directory opens (ERROR_ACCESS_DENIED without `FILE_FLAG_BACKUP_SEMANTICS`) are classified from the path again, so a directory reports `NotAFile`; access-denied on a regular file stays `NotReadable`.
- Device/UNC/NT namespace checks and DOS reserved-name checks are pure helpers tested on every host. They run only on Windows; `\\?\C:\…`/Volume-GUID extended local paths stay valid and Linux files named `NUL.gguf`/`COM1.gguf` register normally.
- Unix opened-file identity is captured as device/inode metadata from the opened handle; non-Unix targets use a best-effort canonical-path identity. Confirmation mismatches clear the confirmation, set a `Changed` error, and never rename or refresh the row.
- A repointed symlink alias refreshes the existing row instead of adding an unreachable duplicate.
- Rendered names and paths also replace U+2028/U+2029, interlinear/tag characters, and invisible fillers. The unused `MODEL_WORKER_JOB_LIMIT` constant was removed and the worker limit is described without it.
- Tests: workspace lib/tests/doctests, focused model_catalog/headless_ui/command_runner, Clippy, GUI build, and a Windows-GNU all-target check.

**Explicit limitation:** the model worker still cannot interrupt a Windows file open that has already started; the bound remains one background job, not a timeout.

## 2026-11-17 — GGUF catalog confirmation and path-privacy fixes (`feat/luna-gguf-model-ui`)

- Confirmation now requires both the stored path and the opened-file identity. A symlink alias refreshes size only and keeps the original path, canonical identity, and canonical file name.
- An already confirmed path can be re-checked. A failed re-registration or identity/path mismatch clears that confirmation. A different invalid path does not.
- Select and Remove are refused while the single model worker is busy. Each result carries the catalog generation from queue time, so a stale success or error cannot rewrite a removed or different entry.
- Unix identity comes from the opened descriptor. Windows named-pipe and DOS-device paths are rejected before open. A normal Windows `CreateFile` is not cancellable and is not described as having a hard timeout.
- Successful registration clears the draft. Painted paths recognize `HOME` and `USERPROFILE` with either separator, truncate the tail, and replace control, bidi, and format characters in the name and path.
- No weight bytes are read and no model is loaded. Chat stays unavailable.

**Explicit limitation:** the model worker still cannot interrupt a Windows file open that has already started. The bound is one queued job on a background thread, not a timeout.

## 2026-11-17 — Session GGUF catalog review fixes (`feat/luna-gguf-model-ui`)

- The Linux GUI can register more than one explicit `.gguf` path. Each check runs on a bounded background worker, so a stalled filesystem call does not stop discovery polling or cancellation.
- A path is accepted only when a non-blocking open (Unix) and `fstat` show a regular readable file. Directories, FIFOs, and directory symlinks are refused. Metadata errors stay distinct: not found, not readable, and other failures.
- The catalog stores the submitted path, a lossy file name (non-UTF-8 names are not blank), and the byte size. Re-registering the same file refreshes the size. Confirm load re-checks the file and refuses a deleted path. Nothing is copied, scanned, persisted, or loaded.
- The window shows a shortened path (`~` for the home directory, ellipsis for a long prefix) and takes the loaded/chat lines from `loaded` plus the disconnected-backend flags. Chat stays unavailable and no tokens are generated.
- The unreadable-file test follows whether `File::open` actually fails, so Windows read-only attributes and root (DAC override) do not fail the suite.
- Verified with `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, focused `model_catalog` tests, `cargo test --workspace --lib --bins --tests` (benches excluded), `cargo build -p fastlink-gui`, and a Windows-target check when that target is installed. No GUI window, GPU, model load, or network scan.

**Explicit limitation:** confirmation is a session flag only. There is still no inference backend, so weights are never loaded and chat cannot run. Unix non-blocking open is the FIFO guard; other platforms open through the standard library on the same worker. The catalog is memory-only and disappears when the process exits.

## 2026-11-15 — Post-exit capture drain race (final review)

- Fixed the final P3 finding: `capture_child_unix` used `min(now + POST_EXIT_DRAIN, overall_deadline)` for the post-exit drain. If a completed child was observed after the deadline, buffered valid JSON was skipped and reported as empty/malformed.
- The post-exit drain now gets its own bounded 50 ms grace after the worker observes child exit. A still-running child is still killed at the overall deadline; only an already-exited child can consume the extra drain window. The hard byte cap is enforced during the drain.
- Added deterministic Unix tests by injecting a poll interval longer than the timeout: a child that finishes during the sleep is accepted; an already-finished child with `timeout=0` is accepted; and oversized buffered output is still rejected by the cap.
- Verified: `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo build -p fastlink`, `FASTLINK_BIN`-backed `cargo test --workspace --all-targets` (50 core + 69 GUI tests), `cargo build -p fastlink-gui`, and the Windows-target fallback compile check. No GUI window or GPU use.

**Explicit limitation:** an already-exited child can extend total capture time by at most the 50 ms drain grace beyond the overall deadline. Inherited descendants are not killed; they cannot block the worker or grow capture storage on Linux. The non-Unix temp-file fallback remains best-effort, not hard-bounded.

## 2026-11-15 — Hard-bounded capture and truthful stderr detail (second review)

- Replaced the Linux/Unix temporary-file capture from the previous checkpoint with nonblocking pipe readers owned by the waiting worker. There are no reader threads to leak. Bytes are counted as they are read, and the child is killed when either stream exceeds 1 MiB; capture never writes a growing regular file.
- A fast 64 MiB writer now stops at the capture limit, and an inherited writer is cut off when the read ends close. The worker never waits for pipe EOF, so inherited handles cannot hang it. The direct child is still killed and reaped on timeout/capture-limit/wait errors.
- Made stderr diagnostics truthful: the error header reports bytes captured, bytes retained (and retained characters), plus the actual 900-character / 12-line UI detail cap. `compact_detail` now includes its ellipsis within both limits instead of exceeding 900 characters.
- Expanded the regression matrix: >64 KiB on both streams still parses, 2 MiB and 64 MiB fast writers are rejected promptly, measured fast-writer progress stays near the 1 MiB cap, oversized stderr is rejected, stderr accounting is exact, timeout kills the child, and inherited readers/writers neither block nor keep consuming capture resources.
- Retained a compile-checked non-Unix fallback using temporary-file polling; it is documented as best-effort rather than a hard OS bound. Linux is the supported/runtime-tested path.
- Verified: `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo build -p fastlink`, `FASTLINK_BIN`-backed `cargo test --workspace --all-targets` (50 core + 66 GUI tests), and `cargo build -p fastlink-gui`. Cross-checked the Windows fallback with `cargo check --target x86_64-pc-windows-gnu`. No GUI window or GPU use.

**Explicit limitation:** the Linux cap counts bytes delivered to the reader; the kernel pipe buffer can transiently hold at most one pipe capacity beyond that count, but disk usage cannot grow. Only the direct child is terminated; descendants are not killed cross-platform, though they cannot block the worker or grow capture storage on Linux.

## 2026-11-15 — Desktop shell output-capture hardening (superseded by the entry above)

- Fixed the P2 review finding in `InventoryCommand::run`: piped stdout/stderr were not drained while waiting, so a CLI writing more than pipe capacity could deadlock until the timeout, and inherited pipe handles could hang `wait_with_output()` past the deadline.
- Replaced pipes with private, restart-safe temporary-file capture. The kernel drains both streams concurrently; regular-file reads never wait for EOF, so a grandchild holding inherited handles cannot block the worker. Each stream is capped at 1 MiB; stderr diagnostics retain 64 KiB with an explicit truncation marker. Capture files are removed after use.
- Kept the deadline over child execution and capture-size monitoring, killed and reaped the owned child on timeout/capture-limit/wait errors, and preserved existing CLI path/error UX.
- Added TDD regression tests: 128 KiB on both stdout and stderr still parses; oversized stdout and oversized stderr produce deterministic capture-limit errors; exit stderr is retained with a truncation marker; timeout kills the owned child; a grandchild holding the output handles does not block after child exit.
- Verified: `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `FASTLINK_BIN`-backed `cargo test --workspace --all-targets` (50 core + 63 GUI tests), and `cargo build -p fastlink-gui`. No GUI window or GPU use.

**Explicit limitation:** only the direct CLI child is terminated on timeout/error; descendants are not killed cross-platform. File-based capture means they cannot block the GUI worker or its deadline. Output beyond 1 MiB per stream is rejected rather than parsed.

## 2026-11-15 — Linux local-device desktop shell (`feat/linux-local-device-ui`)

- Added the `fastlink-gui` workspace crate: native eframe/egui desktop shell for the first local-device milestone, with the GUI kept out of the core crate's dependency set.
- Merged the latest `integrate/fastlink-team` (`558ab65`, merge `e0340be`) and aligned the parser with the implemented `fastlink devices --json` contract: `schema`, `schema_version`, `scope`, `discovery_mode`, `node_count`, and `nodes[]`.
- The sole node in `scope:"local_only"` is treated as the local device. Its opaque hashed `node_id` is preserved exactly; no literal `local` id is expected, and a missing id disables selection/opt-in rather than inventing one.
- Rendered node facts (hostname, OS/release, architecture, logical CPU count, total/available/free RAM), GPU objects, capability fields, and the CLI's explicit `unknown_fields` list. Null values and the backend `"unknown"` connectivity sentinel remain `Unknown`; network speed/connectivity is never inferred.
- Added manual refresh with a bounded 20 s worker invocation, local-node selection, session-only local-node opt-in, stale-snapshot warnings, honest peer/model/chat empty states, and visible CLI/JSON/schema errors.
- Added a sanitized fixture with the exact backend field set (`crates/fastlink-gui/testdata/devices-v1.json`) and headless parser/state/UI tests. Added an environment-gated command-runner test that invokes the actual integrated binary from `FASTLINK_BIN` and proves the GUI parser accepts its live output.
- Verified on Linux without launching a GUI window: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test --all-targets` with `FASTLINK_BIN` pointing at the built CLI (50 core tests plus 57 GUI tests passing), and `cargo build -p fastlink-gui`.
- Documented build/run, the actual contract, fixture handling, headless verification and limitations in `docs/GUI.md` and the README.

**Explicit limitations:** Linux is the only verified target. Window rendering uses OpenGL (no CUDA/ROCm compute), and this work launched no GUI window; tests are CPU/software headless. No pairing, models, chat, inference or remote controls exist yet. Capabilities that the CLI does not measure remain unknown. Opt-in is session-only, local-node-only and never persisted.

## 2026-11-05 — M0

- Public GitHub repository established: `ilievi187/FASTLINK`.
- Apache-2.0 Rust implementation with a documented binary wire protocol.
- BLAKE3 chunk-addressed cache, exact deduplication, changed-chunk transfer.
- RAW/RLE lossless codec with size-based selection and bounded reconstruction.
- Loopback-only TCP CLI: `fastlink serve` and `fastlink send`.
- Five Rust unit tests passed; rustfmt, clippy and release compilation passed locally.
- GitHub Actions cross-platform CI passed on Ubuntu and Windows (run 42098205495).
- 8 MiB synthetic local test: cold payload 1,552 B; warm payload 0 B; one-byte delta 518 B.
- SHA-256 of source and output matched; byte comparison passed.

**Explicit limitation:** This does not yet accelerate GPU inference; no GPU benchmarks exist for FASTLINK's new Rust transport. This log is not a measure of real RPC performance.

## 2026-11-08 — O-1 adapter chunk framing

- Added `src/rpc_client.rs`: FASTLINK v0.1 64 KiB/BLAKE3 chunking with RAW/RLE
  selection, rendered as the llama.cpp adapter's FLR1/FLR3 per-chunk payload
  (`codec:u8`, `encoded_size:u32 LE`, encoded bytes).
- Added `tests/rpc_integration.rs`: byte-exact 64 KiB golden vector
  (BLAKE3 `b01eb900…bb7984`, payload `535a74d2…99d7c0`), determinism,
  multi-chunk ordering, RAW fallback, decode round-trips and malformed-payload
  rejection.
- `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings`
  and `cargo test --all-targets` pass (5 library + 5 integration tests).
- No dependency added. The adapter repository was read-only and was not built
  or modified.

**Explicit limitation:** Only the adapter's per-chunk payload is produced. The
outer FLR1/FLR3 envelope (SHA-256, original RPC header, ≥128 KiB raw tensor) is
not generated, and no adapter process was started, so this is framing
correctness — not an end-to-end RPC transfer or a performance claim.

## 2026-11-10 — O-2 loopback TCP transport and latency bench

- Added `src/transport.rs`: `send_tensor`/`receive_tensor` over loopback TCP.
  Frames tensors with the existing `frame_tensor`, coalesces the 12-byte tensor
  header and every FASTLINK-RPC/1 adapter payload into one `write_all`, and
  validates codec/decoded length on receive. The wire carries no BLAKE3 chunk ID,
  so successful decode does not prove chunk-content integrity.
- Added `benches/rpc_latency.rs` (plain std timing, no new dependencies):
  single-chunk RTT, 8 MiB loopback throughput, wire-byte overhead versus raw,
  and connection-setup time.
- Added `tests/transport_integration.rs`: byte-exact round-trips (single-chunk,
  multi-chunk, 8 MiB), empty/truncated/malformed header rejection, and a
  latency > 0 plus bytes >= raw sanity check on incompressible data.
- `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings` and
  `cargo test --all-targets` pass. No dependency added; core codec untouched.

**Measured (optimized bench profile, loopback, TCP_NODELAY, single connection):**

- Single-chunk RTT (64 KiB patterned, 500 rounds): min 0.092 ms, median
  0.100 ms, mean 0.103 ms.
- 8 MiB patterned (RAW): 597.2 MB/s raw / 597.3 MB/s wire; 8,389,260 wire bytes
  vs 8,388,608 raw (+652 B: 12-byte tensor header + 640 B of chunk headers).
- 8 MiB uniform (RLE): 1,957.9 MB/s raw / 15.6 MB/s wire; 66,700 wire bytes vs
  8,388,608 raw (−8,321,908 B).
- Connection setup (client connect): 0.039 ms.

**Explicit limitation:** Loopback-only, unauthenticated, unencrypted, single
connection; latency and throughput exclude connection setup. No `llama.cpp`
adapter process was started, so this is a transport measurement, not an
end-to-end RPC transfer or a GPU performance claim.

## 2026-11-10 — Pool + batched bench, transport review + benchmark refresh (initially called async)

- Reviewed `src/transport.rs`: `send_tensor`/`receive_tensor` over loopback TCP,
  framed with the existing `frame_tensor`, single `write_all`, and bounded
  codec/decoded-length validation. Correctness confirmed: header order matches,
  `count == chunk_count(total_len)` validated, `encoded_len` bounds-checked
  (1..=MAX_ENCODED_BYTES), unknown codec / odd RLE / size-mismatch all rejected,
  truncated read returns `Underflow`. Edge cases: empty tensor rejected by
  `frame_tensor`; `total_len == 0` early-returned; trailing bytes after `count`
  chunks are ignored (acceptable). The receiver has no transmitted BLAKE3 ID to
  check; do not describe this as content-integrity verification. Perf: preallocated
  `wire` Vec, coalesced write; a persistent pool amortizes connection setup, not
  each tensor's 12-byte + per-chunk headers.
- Historical note from this checkpoint: the original batched bench had clippy
  findings and read its ACK from the wrong socket, so it could hang. The PR #4
  follow-up removed that socket-based claim/behavior; the current batched bench
  prints design arithmetic only and performs no network or timing measurement.
- Refreshed transport latency/throughput numbers in
  `docs/BENCHMARKS.md` and `README.md` (added a Raw TCP vs framed comparison);
  numbers come from one loopback machine and are not a guarantee.

**Measured (canonical optimized loopback snapshot `rpc-latency-linux-2026-11-10-232742-db55920`):** raw/framed 64 KiB RTT min/median/mean 0.017/0.020/0.021 and 0.093/0.101/0.106 ms; patterned 8 MiB raw TCP 8,103.0 MB/s vs FASTLINK RAW 601.6 MB/s raw-equivalent / 601.6 MB/s wire (+652 B); uniform raw TCP 8,014.9 MB/s vs FASTLINK RLE 1,960.6 MB/s raw-equivalent / 15.6 MB/s wire; connection setup 0.039 ms. Raw TCP was 13.5× faster on patterned throughput in this single run. No speed guarantee or GPU/inference claim. Full provenance and methodology in `benches/results_rpc_latency.json` and `docs/BENCHMARKS.md`.

**Explicit limitation:** loopback only, unauthenticated, unencrypted; single TCP connection per scenario. Transport measurement, not end-to-end RPC or GPU result. No measured batched `Pool` latency/throughput numbers exist; the current batched bench is design arithmetic only and opens no sockets.

## 2026-11-15 — Linux local-device desktop shell (`feat/linux-local-device-ui`)

- Added the `fastlink-gui` workspace crate: native eframe/egui desktop shell for the first local-device milestone, with the GUI kept out of the core crate's dependency set.
- Merged the latest `integrate/fastlink-team` (`558ab65`, merge `e0340be`) and aligned the parser with the implemented `fastlink devices --json` contract: `schema`, `schema_version`, `scope`, `discovery_mode`, `node_count`, and `nodes[]`.
- The sole node in `scope:"local_only"` is treated as the local device. Its opaque hashed `node_id` is preserved exactly; no literal `local` id is expected, and a missing id disables selection/opt-in rather than inventing one.
- Rendered node facts (hostname, OS/release, architecture, logical CPU count, total/available/free RAM), GPU objects, capability fields, and the CLI's explicit `unknown_fields` list. Null values and the backend `"unknown"` connectivity sentinel remain `Unknown`; network speed/connectivity is never inferred.
- Added manual refresh with a bounded 20 s worker invocation, local-node selection, session-only local-node opt-in, stale-snapshot warnings, honest peer/model/chat empty states, and visible CLI/JSON/schema errors.
- Added a sanitized fixture with the exact backend field set (`crates/fastlink-gui/testdata/devices-v1.json`) and headless parser/state/UI tests. Added an environment-gated command-runner test that invokes the actual integrated binary from `FASTLINK_BIN` and proves the GUI parser accepts its live output.
- Verified on Linux without launching a GUI window: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test --all-targets` with `FASTLINK_BIN` pointing at the built CLI (50 core tests plus 57 GUI tests passing), and `cargo build -p fastlink-gui`.
- Documented build/run, the actual contract, fixture handling, headless verification and limitations in `docs/GUI.md` and the README.

**Explicit limitations:** Linux is the only verified target. Window rendering uses OpenGL (no CUDA/ROCm compute), and this work launched no GUI window; tests are CPU/software headless. No pairing, models, chat, inference or remote controls exist yet. Capabilities that the CLI does not measure remain unknown. Opt-in is session-only, local-node-only and never persisted.

## 2026-11-15 — Desktop shell output-capture hardening (initial, later superseded)

- Fixed the P2 review finding in `InventoryCommand::run`: piped stdout/stderr were not drained while waiting, so a CLI writing more than pipe capacity could deadlock until the timeout, and inherited pipe handles could hang `wait_with_output()` past the deadline.
- Replaced pipes with private, restart-safe temporary-file capture. The kernel drains both streams concurrently; regular-file reads never wait for EOF, so a grandchild holding inherited handles cannot block the worker. Each stream is capped at 1 MiB; stderr diagnostics retain 64 KiB with an explicit truncation marker. Capture files are removed after use.
- Kept the deadline over child execution and capture-size monitoring, killed and reaped the owned child on timeout/capture-limit/wait errors, and preserved existing CLI path/error UX.
- Added TDD regression tests: 128 KiB on both stdout and stderr still parses; oversized stdout and oversized stderr produce deterministic capture-limit errors; exit stderr is retained with a truncation marker; timeout kills the owned child; a grandchild holding the output handles does not block after child exit.
- Verified: `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `FASTLINK_BIN`-backed `cargo test --workspace --all-targets` (50 core + 63 GUI tests), and `cargo build -p fastlink-gui`. No GUI window or GPU use.

**Explicit limitation:** only the direct CLI child is terminated on timeout/error; descendants are not killed cross-platform. File-based capture means they cannot block the GUI worker or its deadline. Output beyond 1 MiB per stream is rejected rather than parsed. This implementation was superseded by the pipe-based bounded reader in the next hardening entry.

## 2026-11-15 — Hard-bounded capture and truthful stderr detail (second review)

- Replaced the Linux/Unix temporary-file capture from the previous checkpoint with nonblocking pipe readers owned by the waiting worker. There are no reader threads to leak. Bytes are counted as they are read, and the child is killed when either stream exceeds 1 MiB; capture never writes a growing regular file.
- A fast 64 MiB writer now stops at the capture limit, and an inherited writer is cut off when the read ends close. The worker never waits for pipe EOF, so inherited handles cannot hang it. The direct child is still killed and reaped on timeout/capture-limit/wait errors.
- Made stderr diagnostics truthful: the error header reports bytes captured, bytes retained (and retained characters), plus the actual 900-character / 12-line UI detail cap. `compact_detail` now includes its ellipsis within both limits instead of exceeding 900 characters.
- Expanded the regression matrix: >64 KiB on both streams still parses, 2 MiB and 64 MiB fast writers are rejected promptly, measured fast-writer progress stays near the 1 MiB cap, oversized stderr is rejected, stderr accounting is exact, timeout kills the child, and inherited readers/writers neither block nor keep consuming capture resources.
- Retained a compile-checked non-Unix fallback using temporary-file polling; it is documented as best-effort rather than a hard OS bound. Linux is the supported/runtime-tested path.
- Verified: `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo build -p fastlink`, `FASTLINK_BIN`-backed `cargo test --workspace --all-targets` (50 core + 66 GUI tests), and `cargo build -p fastlink-gui`. Cross-checked the Windows fallback with `cargo check --target x86_64-pc-windows-gnu`. No GUI window or GPU use.

**Explicit limitation:** the Linux cap counts bytes delivered to the reader; the kernel pipe buffer can transiently hold at most one pipe capacity beyond that count, but disk usage cannot grow. Only the direct child is terminated; descendants are not killed cross-platform, though they cannot block the worker or grow capture storage on Linux.

## 2026-11-15 — Post-exit capture drain race (final review)

- Fixed the final P3 finding: `capture_child_unix` used `min(now + POST_EXIT_DRAIN, overall_deadline)` for the post-exit drain. If a completed child was observed after the deadline, buffered valid JSON was skipped and reported as empty/malformed.
- The post-exit drain now gets its own bounded 50 ms grace after the worker observes child exit. A still-running child is still killed at the overall deadline; only an already-exited child can consume the extra drain window. The hard byte cap is enforced during the drain.
- Added deterministic Unix tests by injecting a poll interval longer than the timeout: a child that finishes during the sleep is accepted; an already-finished child with `timeout=0` is accepted; and oversized buffered output is still rejected by the cap.
- Verified: `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo build -p fastlink`, `FASTLINK_BIN`-backed `cargo test --workspace --all-targets` (50 core + 69 GUI tests), `cargo build -p fastlink-gui`, and the Windows-target fallback compile check. No GUI window or GPU use.

**Explicit limitation:** an already-exited child can extend total capture time by at most the 50 ms drain grace beyond the overall deadline. Inherited descendants are not killed; they cannot block the worker or grow capture storage on Linux. The non-Unix temp-file fallback remains best-effort, not hard-bounded.

## 2026-11-16 — Release candidate: passive Linux device inventory

- Added `fastlink devices [--json]` with local-only passive host inventory. Unknown capability values remain explicitly unknown; the command does not perform network discovery, start services, or run GPU compute.
- This release slice contains no LAN/Tailscale discovery, secure worker, llama.cpp inference, or performance claims.

## 2026-11-17 — PR #4 follow-up: Enforceable `ChunkCache` `max_bytes` quota

- Added `ChunkCache::max_bytes()` and `ChunkCache::evict_to_quota()` in `src/cache.rs`, enforcing `max_bytes` across sequential single-instance `ChunkCache` operations.
- `ChunkCache::store()` rejects any chunk whose byte length exceeds a configured `max_bytes` quota (`ErrorKind::InvalidData`) before creating shard directories, `.partial` staging files, or final chunk files (zero-length chunks remain allowed under any `max_bytes`, including `0`).
- After storing a within-quota chunk (and on duplicate `store()` calls within quota), `ChunkCache::store()` evicts oldest valid chunks first (`mtime` nanoseconds, digest hex tie-break) while keeping the just-stored chunk last across same-tick mtime ties until valid stored chunk bytes are `<= max_bytes`.
- Standalone `ChunkCache::evict_to_quota()` evicts oldest-first without exempting the newest/final chunk, so shrinking `max_bytes` over an existing cache removes even a lone oversize chunk until valid chunk bytes are `<= max_bytes`.
- Integration coverage in `tests/cache_quota_test.rs` (7 tests) covers pre-write oversize rejection, shrunk-quota eviction (single oversize and multi-chunk caches), oldest-first eviction with newest retention when it fits, no-quota no-op, duplicate storage, and zero-length chunks.

**Residual limits:** Single-instance sequential chunk-file byte bound only — not a process-wide or concurrent quota lock. `store()` renames the new chunk before evicting older ones, so an eviction error can leave a failed store above quota; transient `.partial` staging files can coexist during atomic writes. `evict_to_quota()` ignores stale non-digest/leftover `.partial` files (`purge_invalid()` removes those, while `total_bytes()` counts all files under shard directories). The opt-in quota is not wired into the file-sync CLI/server. Measurements/tests are CPU-only local checks, not disk-block or throughput claims.

## 2026-11-17 — Bad-digest sync regression and benchmark-free automatic CI

- Added a loopback integration regression for a client advertising digest(A) but sending same-length bytes B; the server must reject the chunk, preserve an existing output, and avoid caching B under A's digest.
- Automatic push/PR CI now runs unit, integration, example, and doctest targets without executing benchmark binaries. The RPC benchmark smoke step is available only by manual `workflow_dispatch` on Linux; normal CI may compile benchmarks but does not run them.
- This is correctness-test coverage and CI policy hardening, not a claim that the broader M1 transport security/storage work is complete.

## 2026-11-12 — Team Checkpoint 1 (`integrate/fastlink-team`)

- Added `src/pairing.rs` and `src/manifest.rs`: opt-in keyed-BLAKE3 pairing (`Loopback`/`Lan`/`Tailscale` only), versioned `PartitionManifest` with direct adjacent-stage `next_stage_addr` routing, and `ManifestCoordinator` join/evacuation/atomic-reshard state machine.
- Added `src/residency.rs` and `src/pipeline.rs`: `ResidencyTable` (budgeted weights & KV/SSM state, epoch invalidation, BLAKE3 verification) and `ActivationFrame` (`FLAC` bounded P2P framing + `p50`/`p95` boundary telemetry).
- Added `src/node_runtime.rs`: multi-stage loopback TCP P2P activation relay (`run_loopback_pipeline`).
- Added `src/kv_delta.rs`: BLAKE3-verified incremental KV/SSM page delta checkpointing for stage migration.
- Verified `cargo fmt --check`, `cargo check --all-targets`, `cargo clippy --all-targets -- -D warnings`, and `cargo test --all-targets` (42/42 unit and integration tests passing) on `integrate/fastlink-team`.

**Explicit limitation:** No multi-device hardware or GPU inference benchmarks have been run (CPU-only loopback and synthetic profile tests).

## 2026-11-13 — Team Checkpoint 3 (`integrate/fastlink-team`)

- Wired `migration_runner` into `ClusterOrchestrator::evacuate_with_kv_migration` with `ClusterKvStore` (`tests/orchestrator_kv_migration_test.rs`).
- Wired `stage_auth` into `node_runtime::run_authenticated_loopback_pipeline` (`tests/auth_node_runtime_test.rs`) and `ClusterOrchestrator::run_authenticated_inference`.
- Verified `cargo fmt --check`, `cargo check --all-targets`, `cargo clippy --all-targets -- -D warnings`, and `cargo test --all-targets` (42/42 unit and integration tests passing) on `integrate/fastlink-team`.

**Explicit limitation:** No multi-device hardware or GPU inference benchmarks have been run (CPU-only loopback and synthetic profile tests).

## 2026-11-12 — Team Checkpoint 2 (`integrate/fastlink-team`)

- Merged `src/scheduler.rs` (`plan_heterogeneous_stages` for 2–16+ heterogeneous nodes) and `src/cli_plan.rs` (`fastlink plan` dry-run CLI subcommand).
- Added `src/cluster_orchestrator.rs`: bridges `scheduler` plans, `pairing::NetworkTrust`, `manifest::ManifestCoordinator` evacuation/fault resharding, `LayerMigration` computation, and `node_runtime::run_loopback_pipeline`.
- Added `src/stage_auth.rs`: epoch- and hop-scoped keyed-BLAKE3 MAC (`seal_stage_frame`, `open_stage_frame`) for P2P `ActivationFrame`s.
- Added `src/migration_runner.rs`: `execute_layer_migration` and `execute_reshard_migrations` using `kv_delta` for verified incremental KV/SSM state migration.
- Verified `cargo fmt --check`, `cargo check --all-targets`, `cargo clippy --all-targets -- -D warnings`, and `cargo test --all-targets` (42/42 unit and integration tests passing) on `integrate/fastlink-team`.

**Explicit limitation:** No multi-device hardware or GPU inference benchmarks have been run (CPU-only loopback and synthetic profile tests).

## 2026-11-13 — Team Checkpoint 3 (`integrate/fastlink-team`)

- Wired `migration_runner` into `ClusterOrchestrator::evacuate_with_kv_migration` with `ClusterKvStore` (`tests/orchestrator_kv_migration_test.rs`).
- Wired `stage_auth` into `node_runtime::run_authenticated_loopback_pipeline` (`tests/auth_node_runtime_test.rs`) and `ClusterOrchestrator::run_authenticated_inference`.
- Verified `cargo fmt --check`, `cargo check --all-targets`, `cargo clippy --all-targets -- -D warnings`, and `cargo test --all-targets` (42/42 unit and integration tests passing) on `integrate/fastlink-team`.

**Explicit limitation:** No multi-device hardware or GPU inference benchmarks have been run (CPU-only loopback and synthetic profile tests).


## 2026-11-15 — Linux Local Device Inventory (`feat/linux-device-inventory`)

- Added `src/devices.rs` and wired `fastlink devices [--json]` in `src/main.rs` and `src/lib.rs`.
- Reports exactly one local node using passive host evidence only: hashed stable local ID (`/etc/machine-id` / `/var/lib/dbus/machine-id` with `hostname-derived` fallback), hostname, OS/kernel release/architecture, logical CPU count, RAM total/available/free bytes (`/proc/meminfo`), and directly discoverable DRM GPU metadata/VRAM (`/sys/class/drm/card*` + `pci.ids`).
- Uses versioned JSON schema `fastlink.devices.v1` (`schema_version: 1`) with explicit `null`/empty values and `unknown_fields` for anything not directly discoverable; never fabricates capacity, throughput, network connectivity, or remote peers, and performs no network scans, port binds, or GPU compute.
- Added `tests/devices_test.rs` covering populated host evidence, missing/unknown fields, corrupt/malformed inputs, CLI arg validation, and live binary execution.
- Verified `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and `cargo test --all-targets`.

**Explicit limitation:** Local-only passive Linux sysfs/procfs inventory; non-amdgpu GPUs without sysfs VRAM counters report `vram_total_bytes: null` / `vram_free_bytes: null`, and capacity/throughput/network connectivity remain explicitly `unknown` until measured.

## 2026-11-16 — M2 Opt-In Discovery Integrated (team branch only)

- The opt-in/privacy contract is committed on `integrate/fastlink-team` (`73d0661`): discovery only on explicit user action, LAN/Tailscale scope, no scanning/probing outside that action, no raw private addresses/endpoints in public output, and candidates remain untrusted/unpaired.
- GUI Discover: rebased feature HEAD `264f5d0` passed CI `43846304501` Ubuntu/Windows and independent review `23e0772` found no findings. The feature was integrated with the backend into `integrate/fastlink-team` at `264f5d0`.
- Backend discovery: final candidate `4a79227` and fixture-only cleanup were independently reviewed with no findings. Rebased backend HEAD `acd4b61` passed full local gates, feature CI `43845541662`, and integration CI `43846067899` on Ubuntu/Windows. The redundant-kill stderr was removed.
- Task 7/12 secure-worker branch `feat/secure-linux-worker` is rebased onto integration base `65bc310` at `f4e63d0a99b9905340518f274b555c8bdcfa196d`, pushed but not merged. It includes proof-bound static keys, non-Clone zeroizing `SessionKey`, one overall handshake deadline, dev-only secure temp dirs, and a partial-frame slow-drip regression. The test failed on baseline `40c95ce` after 1.4686 s against a 1.2 s deadline, then passed at ~1.22 s and 10/10 repeats. Commander reran workspace/all-targets tests, fmt, Linux Clippy, Windows GNU check/Clippy, diff-check, and targeted Semgrep (0 findings); exact feature CI `43863887135` passed Ubuntu/Windows. Independent review of the partial-frame test delta is pending. `snow` remains unaudited, `cargo audit` is unavailable, and no-echo pairing UX/network exchange is incomplete.
- Task 11 MoE planner is complete and integrated into `integrate/fastlink-team`: feature branch `feat/moe-scheduler` at `0c0c439` had independent review with no findings and CI `43842010207` passed Ubuntu/Windows. Integration HEAD `4a61d53` includes it; full local gates passed and exact-head integration CI `43842684310` passed Ubuntu/Windows. Planner only—no inference or throughput claims.
- Integration CI `43852078790` passed at exact integration head `3042ac6` on Ubuntu/Windows; feature CI `43851588654` also passed. The previously red CI `43847764815` at `48934bd` is superseded by the post-exit drain WouldBlock fix.
- No real LAN/Tailscale discovery action, model load, inference, or GPU work has been started. M2 backend+GUI and MoE are integrated remotely into `integrate/fastlink-team` at `3042ac6`; Tasks 5, 6, 8 and 11 are complete. Pairing/removal and secure worker remain experimental/pending; `main` is untouched.

**Next:** get the independent review of the partial-frame test delta; if clean, fast-forward rebased `f4e63d0` into `integrate/fastlink-team`, push and verify exact integration CI. Keep `snow` experimental and track the pairing-secret copy, no-echo/network-exchange UX, key storage and audit gaps; no live discovery or GPU/model/inference until their gates are met.

## 2026-11-16 — Experimental secure-worker slice integrated; GUI and CPU branches advancing

- Fast-forwarded secure-worker branch `feat/secure-linux-worker` into `integrate/fastlink-team` at `3e2137e3357b6960a37921baea4a3bac464864e6`. The rebased feature CI (`43866507647`) and exact integration CI (`43867116852`) passed Ubuntu and Windows; independent full security and partial-frame test reviews found no actionable issues.
- Task 15 partial-frame timeout regression is confirmed RED on `40c95ce` and GREEN on the absolute-deadline fix, 10/10 repetitions. The integrated worker remains experimental and loopback-only. `snow` audit, PairingRegistry temporary secret copies, incomplete no-echo/network pairing UX, 0600 file storage instead of OS keyring, and Noise internal zeroization remain unresolved risks.
- GUI zombie-test branch `b89b830` is locally verified with full workspace gates and Windows GNU cross-check; independent review is pending. It remains unpushed/unmerged.
- CPU MLP branch `2af90f7` is rebased on current integration, pushed, and feature-CI green (`43868198001`, Ubuntu/Windows). Strict RED/GREEN fixed NaN masking through ReLU and authenticated-stage error propagation; full local gates and Semgrep pass. Independent review is pending. This proves only CPU authenticated loopback dense-MLP correctness—not LLM inference, multi-machine generation, or throughput.
- `main` remains untouched. No real network discovery, GPU/model load, or inference workload was started, and protected services were left untouched.

## 2026-11-16 — Linux GUI cancellation reliability integrated; CPU thread leak fixed pending re-review

- Integrated the Linux-only GUI zombie-state test fix at `9d4f3dd`; independent review found no findings, feature CI `43870224018` and exact integration CI `43870664756` passed Ubuntu/Windows. The change is test-only; the separate live-child assertion remains strict.
- Fresh CPU review of `2af90f7` found that a non-final CPU-stage error could detach downstream threads blocked in listener `accept()`. Strict TDD RED showed `NonFiniteActivation` with thread count rising 2→4. Commit `05d7fe8` relays a private nonempty error sentinel so all stages finish/join before the safe error is returned; the isolated regression passed 10/10.
- Rebased CPU candidate `05d7fe8` passed commander-run workspace tests, fmt, Linux Clippy, Windows GNU check/Clippy, targeted Semgrep (0 findings), and feature CI `43871521672` on Ubuntu/Windows. Independent review of this final head remains pending, so it is not integrated.
- Secure-worker residual risks remain tracked: unaudited `snow` 0.10, extra PairingRegistry secret copy, incomplete secure pairing UX/network exchange, 0600 file storage rather than OS keyring, and Noise internal zeroization. No production-security claim; no GPU/model/network work; `main` untouched.

## 2026-11-16 — CPU MLP correctness foundation integrated

- Fast-forwarded `feat/cpu-stage-inference-proof` at `69cdcf7` into `integrate/fastlink-team`. Independent final review found no findings; feature CI `43872823024` and exact integration CI `43873323917` passed on Ubuntu/Windows. `main` remains untouched.
- The CPU-only authenticated loopback MLP matches its monolithic reference and fixed expected values. Strict TDD caught both NaN masking through ReLU and a non-final-stage error path that leaked blocked downstream threads; the final error sentinel is relayed until every stage joins, then the stored error is returned. The focused thread-leak test passed 10/10 and full local gates passed at the rebased head.
- Residual: this does not implement an LLM loader/tokenizer or prove multi-machine inference or throughput. Generic runtime failures before its join loop are not covered by this CPU-specific fix. No model/GPU/server/network activity was started.
- Secure worker remains experimental: `snow` audit, extra PairingRegistry secret copy, secure pairing UX/network exchange, OS-keyring storage and explicit Noise internal zeroization remain unresolved.

## 2026-11-16 — Final-push audit and parallel remediation started

- Audited 44 FASTLINK worktrees/branches, all four open PRs, issues #1/#2/#7, FASTLINK checkpoint/roadmap, Herdr workspaces and protected workload state. Five pre-existing dirty worktrees were left intact. Integration is clean at `acf85df`; exact CI `43874530591` is green; remote `main` remains `8cd1b28` and untouched.
- PRs #3 and #4 have no submitted reviews; #5 is draft/unreviewed; #8 is conflicting and its Windows CI failed in the adaptive codec test. OpenCode is fixing #8 in its existing dedicated PR worktree. Re-evaluate it for the integration branch only after conflict resolution, independent review, and green exact-head CI; keep `main` separately gated.
- AGY is hardening `PairingRegistry` secret lifetime on a fresh isolated branch with strict TDD. Separate no-echo pairing UX/OS keyring and `snow` audit work remains queued; secure worker is still experimental.
- Independent review work is underway for the open PRs and generic runtime error paths. Existing local generation/inference work was observed and left untouched; no GPU/model/network work was started.

## 2026-11-16 — Independent PR audit blockers and runtime fix

- Exact integration checkpoint `a2a5b27` passed Linux/Windows CI `43882412272`. `main` remains unchanged at `8cd1b28`.
- Read-only review findings are now posted and verified on PRs #3/#4/#5: #3 has a P1 privacy disclosure in docs; #4 has a P1 peer-sized allocation plus quota/async/ACK/keep-alive/benchmark/Windows shared-memory defects; #5 has a P1 worker-scoped residency error and P2 retry-deadline/ZSTD interoperability defects. No unsafe PR was merged.
- OpenCode started isolated TDD work on generic runtime cancellation/join leaks (`fix/runtime-failure-join`). AGY's PairingRegistry zeroize change is pushed at `77f5bec`, with commander-run local Linux/Windows compile gates and Semgrep green; exact CI and independent review remain pending.
- PR #8's Windows CI/conflict repair is pushed at `24a39d6` with two exact-head Linux/Windows CI runs green; independent review is pending.
- Preserve the GPU/model and live-discovery approval gate; no inference or network scan was run.

## 2026-11-16 — PairingRegistry secret copy integrated

- Rebased `feat/pairing-secret-hardening` at `c9698f2` onto integration after a clean independent review of the code diff. `RegistrySecret::drop` zeroizes its retained key; local test gates, targeted Semgrep, exact feature CI `43885822332`, and exact integration CI `43886390304` passed on Linux/Windows.
- Fast-forwarded the validated narrow fix into `integrate/fastlink-team`. The remaining keyring, pairing UX/exchange, Noise-state and `snow` audit risks remain open; no production-security claim and no `main` merge.

## 2026-11-16 — PR #8 independent-review follow-up

- Independent review of `24a39d6` found the original tensor-like workload no longer passes through adaptive selection; the finding is recorded on PR #8 in comment `6919477540`.
- AGY is adding selector coverage for the original fixture on a fresh branch while preserving the robust fixture and its strict codec assertions. PR #8 is not being merged until the follow-up is reviewed and exact-head CI is green.

## 2026-11-16 — Main-based Linux M1 release candidate

- Built `release/linux-local-device-ui` from `main` with only passive local inventory, Linux desktop shell, and desktop-entry packaging; no network discovery, secure-worker, model or GPU code was ported. Candidate `f4154ab` passed local Linux workspace tests/fmt/Clippy/build and Windows GNU compile/Clippy.
- Draft PR #12 targets `main`; exact-head CI `43891839756` passed Ubuntu/Windows. Independent review is in progress; Semgrep flagged six candidate matches requiring triage, so the PR is not ready to merge yet.

## 2026-11-16 — Main candidate review findings

- Independent review of PR #12 found that non-Linux `fastlink devices` could report a false-success inventory and that the no-op opt-in label overstated its effect. Both P2 findings are recorded on PR #12 in comment `6920376423`.
- OpenCode is fixing the issues on an isolated branch; PR #12 stays draft until follow-up review, exact-head CI and Semgrep triage are complete.

## 2026-11-16 — Generic runtime shutdown/join fix integrated

- The generic node-runtime early-error fix passed independent review with no findings. After rebasing onto integration, full Linux tests/fmt/Clippy, Windows GNU check/Clippy, targeted Semgrep and diff-check passed; exact feature CI `43894615269` and integration CI `43895224086` are green.
- Fast-forward integrated at `379577f61459ba366cc3bc456826ca3564eff4cd`. `main` remains unchanged. The M1 PR #12 P2 follow-up is separate and still under re-review/self-verification.

## 2026-11-16 — PR #12 follow-up re-review and Semgrep triage

- Commander verified the `19fc8e6` follow-up locally and exact CI `43895251495` passed, closing the unsupported-platform inventory defect. Independent re-review found stale “Opted in this session” and “Session-only consent” wording (P2); PR comment `6921143930` records the issue.
- Semgrep reports eight candidate matches for the M1 release code/tests. OpenCode is triaging them and fixing the test temp-root plus any confirmed production risk on isolated branch `fix/m1-semgrep-tempfiles`; PR #12 remains draft pending that work, fresh review, and exact-head CI.

## 2026-11-16 — Keyring storage review blockers

- Experimental keyring storage `e41e88e` passed commander-run Linux/Windows gates and exact feature CI `43904715725`. Independent review found two P2s: identity secret/public-key consistency on reopen and ignored metadata-revocation write failure.
- AGY is addressing the findings on isolated branch `fix/secure-keyring-p2`; no integration or production-security claim until TDD, independent re-review and exact CI pass. No real keyring, live network, model or GPU work was performed.

## 2026-11-16 — Keyring test seam must not be public by default

- Commander inspection found that `KeyringBackend`, `KeyringLocator`, and `TrustStore::open_with_keyring_backend` remain public despite `#[doc(hidden)]`, contrary to the task constraint against a production-default alternate-storage escape hatch. AGY is restricting this seam while preserving injected tests and addressing the two reviewed P2s on `fix/secure-keyring-p2`.

## 2026-11-16 — M1 temp-file and marker-copy follow-up

- OpenCode pushed isolated candidate `1aab6d5`; it fixes stale opt-in/consent wording and all three temp-directory heuristics, including a real Linux ETXTBSY test race (20 focused repeats passed after serialization). Exact CI `43906258084` is green; commander reran the full Linux/Windows-target gate chain and full changed-Rust Semgrep.
- Five remaining Semgrep matches are limited to audited `unsafe` pipe reads/fcntl and CLI argument dispatch. Independent final review found no actionable findings; PR #12 was updated and merged into main.

## 2026-11-16 — M1 Linux device GUI merged to main

- PR #12 final candidate `1aab6d5a079052d62be50a6acda131f428d81df4` passed exact Ubuntu/Windows PR checks `43907795006` and `43907803373`, commander local full gates, and independent defect-first review (no findings). Semgrep's five remaining heuristics were triaged as scoped pipe-syscall/CLI-argument false positives; no temp-dir or secrets findings remained.
- Merged PR #12 into `main` at `3606637ca41d07f60562d536a8b9559f751c1f24` on 2026-11-16 16:33:27 UTC. Exact merge-commit CI `43908805208` is still running; main's release gate is not fully green until it succeeds.
- The release slice contains passive Linux local inventory, a pre-alpha local-device GUI and desktop packaging only. No live discovery, pairing, model/inference, GPU workload or service interruption.

## 2026-11-16 — M1 merge and exact main CI verified

- Exact merge-commit CI `43908805208` passed Ubuntu/Windows for `main` at `3606637ca41d07f60562d536a8b9559f751c1f24`. PR #12 is verified merged; the release branch remains available.
- Integration checkpoint `d4b4a65` is green on exact CI `43909097463`. No GPU/model inference or live discovery was performed.

## 2026-11-16 — Keyring hardening follow-up pushed

- AGY's fix for the two independent keyring P2s plus the public test-seam issue is committed and pushed at `60c2fa1` on isolated `fix/secure-keyring-p2`. Commander verified workspace/unit/integration/doctests, fmt, Linux Clippy, GUI build, Windows GNU check/Clippy, diff-check and targeted Semgrep (0 findings).
- Exact feature CI `43910640643` and independent security re-review are pending. The change is not integrated; no live keyring, network, model, or GPU activity was performed.

## 2026-11-16 — Keyring follow-up exact feature CI passed

- Exact Ubuntu/Windows feature CI `43910640643` passed on `60c2fa1`; the independent security re-review has completed but its result is awaited. Candidate remains isolated and must not be integrated before a clean re-review.
- The next safe step is to review the re-review result, then rebase onto the latest `integrate/fastlink-team` and rerun full local/exact CI gates. No real keyring, network, model, or GPU activity occurred.

## 2026-11-16 — Keyring candidate rebased and revalidated

- Integration `abe47dd` passed exact CI `43911743846`. Replayed the two keyring commits onto it in a separate worktree/branch, preserving the original `fix/secure-keyring-p2` branch; candidate `58545949e37cb93cd4feaec1a112710a5dcb0c4c` is pushed separately.
- Full local Linux/Windows-target gates and targeted Semgrep passed on the rebased candidate. Exact CI `43912572730` and a fresh independent full-diff security review are in progress; do not integrate before both complete cleanly.
- Existing unrelated dirty files in the repository root worktree remain untouched. No real keyring, network discovery, model, or GPU activity occurred.

## 2026-11-16 — Keyring remediation integrated

- PR #13 merged into `integrate/fastlink-team` at `a5f1cd7`; independent security review found no findings. Exact PR checks `43914199921`, feature CI `43913532911`, and exact integration merge CI `43914979069` passed on the reviewed candidate/merge.
- The identity mismatch, ignored revocation persistence failure and public injected-backend seam issues are closed in the integrated candidate. Main remains at the separate M1 merge; no keyring code was merged to main.
- Residual: no live Secret Service/D-Bus test, and no production-security claim. No live keyring, network discovery, model, GPU, or service interruption occurred.

## 2026-11-18 — Managed local Chat

- In Devices and models, register a GGUF and choose Confirm load. In Chat, enter the llama-server executable path and choose Load model (CPU). Send a prompt once the model is ready. Stop server releases the managed process and weights.
- Chat accepts standalone completion prompts. The token rate appears when llama.cpp returns final decode timings.

## 2026-11-18 — SSH-backed worker sessions

- Linux Chat accepts explicit worker endpoints and opens managed loopback-only SSH forwards using already trusted host keys. The coordinator uses llama.cpp RPC with automatic placement and streams through the same Chat view.
- The worker RPC processes are prepared separately. Forward loss tears down the managed session, and the operator can reload after the worker recovers.
