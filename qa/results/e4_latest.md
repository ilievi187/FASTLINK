# FL3-E4 CPU-only benchmark + reliability harness results

- commit: `e9c96ad` (feat/fastlink-v3-network-sim)
- timestamp_utc: 20261107T193002Z
- profile: release (loopback, CPU-only, no GPU/SSH)
- deterministic: seed=42 throughput=8388608B fault=4194304B delta=1.0%@1048576

## Transfer (loopback)
| scenario | byte_exact | chunks_transferred | encoded_payload_bytes | wall_ms | MiB/s (wall) |
|---|---:|---:|---:|---:|---:|
| cold | True | 128 | 8388608 | 109.191 | 73.266439 |
| warm | True | 0 | 0 | 55.858 | 143.221057 |
| delta | True | 2 | 131072 | 95.447 | 83.816351 |

## Codec CPU cost (RAW/RLE only, this worktree)
```json
{
  "codec_scope": "RAW and RLE only; Zstd/LZ4 adaptive selection NOT in this worktree (WP-A branch)",
  "iters_per_pattern": 4096,
  "mib_per_pattern": 256.0,
  "patterns": [
    {
      "codec": 1,
      "decode_mib_s": 71812.58,
      "decode_ns_per_mib": 13925.1,
      "encode_mib_s": 2862.666,
      "encode_ns_per_mib": 349324.7,
      "encoded_bytes": 516,
      "iters": 4096,
      "mib": 256.0,
      "pattern": "sparse_64k_rle"
    },
    {
      "codec": 0,
      "decode_mib_s": 69395.876,
      "decode_ns_per_mib": 14410.1,
      "encode_mib_s": 977.177,
      "encode_ns_per_mib": 1023356.1,
      "encoded_bytes": 65536,
      "iters": 4096,
      "mib": 256.0,
      "pattern": "mixed_64k"
    }
  ],
  "provenance": "measured on this host via rustc -O linked to compiled fastlink lib (RAW/RLE only, v0.1 this worktree)",
  "raw_stdout": "pattern=sparse_64k_rle codec=1 encoded_bytes=516 iters=4096 mib=256.000 encode_ns_per_mib=349324.7 decode_ns_per_mib=13925.1 encode_mib_s=2862.666 decode_mib_s=71812.580\npattern=mixed_64k codec=0 encoded_bytes=65536 iters=4096 mib=256.000 encode_ns_per_mib=1023356.1 decode_ns_per_mib=14410.1 encode_mib_s=977.177 decode_mib_s=69395.876",
  "status": "ok"
}
```

## Kill/resume fault (byte-exact after resume)
```json
{
  "byte_exact_after_resume": true,
  "chunk_files_after_kill": 49,
  "chunks_cached_at_kill_poll": 46,
  "deterministic_params": {
    "chunk_bytes": 65536,
    "file_bytes": 4194304,
    "kill_threshold_chunks": 32,
    "seed": 61483,
    "total_chunks": 64
  },
  "honest_note": "Resume-by-manifest is exercised: server requests only cache-missing chunks on retry. This worktree's v0.1 protocol supports it implicitly via content-addressed cache checks; no explicit session-reattach token exists in this branch.",
  "method": "kill server after K=total/2 chunks cached; restart same cache; re-send manifest (resume-by-manifest)",
  "mid_transfer_kill_achieved": true,
  "out_sha256": "9992de835ec172ae4db717385c7037557fb51387ef7fdf5bb29d9e4651cb9c19",
  "resume_send": {
    "chunks_reused": 49,
    "chunks_total": 64,
    "chunks_transferred": 15,
    "client_elapsed_ms": 90,
    "encoded_payload_bytes": 983040,
    "file_bytes": 4194304,
    "ok": true,
    "output": "FASTLINK sync complete: fault-src.bin\n  file_bytes: 4194304\n  chunks_total: 64\n  chunks_reused: 49\n  chunks_transferred: 15\n  raw_missing_bytes: 983040\n  encoded_payload_bytes: 983040\n  payload_bytes_avoided_vs_full_file: 3211264\n  elapsed_ms: 90\n  note: payload counts exclude wire framing, hashes, and TCP overhead",
    "returncode": 0,
    "throughput_mib_s_wall": 43.863242,
    "wall_ms": 91.193
  },
  "src_sha256": "9992de835ec172ae4db717385c7037557fb51387ef7fdf5bb29d9e4651cb9c19",
  "status": "ok"
}
```

## Summary
```json
{
  "all_required_pass": true,
  "cold_byte_exact": true,
  "delta_byte_exact": true,
  "kill_resume_byte_exact": true,
  "kill_resume_mid_kill_achieved": true,
  "warm_byte_exact": true,
  "warm_zero_transferred": true
}
```

**Honesty:** Measured on loopback with this worktree's v0.1 RAW/RLE PoC. Not a GPU/RPC/remote benchmark. Zstd/LZ4 adaptive codec is out of scope for this branch.
