# FASTLINK Wire Protocol v0.1 (experimental)

This document specifies the CPU-only prototype. It is **not** the future GPU/RPC protocol.

All integers are unsigned **little-endian**. Connections are short-lived and each handles one SYNC operation.

## Handshake

Client sends `FLNK` (4 bytes) + `version: u16=1`. Server responds with the same six bytes. A mismatch aborts the connection.

## SYNC request

| Field | Wire type |
| --- | --- |
| message ID | `u8=1` |
| filename byte length | `u16` |
| file name | UTF-8 bytes, <=240, basename only |
| total file size | `u64` |
| fixed chunk size | `u32=65536` |
| chunk count | `u32` |
| chunk hashes | `count × [u8;32]`, BLAKE3 |

The server verifies message bounds, hash count and file size and sends the missing data requests.

## NEED response

`missing_count: u32` then `missing_count × index:u32`, strictly ascending. A chunk hash can occur many times in the manifest, but its payload need only be sent once.

## CHUNK payloads (one per requested index)

`index:u32` + `codec:u8` + `encoded_length:u32` + `encoded_bytes`.

The frame is those 9 bytes followed by the payload. The integrated client writes the fields in that order. The byte layout is unchanged from v0.1.

Codec 0 is RAW. Codec 1 is a simple RLE stream of `(run:u8, value:u8)` pairs. Codec 2 is LZ4 and codec 3 is ZSTD. All four are lossless and the decoded data is BLAKE3-verified against the manifest.

The server persists chunks in a content-addressable cache, reconstructs the file into a temporary output, validates every cached chunk, atomically renames the output, then returns `status:u8=0`.

## Boundaries, caveats and invariants

- Chunk bytes are 65,536; at most 1,000,000 chunks, at most 65,536,000,000 logical bytes per file.
- Received encoded chunks are capped at 131,072 bytes. RAW length must equal the logical chunk length. RLE length must be even and at most twice the logical chunk length. LZ4 and ZSTD payloads stay inside the 131,072-byte cap and must expand to the logical length. Any other codec byte is rejected before the payload length is used to allocate.
- On startup and on session start the server deletes uncommitted `.partial-*` chunk temps and `.fastlink-inprogress-*` output temps, and WP-A `.partial` / `.fastlink-partial` session temps. It does not rename those files into the cache or the output. Verified cache objects and already committed outputs stay. There is no resume token. A later connection may resend the manifest; the server then asks only for chunks that are not already cached.
- File names are restricted to basenames; no path traversal or nested output paths.
- Deduplication is exact. Reused hashes must match the stored content.
- RLE decompression rejects invalid lengths and overlong runs.
- This is **not encrypted or authenticated**; only loopback listeners are allowed in the prototype.
- The server accepts concurrent loopback sessions. There is still no durable resume token, no on-disk cache quota, and no GPU handle. `MemoryBudgetCache` is a separate in-memory payload cache with FIFO eviction. It does not delete durable chunk files.
- A successful transfer is byte-for-byte lossless; a disconnected transfer may leave verified cache chunks but never commits a partial output as final.

## Retry, Timeout, and Reconnect Policy (`FL3-A5` / `FL3-C3`)

Implemented in `src/retry.rs` (`RetryPolicy`, `LinkEstimate`, `send_file_with_policy`) and re-exported from `fastlink`:

| Constant / Formula | Value | Purpose |
| --- | --- | --- |
| `POLICY_MAX_RETRIES_PER_RPC` | `2` (`3` attempts per cycle) | Covers transient per-RPC loss $p \le 0.05$ ($1 - p^3 = 0.999875$ completion rate) |
| `POLICY_TIMEOUT_FLOOR_MS` | `50.0` ms | Minimum per-attempt socket timeout floor |
| `POLICY_TIMEOUT_MULTIPLIER` | `3.0` | Safety factor applied over expected transfer duration |
| `attempt_timeout_ms` | $\max(50.0,\; 3.0 \times (\text{queue\_ms} + \text{rtt\_ms} + \text{ser\_ms} + \text{wire\_ms}))$ | Recomputed dynamically from the current `LinkEstimate` (`wire_ms = 1000 * mib / throughput_mib_s`) |
| `POLICY_RECONNECT_BACKOFF_MS` | `[100, 400, 1600]` ms | Exponential reconnect backoff schedule on connection drop or per-cycle timeout exhaustion |
| `POLICY_MAX_RECONNECTS` | `3` | Maximum reconnect cycles before terminal abort |
| Nominal `LinkEstimate` | `rtt = 1.0 ms`, `throughput = 95.3674 MiB/s`, `ser = 0.05 ms`, `queue = 0.2 ms` | Reference 1 Gbps boundary hop (`f = 1.0`: `50 ms` for 8 KiB / 64 KiB, `130 ms` for 4 MiB ubatch; `f = 0.01`: `200 ms` for 64 KiB, `12,587 ms` for 4 MiB ubatch) |
| Terminal failure rule | Zero partial output committed | Resumes by manifest across retries/reconnects (re-sending only uncached chunks); if all retries and reconnects fail, aborts cleanly with zero committed or leftover `.fastlink-partial` files in `outputs/` |

The v0.1 format may change. Do not rely on cross-version compatibility yet.
