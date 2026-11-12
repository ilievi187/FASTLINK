# FASTLINK GPU inference adapter — verified local implementation (2026-11-06)

## Implementation

An **independent experimental GPU RPC adapter** has been implemented in a
separate llama.cpp worktree on the same machine, pinned to upstream commit
`0ec4027`. The implementation commits are `1f4f6f6` and `a14de85`
on `feat/fastlink-rpc-adapter` in that llama.cpp checkout.

The adapter is an opt-in extension of **llama.cpp RPC v8.0.1**. It borrows the
64 KiB RAW/RLE lossless chunk encoding concept from FASTLINK v0.1, but does
**not** currently use the FASTLINK file-sync protocol directly. It is an
experimental tensor transport optimization, not a WiCi replacement.

The server and client negotiate the extension at HELLO. New packets use a
bounded RAW/RLE tensor frame and SHA-256 authentication of the original RPC
tensor header and data. Unsupported peers, small messages and incompressible
payloads use stock RPC. GPU graph compute remains ggml's own RPC operation.

## Actual machine validation

- NightKnightPC: Vulkan RX 7900 XT, release Linux client build — passed.
- detti: Vulkan RX 6600 XT, Windows cross-compiled RPC binary/DLL — passed.
- 64 KiB codec, malformed and mutated frames, truncation, zero and random
  fallback, SHA-256: pass.
- 1 MiB tensor over encrypted SSH tunnel: 8,689 bytes compressed payload on
  all-zero data, byte-exact remote tensor readback, Vulkan SCALE graph execute
  and re-execute: pass.
- 5 repeated GPU smoke trials: standard RPC median 0.764 s; opt-in
  FASTLINK-RPC median 0.758 s. These tests prove compatibility, **not a
  measurable inference speedup**.
- Heretic Qwen3.8 27B real inference with remote Vulkan RPC: HTTP 200,
  correct 7 × 8 result with 4K context.
- Heretic 27B on new opt-in 128K server: health OK, `n_ctx_slot=131072`,
  HTTP 200 and correct 17 × 19 answer.
- New JCode profile `heretic27-fastlink-rpc-v1`: completed local prompt,
  returned `LOCAL HERETIC READY` with exit code 0.
- Existing JCode launcher now targets the new 128K instance, while the
  previous launcher and configuration are backed up.

## Operational boundaries

- New server is local only on 127.0.0.1:18881.
- New Windows RPC worker is local only on 127.0.0.1:50123; SSH tunnel on
  NightKnightPC uses 127.0.0.1:15123.
- Previous Heretic launcher and Windows worker binaries have not been
  overwritten. No external RPC port has been opened.
- New command: `heretic128-fastlink-v1 start|status|stop`.
- JCode command: `jcode-heretic128`.
- The experimental code is committed locally; no claim of push or merge.

## Known work remaining

- FASTLINK v0.1 BLAKE3 cross-session cache is **not** integrated with
  ggml RPC. Most quantized LLM weight data is incompressible, so the
  FASTLINK-RPC RLE extension does not guarantee an inference speedup.
- Full Windows Vulkan backend has not been rebuilt from scratch in the
  cross-compiler; the running Windows adapter reuses the existing
  same-revision Vulkan DLL, tested on RX 6600 XT.
- Further comparative inference throughput and latency benchmarking against
  the old Heretic binary is necessary before claiming performance gains.
- The old launcher backup and the separate FASTLINK-RPC branch should be
  preserved until stability is demonstrated over longer GPU sessions.
