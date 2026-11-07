# FL3-B5 integration loopback benchmark

CPU-only. Every server and client address in the commands below is `127.0.0.1`. No GPU was used.

## Command

```
python3 qa/b5_bench.py
```

The script builds `--release` in this worktree and in a detached worktree at
`/tmp/fl3b5-main` (`dcaa5116b74a11d893377c97fba9a74101a16cae`), runs the transfers, writes this file, and removes that worktree.

## Host

- integrate HEAD at build time: `2bd9698bc5105aea960c96e3362c49ae75f009c2`
- main checkout: `dcaa5116b74a11d893377c97fba9a74101a16cae`
- started: 2026-11-07T22:36:39+0200
- finished: 2026-11-07T22:36:58+0200
- After this run, `61a9e43` added only `qa/results/e7_integrated_reliability.md`. It does not change the measured binary.
- uname: Linux 7.2.15-1-omarchy #1 SMP PREEMPT_DYNAMIC Mon, 02 Nov 2026 10:32:17 +0000 x86_64
- nproc: 16
- profile: release

## Method

- 8 MiB and 64 MiB files use the `qa/e4_harness.py` mixed generator, seed 42.
- Delta mutates 1% of bytes at offset 1048576 with the e4 mutation (`seed ^ 0xDE17A`).
- Cold, warm, and delta each start a new `fastlink serve` on the same cache directory.
- `wall_ms` is `time.perf_counter` around the `fastlink send` process.
- `MiB/s (wall)` is `file_bytes / 1048576 / (wall_ms / 1000)`.
- Each sized scenario is one run.
- Client `elapsed_ms`, `logical_mib_per_s`, and `wire_mib_per_s` are copied from CLI stdout when that binary prints them.
- Main `dcaa511` prints encoded payload bytes and elapsed_ms. It does not print total wire bytes.
- Codec counts are a second `encode_chunk` pass on the same files (unique BLAKE3 chunk digests), not a capture of the timed send.
- Main `encode_chunk` chooses RLE only when the encoded length plus 32 is below the raw length. A matching `encoded_bytes` value is that run's codec mix.
- Integration `encode_chunk` calls `encode_chunk_adaptive`, which times RLE, LZ4, and sometimes ZSTD. A later pass can encode a different byte count. The CLI does not print per-chunk codec IDs.
- Warm sends no chunks.

## Published e4 reference (not re-measured)

From `qa/results/e4_latest.md` on network-sim `e9c96ad`: one 8 MiB mixed seed-42 loopback release run.
Cold wall 73.266439 MiB/s (109.191 ms, encoded payload 8388608, 128 chunks).
Warm wall 143.221057 MiB/s (55.858 ms, encoded payload 0, 0 chunks).
That run is a different binary (network-sim RAW/RLE) from both trees measured below.

## Transfer table

| tree | size | scenario | byte_exact | chunks_transferred | encoded_payload_bytes | wire_bytes_total | wall_ms | client_elapsed_ms | MiB/s wall | logical_mib_per_s | wire_mib_per_s |
|---|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| integrate | 8388608 | cold | True | 128 | 8286801 | 8292605 | 159.479 | 158 | 50.163251 | 50.505 | 49.927 |
| integrate | 8388608 | warm | True | 0 | 0 | 4140 | 55.801 | 54 | 143.367038 | 146.304 | 0.072 |
| integrate | 8388608 | delta | True | 2 | 131072 | 135244 | 98.563 | 97 | 81.166318 | 82.128 | 1.324 |
| main | 8388608 | cold | True | 128 | 8388608 | not reported | 153.209 | 151 | 52.21634 | not reported | not reported |
| main | 8388608 | warm | True | 0 | 0 | not reported | 55.126 | 54 | 145.122018 | not reported | not reported |
| main | 8388608 | delta | True | 2 | 131072 | not reported | 95.182 | 93 | 84.049375 | not reported | not reported |
| integrate | 67108864 | cold | True | 1024 | 66254451 | 66300575 | 575.744 | 574 | 111.160524 | 111.403 | 110.061 |
| integrate | 67108864 | warm | True | 0 | 0 | 32812 | 91.711 | 90 | 697.847907 | 707.169 | 0.346 |
| integrate | 67108864 | delta | True | 11 | 720896 | 753857 | 151.225 | 149 | 423.210792 | 427.169 | 4.799 |
| main | 67108864 | cold | True | 1024 | 67108864 | not reported | 253.14 | 251 | 252.824213 | not reported | not reported |
| main | 67108864 | warm | True | 0 | 0 | not reported | 100.607 | 99 | 636.135888 | not reported | not reported |
| main | 67108864 | delta | True | 11 | 720896 | not reported | 137.476 | 136 | 465.53693 | not reported | not reported |

