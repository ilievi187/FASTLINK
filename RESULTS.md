# FASTLINK A-1: Single-GPU Benchmark Results on detti (RX 6600 XT)

**Date:** 2026-11-10  
**Host Node:** `detti` (Windows 11 Build 26200.9711, AMD Ryzen 7 5800X3D 8C/16T, 32 GB RAM)  
**Target GPU:** AMD Radeon RX 6600 XT (Navi 23, 8192 MB GDDR6, Driver 32.0.21043.19003)  
**Inference Engine:** `llama.cpp` b12191 (`llama-server.exe`), Vulkan0 backend  
**Active Model:** `Ornith-1.5-35B-A3B-Heretic-MTP-APEX-I-Mini.gguf` (Q3_K_M, 35B MoE, ~3.4B active params)  
**Server Config:** Context `65536`, KV cache `q4_0`, Flash Attention `on`, MoE GPU cache `4096 MiB` (CPU miss share `0.95`), batch `1024`, ubatch `512`, threads `6`  
**Harness:** `benches/detti_gpu_bench.py` and `benches/rpc_latency.rs`  
**Machine-Readable Raw Data:** `benches/results_detti_gpu.json`

---

## 1. Executive Summary

This report documents the single-GPU baseline benchmark on node `detti` (AMD Radeon RX 6600 XT 8GB) and evaluates the FASTLINK-RPC/1 network transport against raw TCP socket transfer.

Key findings:
1. **Single-GPU Inference Throughput:** The RX 6600 XT delivers **28.2 – 31.4 decode tok/s** consistently across varied prompt types (system architecture, code generation, GLSL shaders) and **36.8 – 60.1 prefill tok/s** for prompt processing.
2. **VRAM Footprint & Engine Utilization:** The model occupies **6,018.09 MB** (73.46% of 8 GB VRAM) dedicated to the `llama-server.exe` process, with total system adapter VRAM at **6,782.07 MB** (82.79%). During generation bursts, active Vulkan Compute Engine utilization reaches **89.8% – 96.8%**.
3. **FASTLINK Transport vs Raw TCP:**
   - On **compressible/sparse tensors (RLE)**, FASTLINK reduces 8 MiB wire payload to **66,700 B** (**99.2% wire bandwidth reduction**), delivering an effective raw equivalent throughput of **1,892.2 MB/s** over loopback.
   - On **incompressible patterned data (RAW)**, raw TCP streams at **8,122.5 MB/s** directly through kernel buffers, while FASTLINK achieves **605.6 MB/s** due to the CPU cost of BLAKE3 chunk hashing and framing (+652 bytes overhead for 128 chunks).
   - Single-chunk (64 KiB) round-trip latency: Raw TCP achieves **0.021 ms** median RTT, whereas FASTLINK achieves **0.096 ms** median RTT (+0.075 ms for 64 KiB BLAKE3 digest computation + FLR1 framing verification).

---

## 2. detti Single-GPU Inference Performance

Measured across multiple prompt scenarios using deterministic generation (`temperature=0.0`, `cache_prompt=false`):

| Scenario | Description | Prompt Tokens | Output Tokens | Prefill (tok/s) | Decode (tok/s) | TTFT (s) | Total Wall Time (s) | Peak Compute Util |
|---|---|---:|---:|---:|---:|---:|---:|---:|
| `short_32` | Smoke test / concept query | 15 | 32 | **60.09** | **29.23** | 0.2496 | 7.51 | 89.8% |
| `medium_64` | Architecture trade-offs | 45 | 64 | **39.97** | **30.35** | 1.1257 | 15.88 | 96.8% |
| `standard_128` | Concurrency & code review | 51 | 128 | **45.30** | **31.03** | 1.1257 | 5.25 | 60.1% |
| `code_task_128` | Low-poly GLSL shader task | 38 | 128 | **37.48** | **31.44** | 1.0139 | 5.08 | 65.0% |

*Note on wall time:* Initial runs reflect slot allocation and cold queue scheduling; steady-state generation delivers ~31 tok/s with ~1.0–1.1s Time to First Token (TTFT).

---

## 3. VRAM & Hardware Utilization Telemetry

Hardware telemetry sampled via Windows Performance Counters (`\GPU Adapter Memory(*)\*`, `\GPU Process Memory(*)\*`, `\GPU Engine(*)\*`) on `detti`:

| Metric | Measured Value | % of Total (8,192 MB) | Details / Provenance |
|---|---:|---:|---|
| **GPU Total Dedicated VRAM** | **6,782.07 MB** | **82.79%** | Total physical VRAM consumed on RX 6600 XT |
| **`llama-server` Process Dedicated** | **6,018.09 MB** | **73.46%** | VRAM allocated directly to PID 16736 |
| **Process Shared System RAM** | **104.64 MB** | — | System RAM allocated for GPU staging / fallback |
| **Available Free VRAM** | **1,409.93 MB** | **17.21%** | Headroom available for additional KV context / layers |
| **Active Compute Engine Util** | **89.8% – 96.8%** | — | Peak Vulkan compute engine load during decode |

---

## 4. FASTLINK Transport vs Raw TCP Transfer

Benchmarked with `cargo bench --bench rpc_latency` using 64 KiB chunks and 8 MiB tensors (500 latency rounds, 32 throughput rounds, TCP_NODELAY enabled):

| Benchmark Scenario | Raw TCP Transfer | FASTLINK Transport | Delta / Advantage |
|---|---:|---:|---|
| **Single-Chunk RTT (64 KiB, min)** | 0.018 ms | 0.093 ms | +0.075 ms (BLAKE3 + framing) |
| **Single-Chunk RTT (64 KiB, median)** | **0.021 ms** | **0.096 ms** | +0.075 ms framing overhead |
| **Single-Chunk RTT (64 KiB, mean)** | 0.022 ms | 0.098 ms | +0.076 ms |
| **8 MiB Patterned (Incompressible) Throughput** | **8,122.5 MB/s** | **605.6 MB/s** | CPU hashing / framing bound |
| **8 MiB Patterned Wire Bytes** | 8,388,608 B | 8,389,260 B | +652 B framing (+0.0078%) |
| **8 MiB Uniform (Compressible) Throughput** | 7,815.5 MB/s | **1,892.2 MB/s** | Raw equivalent speed |
| **8 MiB Uniform Wire Bytes** | 8,388,608 B | **66,700 B** | **−8,321,908 B (−99.20% wire data)** |
| **Connection Setup Latency** | 0.033 ms | 0.033 ms | Client TCP 3-way handshake |

---

## 5. Architectural Observations & Recommendations

1. **When FASTLINK Wins:**
   - On repetitive, sparse, or uniform activations/weights, FASTLINK's RLE encoding eliminates **99.2% of network bytes**, reducing an 8 MiB transfer to just 66.7 KiB. On bandwidth-constrained links (e.g., 1 Gbps LAN or WAN), this converts an 80 ms network transfer into < 1 ms.
   - Content-addressable BLAKE3 chunking ensures cached tensors require **0 wire bytes** on cache hits.

2. **When Raw TCP Wins:**
   - On high-bandwidth loopback or dense incompressible tensor data, raw TCP streams memory buffers directly through the network stack without cryptographic hashing overhead.
   - The ~0.075 ms per-chunk CPU hashing cost should be amortized via batched SIMD hashing (e.g. AVX2/AVX-512 BLAKE3) or adaptive bypass for verified non-deduplicatable streams.

3. **Single-GPU Host Stability:**
   - detti's RX 6600 XT operates stably within its 8 GB budget at 82.8% VRAM utilization, sustaining ~30 tok/s decode for the 35B MoE architecture.
