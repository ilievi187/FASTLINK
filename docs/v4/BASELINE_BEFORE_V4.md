# FASTLINK v4 — BEFORE baseline (v3 integration build, measured)

Measured on the v3 integration branch (`integrate/fastlink-v3-m1` / `feat/fastlink-v4` base) with the
RPC adapter build (commit e98cb85), the exact benchmark GGUF, q4_0 K/V cache, flash-attention on,
deterministic seeded prompt sized to (target_context - n_predict), n_predict=64, 3 repetitions, median reported.

## Model
- `/mnt/gamez/AI/model-tests/qwen3d-sensenova/Qwen3.5-4B-3D-Coder-Q4_K_M.gguf`
- SHA-256 `32c37c83d3ca055d30aefa679f1a1670d3e2eb889ee7cc805792d53f219cfae8` (2,708,804,096 B)
- Hybrid: 32 layers, 8 full-attention / 24 recurrent (SSM); qwen35 arch; declared ctx 262,144.
- llama.cpp: version 0.8.0-dev (build 6, commit e98cb85), GNU 16.2.1.

## Hardware
- NightKnightPC: Ryzen 7 7800X3D, RX 7900 XT 20 GB (19.5 GB used by the 27B service → ~2 GB free), 32 GB RAM.
- detti: Ryzen 5800X3D, RX 6600 XT 8 GB (~7 GB free), 32 GB RAM.
- Network: Tailscale/direct, measured ~926 MiB/s N→D / ~981 MiB/s D→N (256 MiB transfers).

## Measured results (median of 3 reps) — prefill tok/s / decode tok/s

| Config | 8K | 32K | 64K | 128K |
|---|---|---|---|---|
| **A** CPU-only (NKP) | 58.2 / 14.07 | 29.7 / 8.68 | infeasible (documented) | infeasible (documented) |
| **B** 7900 XT (NKP) single GPU | BLOCKED — only ~2 GB VRAM free (27B occupies 19.5 GB); model+KV won't fit | — | — | — |
| **C** 6600 XT (detti) single GPU | 381 / 64.7 | 158 / 57.9 | 68 / 46.2 | 35 / 35.7 |
| **D** hybrid CPU/GPU (NKP) | (pending) | — | — | — |
| **E** dual independent replicas | (pending) | — | — | — |
| **F** distributed (Flink v3 RPC, layer split 0.30/0.70) | 423 / 28.0 | (pending) | — | 41.0 / 18.6 |

## Key findings (before v4)
1. **Single GPU (C, 6600 XT) is the fastest for decode** (64.7 tok/s @8K); it scales down to 35.7 @128K.
2. **Distributed (F) beats single-GPU on PREFILL** (423 vs 381 @8K, +11%; 41 vs 35 @128K, +17%) — two GPUs process the prompt in parallel.
3. **Distributed LOSES on DECODE** (28 vs 64.7 @8K, −57%; 18.6 vs 35.7 @128K, −48%) — every token requires cross-machine tensor sync (per-token RPC round-trips), which dominates decode. The 128K prefill took ~53 min (each token's KV synced across the wire).
4. **CPU (A) is ~6.5x slower than GPU** on prefill, ~4.6x on decode — GPU acceleration is essential.
5. **128K on a single 6600 XT is feasible** (fits in 8 GB); 7900 XT currently cannot host it (occupied by the 27B service).
6. **Config B is blocked** by the 27B service occupying 19.5 GB of the 7900 XT (only ~2 GB free). Documented honestly, not fabricated.

## The v4 target (what must improve)
- **Decode throughput**: the dominant cost is per-token cross-machine sync. v4 persistent residency must reuse valid tensors (0 re-send) and reduce sync round-trips, so decode approaches single-GPU speed while keeping the distributed prefill win.
- **Network bytes/token**: eliminate redundant re-transmission of unchanged state.
- **128K**: the main target — reliable distributed 128K with fast decode.

## Reproduction
```
# single GPU (deti, 6600 XT):   config C via fl4_bench.ps1
# distributed (F):              /tmp/fl4_bench_f.sh  (7900 XT local + 6600 XT RPC, -ts 0.30,0.70)
# CPU (A):                      qa/v4/baseline_runner.py --config A
# full matrix:                 qa/v4/baseline_runner.py --list
# raw results:                 qa/v4/baseline_results.json
```
Blocked/deferred: B (VRAM), A-64K/128K (CPU infeasible), D/E (pending — see runner), F-32K/64K (pending).