## Second encode_chunk pass (not a capture of the timed send)

The timed CLI does not print per-chunk codec IDs.
This table is a later `encode_chunk` pass on the same files. It counts the first occurrence of each BLAKE3 chunk digest.
Warm rows are empty because the timed send transferred 0 chunks.
Main `dcaa511` chooses RLE only when the encoded length plus 32 is below the raw length, with no CPU timer. When `encoded_bytes` equals the timed `encoded_payload_bytes`, that histogram is the timed codec mix.
Integration `encode_chunk` times CPU inside `encode_chunk_adaptive`. A row whose `encoded_bytes` differs from the timed payload is a different sample, not the timed codec mix.

The two non-zero gaps are integration cold only: 8 MiB second pass 8281960 versus timed payload 8286801 (difference -4841); 64 MiB second pass 66253387 versus timed payload 66254451 (difference -1064). Those histograms are not the timed codec mix. The timed integration cold payloads are below the raw file size (8 MiB by 101807 bytes, 64 MiB by 854413 bytes), so at least one chunk on each cold send was not RAW. The timed codec IDs were not printed.

Delta rows match the timed payload and equal `chunks * 65536`. `encode_chunk_adaptive` keeps a compressed candidate only when `encoded_len + min_byte_savings` (32) is still below the raw chunk length, so a transferred payload that equals `chunks * 65536` was RAW on every chunk. Main rows are all RAW and match the timed payload.

| tree | size | scenario | RAW | RLE | LZ4 | ZSTD | chunks | encoded_bytes | timed_encoded_payload_bytes | encoded_bytes_minus_timed |
|---|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|
| integrate | 8388608 | cold | 81 | 0 | 47 | 0 | 128 | 8281960 | 8286801 | -4841 |
| integrate | 8388608 | warm | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| integrate | 8388608 | delta | 2 | 0 | 0 | 0 | 2 | 131072 | 131072 | 0 |
| main | 8388608 | cold | 128 | 0 | 0 | 0 | 128 | 8388608 | 8388608 | 0 |
| main | 8388608 | warm | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| main | 8388608 | delta | 2 | 0 | 0 | 0 | 2 | 131072 | 131072 | 0 |
| integrate | 67108864 | cold | 646 | 0 | 378 | 0 | 1024 | 66253387 | 66254451 | -1064 |
| integrate | 67108864 | warm | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| integrate | 67108864 | delta | 11 | 0 | 0 | 0 | 11 | 720896 | 720896 | 0 |
| main | 67108864 | cold | 1024 | 0 | 0 | 0 | 1024 | 67108864 | 67108864 | 0 |
| main | 67108864 | warm | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| main | 67108864 | delta | 11 | 0 | 0 | 0 | 11 | 720896 | 720896 | 0 |

## Sustained 20x1 MiB (integration build only)

Generator and seed match `qa/e5_harness.py` (`0xE5E5` plus the file index). N is 20, not 50.
One server stays up for the cold pass and the warm pass. Aggregate MiB/s is `20 / sum(wall seconds)`.
`all_chunks_transferred_zero` is true only when every send in that phase reported `chunks_transferred` 0.

