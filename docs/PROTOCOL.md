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

The client submits that 9-byte prefix and the payload in one vectored write. The byte layout is the same.

Codec 0 is RAW. Codec 1 is a simple RLE stream of `(run:u8, value:u8)` pairs. Codecs are lossless and the decoded data is BLAKE3-verified against the manifest.

The server persists chunks in a content-addressable cache, reconstructs the file into a temporary output, validates every cached chunk, atomically renames the output, then returns `status:u8=0`.

## Boundaries, caveats and invariants

- Chunk bytes are 65,536; at most 1,000,000 chunks, at most 65,536,000,000 logical bytes per file.
- Received encoded chunks are capped at 131,072 bytes. RAW length must equal the logical chunk length. RLE length must be even and at most twice the logical chunk length. Any other codec byte is rejected before the payload length is used to allocate.
- On session start the server deletes uncommitted `.partial-*` chunk temps and `.fastlink-inprogress-*` output temps. It does not rename those files into the cache or the output. Verified cache objects and already committed outputs stay. There is no resume token.
- File names are restricted to basenames; no path traversal or nested output paths.
- Deduplication is exact. Reused hashes must match the stored content.
- RLE decompression rejects invalid lengths and overlong runs.
- This is **not encrypted or authenticated**; only loopback listeners are allowed in the prototype.
- No durable resumable session identifiers, on-disk cache quota, multi-client concurrency, or GPU handles exist yet. `MemoryBudgetCache` is a separate in-memory payload cache with FIFO eviction. It does not delete durable chunk files.
- A successful transfer is byte-for-byte lossless; a disconnected transfer may leave verified cache chunks but never commits a partial output as final.

The v0.1 format may change. Do not rely on cross-version compatibility yet.
