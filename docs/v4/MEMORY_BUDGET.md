# FASTLINK v4 — MEMORY BUDGET

**Model:** `/mnt/gamez/AI/model-tests/qwen3d-sensenova/Qwen3.5-4B-3D-Coder-Q4_K_M.gguf`
**SHA-256:** `32c37c83d3ca055d30aefa679f1a1670d3e2eb889ee7cc805792d53f219cfae8`
**File size:** 2,708,804,096 bytes = 2.5216 GiB (2.7088 GB)
**Arch (from GGUF metadata):** `qwen35`, size label `4.2B`

## 1. Architecture (measured from GGUF header — not generic assumptions)

| Parameter | Value |
|---|---|
| `block_count` (layers) | 32 |
| `embedding_length` | 2560 |
| `attention.head_count` (Q heads) | 16 |
| `attention.head_count_kv` (KV heads) | 4 (GQA 4:1) |
| `attention.key_length` / `value_length` (head_dim) | 256 |
| `full_attention_interval` | 4 → **8 full-attention layers** |
| recurrent (linear/SSM) layers | **24** (every non-4th layer) |
| SSM: `inner_size` | 4096 |
| SSM: `state_size` | 128 |
| SSM: `conv_kernel` | 4 |
| SSM: `group_count` | 16 |
| `context_length` (declared) | 262144 |
| `feed_forward_length` | 9216 |

**Key consequence:** this is a hybrid attention/recurrent model (Qwen3.5-Next family).
Only the **8 full-attention layers** store a KV cache that grows with context length.
The **24 recurrent (SSM/Mamba2) layers** store a *fixed-size* state that does NOT grow
with context length. This makes 128K context far cheaper than a generic 32-layer
transformer of the same parameter count.

## 2. Per-component memory model

### 2.1 Quantized weights
- Q4_K_M: **2.5216 GiB** (2,708,804,096 B). This is constant across all configs.

### 2.2 KV cache (only 8 full-attention layers)
Elements per token = 8 layers × 2 (K,V) × 4 KV-heads × 256 head_dim = **16,384 elements**

| KV dtype | bytes/element (incl. scale) | bytes/token | 8K | 32K | 64K | 128K |
|---|---|---|---|---|---|---|
| f16 | 2.0 | 32,768 (32 KiB) | 256 MiB | 1.00 GiB | 2.00 GiB | 4.00 GiB |
| q8_0 | 1.125 (36/32) | 18,432 (18 KiB) | 147 MiB | 586 MiB | 1.17 GiB | 2.33 GiB |
| q4_0 | 0.625 (20/32) | 10,240 (10 KiB) | 82 MiB | 320 MiB | 640 MiB | 1.25 GiB |

(Quant block overhead: Q4_0 = 32 elems → 16 B data + 4 B scale = 20 B; Q8_0 = 32 elems → 32 B + 4 B = 36 B.)

### 2.3 Recurrent/SSM state (fixed, 24 recurrent layers)
Per recurrent layer ≈ state_size(128) × inner_size(4096) = 524,288 elements (f32) ≈ 2 MiB,
plus conv-state (small). 24 layers ≈ **~50–65 MiB total, constant** (context-independent).

### 2.4 Compute buffers / graph workspace
Matmul/compute buffers + Vulkan graph allocations: estimated **~256 MiB** (depends on
batch size; grows modestly for large prefill).

### 2.5 Backend/runtime overhead
Vulkan device buffers, shader caches, allocator metadata: **~128–192 MiB**.

### 2.6 CPU RAM for offloaded layers (partial GPU)
When offloading L layers to CPU: L × (per-layer weight bytes) + SSM state for those
layers stays in system RAM. Per-layer weight ≈ 2.52 GiB / 32 ≈ 82 MiB.

### 2.7 Fragmentation + safety margin
Add **~10%** over the sum of components to cover allocator fragmentation,
Vulkan pool growth and headroom.

## 3. Total budget per context (weights + KV + SSM + buffers + backend, then +10%)

| context | KV f16 total | KV q4_0 total |
|---|---|---|
| 8K  | ≈ 3.55 GiB | ≈ 3.37 GiB |
| 32K | ≈ 4.38 GiB | ≈ 4.00 GiB |
| 64K | ≈ 5.48 GiB | ≈ 3.98 GiB |
| 128K| ≈ 7.68 GiB | ≈ 4.65 GiB |