| phase | all_ok | all_byte_exact | all_chunks_transferred_zero | wall_ms min | mean | p50 | max | aggregate MiB/s wall | per-run MiB/s median |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| cold | True | True | False | 93.965 | 122.271 | 135.51 | 139.114 | 8.178584 | 7.39459 |
| warm | True | True | True | 44.402 | 47.469 | 45.615 | 85.792 | 21.06658 | 21.957271 |

Server VmRSS after dropping the first 2 samples: min 3396 kB, max 4096 kB, delta 700 kB.

Per-run wall_ms and client elapsed_ms:

| phase | idx | wall_ms | elapsed_ms | chunks_transferred | encoded_payload_bytes | wire_bytes_total | byte_exact | rss_kb |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| cold | 0 | 94.538 | 93 | 16 | 653688 | 654454 | True | 3216 |
| cold | 1 | 94.833 | 93 | 16 | 658419 | 659185 | True | 3308 |
| cold | 2 | 94.394 | 93 | 16 | 653071 | 653837 | True | 3396 |
| cold | 3 | 107.061 | 94 | 16 | 656027 | 656793 | True | 3492 |
| cold | 4 | 93.965 | 93 | 16 | 652288 | 653054 | True | 3584 |
| cold | 5 | 95.353 | 94 | 16 | 645599 | 646365 | True | 3696 |
| cold | 6 | 136.5 | 134 | 16 | 667708 | 668474 | True | 3812 |
| cold | 7 | 135.039 | 133 | 16 | 659949 | 660715 | True | 3920 |
| cold | 8 | 136.015 | 134 | 16 | 655877 | 656643 | True | 3944 |
| cold | 9 | 136.162 | 135 | 16 | 661774 | 662540 | True | 3972 |
| cold | 10 | 139.114 | 137 | 16 | 653542 | 654308 | True | 3996 |
| cold | 11 | 136.165 | 134 | 16 | 646539 | 647305 | True | 4016 |
| cold | 12 | 95.836 | 94 | 16 | 645487 | 646253 | True | 4072 |
| cold | 13 | 135.234 | 134 | 16 | 643577 | 644343 | True | 4072 |
| cold | 14 | 135.51 | 134 | 16 | 645102 | 645868 | True | 4092 |
| cold | 15 | 134.017 | 132 | 16 | 667634 | 668400 | True | 4092 |
| cold | 16 | 136.293 | 135 | 16 | 655389 | 656155 | True | 4092 |
| cold | 17 | 136.304 | 135 | 16 | 645362 | 646128 | True | 4092 |
| cold | 18 | 137.241 | 133 | 16 | 664061 | 664827 | True | 4096 |
| cold | 19 | 135.837 | 134 | 16 | 662508 | 663274 | True | 4096 |
| warm | 0 | 45.615 | 44 | 0 | 0 | 558 | True | 4096 |
| warm | 1 | 45.83 | 44 | 0 | 0 | 558 | True | 4096 |
| warm | 2 | 45.827 | 44 | 0 | 0 | 558 | True | 4096 |
| warm | 3 | 45.617 | 44 | 0 | 0 | 558 | True | 4096 |
| warm | 4 | 46.631 | 45 | 0 | 0 | 558 | True | 4096 |
| warm | 5 | 45.174 | 44 | 0 | 0 | 558 | True | 4096 |
| warm | 6 | 85.792 | 44 | 0 | 0 | 558 | True | 4096 |
| warm | 7 | 44.551 | 43 | 0 | 0 | 558 | True | 4096 |
| warm | 8 | 44.865 | 43 | 0 | 0 | 558 | True | 4096 |
| warm | 9 | 44.402 | 43 | 0 | 0 | 558 | True | 4096 |
| warm | 10 | 46.142 | 44 | 0 | 0 | 558 | True | 4096 |
| warm | 11 | 44.92 | 43 | 0 | 0 | 558 | True | 4096 |
| warm | 12 | 46.353 | 45 | 0 | 0 | 558 | True | 4096 |
| warm | 13 | 45.77 | 44 | 0 | 0 | 558 | True | 4096 |
| warm | 14 | 45.23 | 44 | 0 | 0 | 558 | True | 4096 |
| warm | 15 | 45.174 | 43 | 0 | 0 | 558 | True | 4096 |
| warm | 16 | 45.265 | 44 | 0 | 0 | 558 | True | 4096 |
| warm | 17 | 46.038 | 44 | 0 | 0 | 558 | True | 4096 |
| warm | 18 | 45.543 | 44 | 0 | 0 | 558 | True | 4096 |
| warm | 19 | 44.632 | 43 | 0 | 0 | 558 | True | 4096 |

