# FL4-KV layer placement (q4_0 KV)

CPU arithmetic only. No GPU allocation, no model load, and no inference server were run for this table.

Inputs are the measured GGUF header values recorded in `docs/v4/MEMORY_BUDGET.md` (`feat/fastlink-v4` `674b400`) for `Qwen3.5-4B-3D-Coder-Q4_K_M.gguf` (SHA-256 `32c37c83d3ca055d30aefa679f1a1670d3e2eb889ee7cc805792d53f219cfae8`):

- 32 layers, file size 2,708,804,096 bytes.
- `qwen35.attention.recurrent_layers` = 24 ones and 8 zeros. Full attention is layers 3, 7, 11, 15, 19, 23, 27, 31. That is the same predicate as `(i + 1) % 4 != 0` for recurrent layers.
- KV heads 4, head dim 256. q4_0 stores 32 elements in 20 bytes, so one attention layer is `4 * 2 * 256 * 20 / 32 = 1280` bytes per token.
- SSM f32 state is `128 * 4096 * 4 = 2,097,152` bytes per recurrent layer. The conv state is not a measured byte count, so it is not added.

## What this table is

Local is a contiguous prefix `0..k` on the NightKnightPC 7900 XT. Remote is `k..32` on the detti 6600 XT. Free byte budgets are the memory-budget figures "~3.7 GB" and "~7 GB" taken as decimal bytes: 3,700,000,000 and 7,000,000,000.

A side with zero layers costs 0. A side with any layers pays one copy of the memory budget's estimated compute buffer (256 MiB) and the high end of its estimated backend range (192 MiB). Those two figures are estimates in MEMORY_BUDGET §2.4 and §2.5, not new measurements. The §2.7 10% margin is `ceil(sum * 1.1)`, computed as `(sum * 11 + 9) / 10`.

Weights use the budget's uniform split, `file_bytes / 32 = 84,650,128` bytes per layer. That is not a per-tensor measurement.

`est decode tok/s` is **not estimated**. MEMORY_BUDGET.md has no decode measurement and no per-layer compute rate. The v3 scheduler's tok/s figures are for a different 27B dense model and are not used here.

Section 3 of MEMORY_BUDGET.md lists a 64K q4_0 total below its 32K q4_0 total. This table does not copy those totals. It recomputes from the §2 component formulas.

## Selection rule

Among splits whose high envelopes both fit, pick the minimum of `max(local_envelope / local_free, remote_envelope / remote_free)`. Ties keep more layers on the 6600 XT. This is a memory-fit split, not a measured speed optimum.

`src/qwen35_4b.rs` `placement_table` is the implementation. The numbers below are the values that test locks.

## Selected split

| context | tokens | local_layers | remote_layers | VRAM_local bytes | VRAM_remote bytes | feasibility | est decode tok/s |
|---|---:|---:|---:|---:|---:|---|---|
| 8K | 8192 | 9 | 23 | 1393991263 | 2766809250 | fits both free budgets | not estimated |
| 32K | 32768 | 10 | 22 | 1558619287 | 2879005290 | fits both free budgets | not estimated |
| 64K | 65536 | 10 | 22 | 1650893975 | 3155829354 | fits both free budgets | not estimated |
| 128K | 131072 | 11 | 21 | 1930865359 | 3614055474 | fits both free budgets | not estimated |

VRAM columns are the high envelope after the 10% margin. Dividing those byte counts by 2^30 gives, in row order: local/remote GiB 1.298256/2.576792, 1.451577/2.681283, 1.537515/2.939095, and 1.798258/3.365851. The byte columns are the exact values.

## Largest local prefix that still fits

| context | max local layers that fit | all 32 layers on the 7900 XT |
|---|---:|---|
| 8K | 32 | fits |
| 32K | 30 | does not fit |
| 64K | 27 | does not fit |
| 128K | 23 | does not fit |

`k = 0` (entire model on the 6600 XT) fits at every context in this table. The selected rows above are the balanced memory-fit splits, not that all-remote placement.

## Envelope contents for the selected rows

Overhead on every non-empty side is 469,762,048 bytes (256 MiB + 192 MiB) before the 10% margin.

| context | side | layers | attention | recurrent | weight bytes | KV bytes | SSM state bytes | envelope bytes |
|---|---|---:|---:|---:|---:|---:|---:|---:|
| 8K | local | 9 | 2 | 7 | 761851152 | 20971520 | 14680064 | 1393991263 |
| 8K | remote | 23 | 6 | 17 | 1946952944 | 62914560 | 35651584 | 2766809250 |
| 32K | local | 10 | 2 | 8 | 846501280 | 83886080 | 16777216 | 1558619287 |
| 32K | remote | 22 | 6 | 16 | 1862302816 | 251658240 | 33554432 | 2879005290 |
| 64K | local | 10 | 2 | 8 | 846501280 | 167772160 | 16777216 | 1650893975 |
| 64K | remote | 22 | 6 | 16 | 1862302816 | 503316480 | 33554432 | 3155829354 |
| 128K | local | 11 | 2 | 9 | 931151408 | 335544320 | 18874368 | 1930865359 |
| 128K | remote | 21 | 6 | 15 | 1777652688 | 1006632960 | 31457280 | 3614055474 |

## Where KV is placed

KV stays with the attention layer that owns it. `kv_layer_home` in `src/qwen35_4b.rs` uses the selected split above. The state runtime labels every accepted KV span with the 8K row (`KV_PLACEMENT_CONTEXT` = 8192). The 32K, 64K, and 128K rows make the same local/remote choice for every attention layer, because each selected prefix is 9, 10, 10, or 11 layers and the attention layers are 3, 7, 11, 15, 19, 23, 27, and 31.

| attention layer | home | free budget of that worker |
|---:|---|---:|
| 3, 7 | local, NightKnightPC 7900 XT | 3,700,000,000 bytes |
| 11, 15, 19, 23, 27, 31 | remote, detti 6600 XT | 7,000,000,000 bytes |

The 6600 XT is the worker with more free capacity, and it holds six of the eight KV layers. The 7900 XT holds KV only for layers 3 and 7, which is the prefix that the memory-fit split leaves on that card. At 8K the 7900 XT could hold all 32 layers, but that is not the selected split, so those six KV layers stay on the 6600 XT. This is a label on the CPU state path. It does not allocate a GPU or open a second host. Decode tok/s is still not estimated.