## 4. Host memory state (measured 2026-11-08)

| Host | GPU | Total VRAM | Free VRAM | Used by | System RAM (avail) |
|---|---|---|---|---|---|
| NightKnightPC | RX 7900 XT | 20 GB | **~3.7 GB** | Heretic 27B commander (17.8 GB, Vulkan0, 128K ctx, q4_0 KV) | 7.1 GB (but 4.1 GB RSS in active llama-server on :8131) |
| detti | RX 6600 XT | 8 GB | **~7 GB** (op. report; ~1 GB used) | — | 32 GB |

- NightKnightPC: Ryzen 7 7800X3D, 32 GB RAM, Arch.
- detti: Ryzen 7 5800X3D, 32 GB RAM, Windows 11.
- LAN: **~926 MiB/s (N→D), ~981 MiB/s (D→N)** (~7.4–7.8 Gbps). RTT ~0.6–24 ms (avg ~6.5 ms via Tailscale direct; direct ICMP to 192.168.1.4 blocked, data path = Tailscale direct 192.168.1.4:41641).

## 5. Feasibility verdict

| Config | 8K | 32K | 64K | 128K |
|---|---|---|---|---|
| A — CPU-only (N) | tight* | ok (q4_0) | ok (q4_0) | at risk* |
| B — 7900 XT full (N) | fits (q4_0 KV) | ✗ (3.44 GiB free) | ✗ | ✗ |
| C — 6600 XT full (detti) | fits | fits | fits (f16) | fits (q4_0 KV) |
| D — hybrid (N) | fits | fits | fits | fits (offload) |
| E — dual replicas | ok | ok | ok | ok (each host ≤ its free VRAM) |
| F — distributed v3 | 8K-128K | see §6 | | |

\* CPU-only 128K: q4_0 KV total ≈ 4.65 GiB, but only ~3 GB RAM is free while the
active 8131 llama-server (4.1 GB RSS) is running. Feasible only if that server is
released or KV is q4_0 + buffers trimmed. **Do not assume full 128K on CPU.**

**Primary 128K target:** detti 6600 XT with **q4_0 KV cache** (fits in ~7 GB free).

## 6. Recommended placement strategy

- **128K context → detti 6600 XT (7 GB free), q4_0 KV, all 32 layers on GPU.**
- **7900 XT (3.7 GB free)** can host the 4B weights + 8K; beyond that, offload layers to CPU
  (hybrid D) or move large-context work to detti.
- **Distributed (F):** split layers 70% local (7900 XT) / 30% remote (6600 XT) via the
  existing FASTLINK-RPC/1 `-sm layer -ts 0.70,0.30` path. For 128K, invert: put the bulk of
  layers + KV on detti (7 GB) and a smaller share on the 7900 XT.

## 7. Validation plan (runtime measurement)

Estimates above must be confirmed by real runtime allocation:
1. Load the 4B GGUF on detti 6600 XT at `-c 131072 --cache-type-k q4_0 --cache-type-v q4_0`,
   all layers on GPU (`-ngl 99`), and record peak VRAM (via `vram` tool / GPU counters).
2. Repeat on 7900 XT at `-c 8192` (and try 32768) to confirm the 3.44-GiB-free ceiling.
3. Record `--fit` behaviour: with `--fit on --fit-target 0`, llama.cpp auto-drops layers to
   CPU when VRAM is insufficient — count actual `-ngl` used.
4. Confirm KV dtype actually applied (llama-server log line `KV cache size: ...`).

Reproducible load commands:
```
# detti full-GPU 128K (q4_0 KV)
llama-server -m Qwen3.5-4B-3D-Coder-Q4_K_M.gguf --device Vulkan0 -ngl 99 -c 131072 \
  --cache-type-k q4_0 --cache-type-v q4_0 -fa on --host 127.0.0.1 --port 8141 -lv 3

# 7900 XT 8K (Vulkan, auto-fit to free VRAM)
llama-server -m Qwen3.5-4B-3D-Coder-Q4_K_M.gguf --device Vulkan0 --fit on --fit-target 0 \
  -ngl 99 -c 8192 --cache-type-k q4_0 --cache-type-v q4_0 -fa on -lv 3
```