## Comparison with the published e4 rates and with main dcaa511

- integrate 8 MiB cold wall MiB/s vs e4 cold 73.266439: measured 50.163251 (difference -23.103188)
- integrate 8 MiB warm wall MiB/s vs e4 warm 143.221057: measured 143.367038 (difference 0.145981)
- integrate 8 MiB cold wall MiB/s vs main dcaa511: measured 50.163251 (main 52.21634, difference -2.053089)
- integrate 8 MiB warm wall MiB/s vs main dcaa511: measured 143.367038 (main 145.122018, difference -1.75498)
- integrate 8 MiB delta wall MiB/s vs main dcaa511: measured 81.166318 (main 84.049375, difference -2.883057)
- integrate 64 MiB cold wall MiB/s vs main dcaa511: measured 111.160524 (main 252.824213, difference -141.663689)
- integrate 64 MiB warm wall MiB/s vs main dcaa511: measured 697.847907 (main 636.135888, difference 61.712019)
- integrate 64 MiB delta wall MiB/s vs main dcaa511: measured 423.210792 (main 465.53693, difference -42.326138)
- integrate 20x1 MiB cold aggregate wall MiB/s vs e4 single 8 MiB cold 73.266439 (different workload): measured 8.178584 (difference -65.087855)
- integrate 20x1 MiB warm aggregate wall MiB/s vs e4 single 8 MiB warm 143.221057 (different workload): measured 21.06658 (difference -122.154477)

## Raw CLI stdout

### integrate 8388608 cold

```
/home/nightknight/Projects/FASTLINK/.worktrees/fastlink-v3-integrate/target/release/fastlink send /tmp/fl3b5-data/size-8388608/data.bin 127.0.0.1:56881
FASTLINK sync complete: data.bin
  file_bytes: 8388608
  chunks_total: 128
  chunks_reused: 0
  chunks_transferred: 128
  raw_missing_bytes: 8388608
  encoded_payload_bytes: 8286801
  handshake_wire_bytes: 12
  manifest_wire_bytes: 4123
  need_wire_bytes: 516
  chunk_framing_wire_bytes: 1152
  status_wire_bytes: 1
  headers_and_framing_wire_bytes: 5804
  wire_bytes_tx: 8292082
  wire_bytes_rx: 523
  wire_bytes_total: 8292605
  payload_bytes_avoided_vs_full_file: 101807
  wire_bytes_avoided_vs_full_file: 96003
  handshake_us: 68
  manifest_us: 82451
  transfer_us: 73149
  elapsed_us: 158401
  elapsed_ms: 158
  wire_mib_per_s: 49.927
  logical_mib_per_s: 50.505
  note: wire_bytes_total includes handshake, manifest, NEED list, chunk framing, encoded payloads, and status (excludes TCP/IP headers)
```

### integrate 8388608 warm

```
/home/nightknight/Projects/FASTLINK/.worktrees/fastlink-v3-integrate/target/release/fastlink send /tmp/fl3b5-data/size-8388608/data.bin 127.0.0.1:48931
FASTLINK sync complete: data.bin
  file_bytes: 8388608
  chunks_total: 128
  chunks_reused: 128
  chunks_transferred: 0
  raw_missing_bytes: 0
  encoded_payload_bytes: 0
  handshake_wire_bytes: 12
  manifest_wire_bytes: 4123
  need_wire_bytes: 4
  chunk_framing_wire_bytes: 0
  status_wire_bytes: 1
  headers_and_framing_wire_bytes: 4140
  wire_bytes_tx: 4129
  wire_bytes_rx: 11
  wire_bytes_total: 4140
  payload_bytes_avoided_vs_full_file: 8388608
  wire_bytes_avoided_vs_full_file: 8384468
  handshake_us: 853
  manifest_us: 44122
  transfer_us: 7487
  elapsed_us: 54680
  elapsed_ms: 54
  wire_mib_per_s: 0.072
  logical_mib_per_s: 146.304
  note: wire_bytes_total includes handshake, manifest, NEED list, chunk framing, encoded payloads, and status (excludes TCP/IP headers)
```

