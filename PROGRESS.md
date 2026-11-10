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
  reconstructs on receive with per-chunk `decode_chunk` verification.
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

## 2026-11-10 — O-3 async Pool + batched bench, transport review + benchmark refresh

- Reviewed `src/transport.rs`: `send_tensor`/`receive_tensor` over loopback TCP,
  framed with the existing `frame_tensor`, single `write_all`, per-chunk
  `decode_chunk` verification. Correctness confirmed: header order matches,
  `count == chunk_count(total_len)` validated, `encoded_len` bounds-checked
  (1..=MAX_ENCODED_BYTES), unknown codec / odd RLE / size-mismatch all rejected,
  truncated read returns `Underflow`. Edge cases: empty tensor rejected by
  `frame_tensor`; `total_len == 0` early-returned; trailing bytes after `count`
  chunks are ignored (acceptable). Perf: preallocated `wire` Vec, coalesced write,
  `Pool::batch_send` amortises the 12-byte header + 5-byte per-chunk header across
  multiple tensors.
- Known WIP: `benches/rpc_latency_batched.rs` has clippy findings (unused
  `CHUNK_BYTES` import, unused `mut` on a connect sock) and the ack is read on a
  freshly connected `sock` while data is sent on the pool client, so the batched
  bench can hang waiting for the ACK; these are pre-existing, not touched here.
- Refreshed transport latency/throughput numbers in
  `docs/BENCHMARKS.md` and `README.md` (added a Raw TCP vs framed comparison);
  numbers come from one loopback machine and are not a guarantee.

**Measured (canonical optimized loopback snapshot `rpc-latency-linux-2026-11-10-224820-8fc776c`):** raw/framed 64 KiB RTT min/median/mean 0.016/0.018/0.019 and 0.093/0.102/0.108 ms; patterned 8 MiB raw TCP 8,605.5 MB/s vs FASTLINK RAW 564.9 MB/s raw-equivalent / 565.0 MB/s wire (+652 B); uniform raw TCP 7,758.1 MB/s vs FASTLINK RLE 1,869.6 MB/s raw-equivalent / 14.9 MB/s wire; connection setup0.033ms. Single local Linux run, no speed guarantee or GPU/inference claim. Full provenance and methodology in `benches/results_rpc_latency.json` and `docs/BENCHMARKS.md`.

**Explicit limitation:** loopback only, unauthenticated, unencrypted; single TCP connection per scenario. Transport measurement, not end-to-end RPC or GPU result. Batched `Pool` numbers pending.
