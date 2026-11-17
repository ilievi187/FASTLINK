# Development progress

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