### integrate 8388608 delta

```
/home/nightknight/Projects/FASTLINK/.worktrees/fastlink-v3-integrate/target/release/fastlink send /tmp/fl3b5-data/size-8388608/data-delta.bin 127.0.0.1:57583
FASTLINK sync complete: data-delta.bin
  file_bytes: 8388608
  chunks_total: 128
  chunks_reused: 126
  chunks_transferred: 2
  raw_missing_bytes: 131072
  encoded_payload_bytes: 131072
  handshake_wire_bytes: 12
  manifest_wire_bytes: 4129
  need_wire_bytes: 12
  chunk_framing_wire_bytes: 18
  status_wire_bytes: 1
  headers_and_framing_wire_bytes: 4172
  wire_bytes_tx: 135225
  wire_bytes_rx: 19
  wire_bytes_total: 135244
  payload_bytes_avoided_vs_full_file: 8257536
  wire_bytes_avoided_vs_full_file: 8253364
  handshake_us: 67
  manifest_us: 86081
  transfer_us: 8104
  elapsed_us: 97408
  elapsed_ms: 97
  wire_mib_per_s: 1.324
  logical_mib_per_s: 82.128
  note: wire_bytes_total includes handshake, manifest, NEED list, chunk framing, encoded payloads, and status (excludes TCP/IP headers)
```

### main 8388608 cold

```
/tmp/fl3b5-main/target/release/fastlink send /tmp/fl3b5-data/size-8388608/data.bin 127.0.0.1:39667
FASTLINK sync complete: data.bin
  file_bytes: 8388608
  chunks_total: 128
  chunks_reused: 0
  chunks_transferred: 128
  raw_missing_bytes: 8388608
  encoded_payload_bytes: 8388608
  payload_bytes_avoided_vs_full_file: 0
  elapsed_ms: 151
  note: payload counts exclude wire framing, hashes, and TCP overhead
```

### main 8388608 warm

```
/tmp/fl3b5-main/target/release/fastlink send /tmp/fl3b5-data/size-8388608/data.bin 127.0.0.1:54977
FASTLINK sync complete: data.bin
  file_bytes: 8388608
  chunks_total: 128
  chunks_reused: 128
  chunks_transferred: 0
  raw_missing_bytes: 0
  encoded_payload_bytes: 0
  payload_bytes_avoided_vs_full_file: 8388608
  elapsed_ms: 54
  note: payload counts exclude wire framing, hashes, and TCP overhead
```

### main 8388608 delta

```
/tmp/fl3b5-main/target/release/fastlink send /tmp/fl3b5-data/size-8388608/data-delta.bin 127.0.0.1:57657
FASTLINK sync complete: data-delta.bin
  file_bytes: 8388608
  chunks_total: 128
  chunks_reused: 126
  chunks_transferred: 2
  raw_missing_bytes: 131072
  encoded_payload_bytes: 131072
  payload_bytes_avoided_vs_full_file: 8257536
  elapsed_ms: 93
  note: payload counts exclude wire framing, hashes, and TCP overhead
```

### integrate 67108864 cold

```
/home/nightknight/Projects/FASTLINK/.worktrees/fastlink-v3-integrate/target/release/fastlink send /tmp/fl3b5-data/size-67108864/data.bin 127.0.0.1:39805
FASTLINK sync complete: data.bin
  file_bytes: 67108864
  chunks_total: 1024
  chunks_reused: 0
  chunks_transferred: 1024
  raw_missing_bytes: 67108864
  encoded_payload_bytes: 66254451
  handshake_wire_bytes: 12
  manifest_wire_bytes: 32795
  need_wire_bytes: 4100
  chunk_framing_wire_bytes: 9216
  status_wire_bytes: 1
  headers_and_framing_wire_bytes: 46124
  wire_bytes_tx: 66296468
  wire_bytes_rx: 4107
  wire_bytes_total: 66300575
  payload_bytes_avoided_vs_full_file: 854413
  wire_bytes_avoided_vs_full_file: 808289
  handshake_us: 111
  manifest_us: 46244
  transfer_us: 507062
  elapsed_us: 574492
  elapsed_ms: 574
  wire_mib_per_s: 110.061
  logical_mib_per_s: 111.403
  note: wire_bytes_total includes handshake, manifest, NEED list, chunk framing, encoded payloads, and status (excludes TCP/IP headers)
```

### integrate 67108864 warm

```
/home/nightknight/Projects/FASTLINK/.worktrees/fastlink-v3-integrate/target/release/fastlink send /tmp/fl3b5-data/size-67108864/data.bin 127.0.0.1:52247
FASTLINK sync complete: data.bin
  file_bytes: 67108864
  chunks_total: 1024
  chunks_reused: 1024
  chunks_transferred: 0
  raw_missing_bytes: 0
  encoded_payload_bytes: 0
  handshake_wire_bytes: 12
  manifest_wire_bytes: 32795
  need_wire_bytes: 4
  chunk_framing_wire_bytes: 0
  status_wire_bytes: 1
  headers_and_framing_wire_bytes: 32812
  wire_bytes_tx: 32801
  wire_bytes_rx: 11
  wire_bytes_total: 32812
  payload_bytes_avoided_vs_full_file: 67108864
  wire_bytes_avoided_vs_full_file: 67076052
  handshake_us: 1975
  manifest_us: 24775
  transfer_us: 45441
  elapsed_us: 90501
  elapsed_ms: 90
  wire_mib_per_s: 0.346
  logical_mib_per_s: 707.169
  note: wire_bytes_total includes handshake, manifest, NEED list, chunk framing, encoded payloads, and status (excludes TCP/IP headers)
```

### integrate 67108864 delta

```
/home/nightknight/Projects/FASTLINK/.worktrees/fastlink-v3-integrate/target/release/fastlink send /tmp/fl3b5-data/size-67108864/data-delta.bin 127.0.0.1:50075
FASTLINK sync complete: data-delta.bin
  file_bytes: 67108864
  chunks_total: 1024
  chunks_reused: 1013
  chunks_transferred: 11
  raw_missing_bytes: 720896
  encoded_payload_bytes: 720896
  handshake_wire_bytes: 12
  manifest_wire_bytes: 32801
  need_wire_bytes: 48
  chunk_framing_wire_bytes: 99
  status_wire_bytes: 1
  headers_and_framing_wire_bytes: 32961
  wire_bytes_tx: 753802
  wire_bytes_rx: 55
  wire_bytes_total: 753857
  payload_bytes_avoided_vs_full_file: 66387968
  wire_bytes_avoided_vs_full_file: 66355007
  handshake_us: 2190
  manifest_us: 66966
  transfer_us: 60297
  elapsed_us: 149823
  elapsed_ms: 149
  wire_mib_per_s: 4.799
  logical_mib_per_s: 427.169
  note: wire_bytes_total includes handshake, manifest, NEED list, chunk framing, encoded payloads, and status (excludes TCP/IP headers)
```

### main 67108864 cold

```
/tmp/fl3b5-main/target/release/fastlink send /tmp/fl3b5-data/size-67108864/data.bin 127.0.0.1:49953
FASTLINK sync complete: data.bin
  file_bytes: 67108864
  chunks_total: 1024
  chunks_reused: 0
  chunks_transferred: 1024
  raw_missing_bytes: 67108864
  encoded_payload_bytes: 67108864
  payload_bytes_avoided_vs_full_file: 0
  elapsed_ms: 251
  note: payload counts exclude wire framing, hashes, and TCP overhead
```

### main 67108864 warm

```
/tmp/fl3b5-main/target/release/fastlink send /tmp/fl3b5-data/size-67108864/data.bin 127.0.0.1:41111
FASTLINK sync complete: data.bin
  file_bytes: 67108864
  chunks_total: 1024
  chunks_reused: 1024
  chunks_transferred: 0
  raw_missing_bytes: 0
  encoded_payload_bytes: 0
  payload_bytes_avoided_vs_full_file: 67108864
  elapsed_ms: 99
  note: payload counts exclude wire framing, hashes, and TCP overhead
```

### main 67108864 delta

```
/tmp/fl3b5-main/target/release/fastlink send /tmp/fl3b5-data/size-67108864/data-delta.bin 127.0.0.1:49973
FASTLINK sync complete: data-delta.bin
  file_bytes: 67108864
  chunks_total: 1024
  chunks_reused: 1013
  chunks_transferred: 11
  raw_missing_bytes: 720896
  encoded_payload_bytes: 720896
  payload_bytes_avoided_vs_full_file: 66387968
  elapsed_ms: 136
  note: payload counts exclude wire framing, hashes, and TCP overhead
```

Sustained runs use the same `fastlink send FILE 127.0.0.1:PORT` command. 
Their parsed fields are in the per-run table. One cold sample and one warm sample follow.

### sustained cold idx 0

```
/home/nightknight/Projects/FASTLINK/.worktrees/fastlink-v3-integrate/target/release/fastlink send /tmp/fl3b5-data/sustained/src/b5-000.bin 127.0.0.1:54529
FASTLINK sync complete: b5-000.bin
  file_bytes: 1048576
  chunks_total: 16
  chunks_reused: 0
  chunks_transferred: 16
  raw_missing_bytes: 1048576
  encoded_payload_bytes: 653688
  handshake_wire_bytes: 12
  manifest_wire_bytes: 541
  need_wire_bytes: 68
  chunk_framing_wire_bytes: 144
  status_wire_bytes: 1
  headers_and_framing_wire_bytes: 766
  wire_bytes_tx: 654379
  wire_bytes_rx: 75
  wire_bytes_total: 654454
  payload_bytes_avoided_vs_full_file: 394888
  wire_bytes_avoided_vs_full_file: 394122
  handshake_us: 94
  manifest_us: 82291
  transfer_us: 10412
  elapsed_us: 93275
  elapsed_ms: 93
  wire_mib_per_s: 6.691
  logical_mib_per_s: 10.721
  note: wire_bytes_total includes handshake, manifest, NEED list, chunk framing, encoded payloads, and status (excludes TCP/IP headers)
```

### sustained warm idx 0

```
/home/nightknight/Projects/FASTLINK/.worktrees/fastlink-v3-integrate/target/release/fastlink send /tmp/fl3b5-data/sustained/src/b5-000.bin 127.0.0.1:54529
FASTLINK sync complete: b5-000.bin
  file_bytes: 1048576
  chunks_total: 16
  chunks_reused: 16
  chunks_transferred: 0
  raw_missing_bytes: 0
  encoded_payload_bytes: 0
  handshake_wire_bytes: 12
  manifest_wire_bytes: 541
  need_wire_bytes: 4
  chunk_framing_wire_bytes: 0
  status_wire_bytes: 1
  headers_and_framing_wire_bytes: 558
  wire_bytes_tx: 547
  wire_bytes_rx: 11
  wire_bytes_total: 558
  payload_bytes_avoided_vs_full_file: 1048576
  wire_bytes_avoided_vs_full_file: 1048018
  handshake_us: 1472
  manifest_us: 41553
  transfer_us: 1019
  elapsed_us: 44459
  elapsed_ms: 44
  wire_mib_per_s: 0.012
  logical_mib_per_s: 22.492
  note: wire_bytes_total includes handshake, manifest, NEED list, chunk framing, encoded payloads, and status (excludes TCP/IP headers)
```

