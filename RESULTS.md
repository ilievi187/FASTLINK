# FASTLINK A-2 / A-3: Concurrent Dual-GPU Inference & CPU Tensor-Transfer Results

**Date:** 2026-11-10  
**Nodes Participating:**
- **Node 0 (Primary Host):** `NightKnightPC`  
  - **GPU 0:** AMD Radeon RX 7900 XT (Navi 31, 20,464 MB GDDR6)
- **Node 1 (Secondary Host):** `detti`  
  - **GPU 1:** AMD Radeon RX 6600 XT (Navi 23, 8,192 MB GDDR6)
- **Active Model:** `Ornith-1.5-35B-A3B-Heretic-MTP-APEX-I-Mini.gguf` (Q3_K_M, 35B MoE, ~3.4B active params)  
- **Benchmark Harnesses:**  
  - Distributed GPU Inference: `benches/distributed_gpu_bench.py`  
  - Memory-Mapped Tensor Transfer: `benches/mmap_transfer.rs` (`cargo bench --bench mmap_transfer`)  
  - Single-GPU Baseline: `benches/detti_gpu_bench.py`  
- **Machine-Readable Raw Data:** `benches/results_distributed_gpu.json` (inference/VRAM and the older `mmap_transfer` report); latest `rpc_latency` transport run: `benches/results_rpc_latency.json`.
- The distributed report JSON does not capture host CPU, kernel, RAM, or per-scenario GPU utilization; do not infer those values from it. The separately captured `rpc_latency` host metadata is in section 4.3.

---

## 1. Executive Summary

This report contains supplied concurrent independent inference measurements and a separate CPU-only shared-memory copy / TCP transport microbenchmark. The `mmap_transfer` harness does not integrate with a GPU; its shared-memory numbers are not GPU staging or zero-copy measurements. A separately run `rpc_latency` snapshot is recorded in section 4.3.

Key findings:
1. **Concurrent Dual-GPU Request Throughput (summed per-device rates):**
   - Concurrent independent requests to the RX 7900 XT and RX 6600 XT produce a summed rate of **117.41 – 118.94 decode tok/s** across the four prompt workloads.
   - In the single-GPU baseline rows, the RX 7900 XT records **84.58 – 86.29 decode tok/s** with **0.11 – 0.17s TTFT**; the RX 6600 XT records **26.68 – 29.98 decode tok/s** with **0.32 – 1.43s TTFT**.
   - In the distributed rows, the two per-GPU rates sum to **117.41 – 118.94 tok/s**. The displayed **3.72x – 3.80x** is aggregate rate divided by the concurrent RX 6600 XT-side rate, not a measured speedup against the single-GPU baseline.
2. **Multi-GPU VRAM Telemetry & Memory Headroom:**
   - **RX 7900 XT (20 GB):** Occupies **19,510.8 MB** (95.34% of 20,464 MB), with **18,800 MB** dedicated to `llama-server` supporting a 131k context window.
   - **RX 6600 XT (8 GB):** Occupies **6,779.3 MB** (82.76% of 8,192 MB), with **6,015.3 MB** dedicated to `llama-server.exe` supporting a 65k context window and leaving **1,412.7 MB** (17.24%) headroom.
3. **mmap Tensor Transfer vs Raw TCP vs FASTLINK-RPC/1:**
   - **CPU-only POSIX shared-memory copy (`/dev/shm`):**
     - Single-chunk (64 KiB) round-trip latency: **p50 = 0.001 ms (1 µs)**, **p95 = 0.001 ms**, **p99 = 0.001 ms**, **mean = 0.001 ms**.
     - 8 MiB tensor transfer throughput: **26,166.1 MB/s** (patterned incompressible) and **26,849.9 MB/s** (uniform).
     - The harness copies bytes into a shared mapping and then copies them into a consumer-thread buffer. It does not exercise a GPU and is not zero-copy.
   - **Raw TCP Socket Streaming:**
     - Single-chunk (64 KiB) latency: **p50 = 0.021 ms**, **p95 = 0.029 ms**, **p99 = 0.043 ms**.
     - 8 MiB throughput: **7,779.5 MB/s** (patterned) and **7,398.2 MB/s** (uniform). Bound by TCP stack syscalls and kernel copy buffers.
   - **FASTLINK-RPC/1 Framed Transport:**
     - Single-chunk (64 KiB) latency: **p50 = 0.113 ms**, **p95 = 0.196 ms**, **p99 = 0.215 ms** (0.092 ms above raw TCP p50 in this run; the difference includes framing, hash calculation, encoding/decoding, and length checks—not a separately isolated hash cost or frame-integrity check).
     - 8 MiB patterned throughput: **574.1 MB/s** (observed in this run; no CPU profile was collected to attribute the bottleneck).
     - 8 MiB uniform throughput: **1,930.9 MB/s** raw equivalent speed with **66,700 bytes on wire** (**−99.20% wire data bandwidth reduction**).

---

## 2. Multi-GPU VRAM Allocation & Hardware Telemetry

Hardware telemetry sampled via DRM sysfs on Linux (`NightKnightPC`) and Windows Performance Counters over SSH on Windows (`detti`):

| Node / Host | Physical GPU Adapter | Architecture | Total VRAM | Dedicated Used | Used % | Free VRAM | Process Dedicated | Compute Util |
|---|---|---|---:|---:|---:|---:|---:|---:|
| **`NightKnightPC` (Local)** | AMD Radeon RX 7900 XT | Navi 31 (RDNA 3) | **20,464.0 MB** | **19,510.8 MB** | **95.34%** | 953.2 MB | 18,800.0 MB | N/A (not in attached raw JSON) |
| **`detti` (Remote)** | AMD Radeon RX 6600 XT | Navi 23 (RDNA 2) | **8,192.0 MB** | **6,779.3 MB** | **82.76%** | 1,412.7 MB | 6,015.3 MB | N/A (raw JSON records 0.0; treat as unavailable) |
| **Combined Topology** | **Dual AMD GPU** | **RDNA 3 + 2** | **28,656.0 MB** | **26,290.1 MB** | **91.74%** | **2,365.9 MB** | **24,815.3 MB** | — |

---

## 3. Concurrent Independent GPU Inference Requests

Measured across standard deterministic prompt scenarios (`temperature=0.0`, `cache_prompt=false`):

| Scenario | Workload Scope | Prompt Tok | Output Tok | 7900 XT (tok/s) | 6600 XT (tok/s) | Dual-GPU Sum | 7900 TTFT | 6600 TTFT | Sum / 6600 concurrent rate |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|
| `short_32` | Smoke test / concept query | 15 | 32 | **85.83** | **31.58** | **117.41 tok/s** | 0.0999s | 0.2118s | **3.72x** |
| `medium_64` | Architecture trade-offs | 45 | 64 | **87.05** | **31.62** | **118.67 tok/s** | 0.1350s | 1.0597s | **3.75x** |
| `standard_128` | Concurrency & code review | 51 | 128 | **87.64** | **31.30** | **118.94 tok/s** | 0.1410s | 1.1078s | **3.80x** |
| `code_task_128` | Low-poly GLSL shader task | 38 | 128 | **86.97** | **31.02** | **117.99 tok/s** | 0.1260s | 0.9991s | **3.80x** |

*Ratio note:* the last column divides the dual-GPU sum by the RX 6600 XT rate measured during that same distributed run. It is not a speedup against the separately measured single-GPU baseline.

### Inference Latency Percentiles (Total Wall Time)

| Execution Mode | Min (s) | p50 (s) | p95 (s) | p99 (s) | Mean (s) |
|---|---:|---:|---:|---:|---:|
| **Node 0: RX 7900 XT Alone** | 0.5147 | 1.6778 | 1.6887 | 1.6887 | 1.2010 |
| **Node 1: RX 6600 XT Alone** | 1.5091 | 5.4779 | 5.5416 | 5.5416 | 4.0652 |
| **Concurrent Independent Requests (per-request wall-time distribution)** | 0.4858 | 1.6164 | 5.2425 | 5.2425 | 2.4245 |

---

## 4. Tensor Transfer Harnesses: CPU shared-memory copy vs Raw TCP vs FASTLINK-RPC/1

The older comparison below is from `cargo bench --bench mmap_transfer` (64 KiB chunks, 500 latency rounds; 8 MiB tensors, 32 throughput rounds). Its `mmap` branch is a CPU shared-memory memcpy loop: the producer copies into a POSIX mapping and the consumer thread copies out into a heap buffer. It does not exercise a GPU and is not zero-copy.

### 4.1. Single-Chunk (64 KiB) Round-Trip Latency

| Transfer Mechanism | Min Latency | Median (p50) | p95 Latency | p99 Latency | Mean Latency | Overhead vs mmap |
|---|---:|---:|---:|---:|---:|---|
| **`mmap` Shared Memory (`/dev/shm`)** | **0.001 ms** | **0.001 ms** | **0.001 ms** | **0.001 ms** | **0.001 ms** | Baseline (1.0x) |
| **Raw TCP Socket Streaming** | 0.017 ms | **0.021 ms** | 0.029 ms | 0.043 ms | 0.023 ms | +0.020 ms (21.0x) |
| **FASTLINK-RPC/1 Transport** | 0.091 ms | **0.113 ms** | 0.196 ms | 0.215 ms | 0.124 ms | +0.112 ms (113.0x) |

*Note on FASTLINK-RPC/1 latency:* the sender computes BLAKE3 chunk IDs while framing, but the adapter payload on this wire does not transmit those IDs. Receive-side validation checks codec output and decoded lengths only; it does not verify chunk-content integrity. The RTT also includes frame/decode work and cannot be ascribed solely to BLAKE3.

### 4.2. 8 MiB Tensor Transfer Throughput & Wire Efficiency

| Transfer Mechanism | 8 MiB Patterned (Incompressible) | 8 MiB Uniform (Compressible) | Wire Bytes per 8MB Tensor | Wire Bandwidth Delta |
|---|---:|---:|---:|---|
| **`mmap` CPU shared-memory copy** | **26,166.1 MB/s** | **26,849.9 MB/s** | N/A (no network wire) | Producer + consumer memcpy; 8 MiB destination buffer |
| **Raw TCP Socket Streaming** | **7,779.5 MB/s** | **7,398.2 MB/s** | 8,388,608 B (raw socket) | Kernel socket buffer copy |
| **FASTLINK-RPC/1 Transport** | **574.1 MB/s** | **1,930.9 MB/s** | **66,700 B (RLE)** | **−8,321,908 B (−99.20%)** |

### 4.3. Latest single-connection `rpc_latency` loopback snapshot

This is a different harness and measurement run from the `mmap_transfer` table above.
The earlier report's JSON does not record its host/kernel/CPU, so host identity
cannot be confirmed from that artifact. Snapshot
`rpc-latency-linux-2026-11-10-230038-76e77cc` ran `cargo bench --bench rpc_latency`
on Linux x86_64 / AMD Ryzen 7 7800X3D / rustc 1.99.0. Its machine-readable data
is `benches/results_rpc_latency.json`; captured benchmark stdout is
`benches/results_rpc_latency_20261110.txt`.

| Measurement | Result |
|---|---:|
| 64 KiB RTT median, raw TCP / framed RAW | 0.020 / 0.095 ms |
| 8 MiB patterned raw TCP / FASTLINK RAW (raw-equivalent) | 8,401.5 / 547.2 MB/s |
| Patterned FASTLINK wire throughput | 547.2 MB/s |
| FASTLINK framing overhead per patterned tensor | +652 B |
| 8 MiB uniform raw TCP / FASTLINK RLE (raw-equivalent) | 8,293.6 / 2,030.5 MB/s |
| Uniform FASTLINK wire throughput | 16.1 MB/s |
| Uniform encoded bytes per tensor | 66,700 B |

The raw/framed patterned throughput ratio was 15.4× in this one loopback run; it
is not a performance guarantee. This harness does not exercise a GPU, actual
`llama.cpp` RPC, LAN/WAN networking, multiple clients, or per-chunk integrity
verification. Full measurement definitions and repeatability guidance are in
`docs/BENCHMARKS.md`.

---

## 5. Architectural Findings & Strategic Recommendations

1. **Same-Host CPU Shared Memory:**
   - The `mmap_transfer` test is a CPU-to-CPU shared-memory copy microbenchmark, not a GPU staging or zero-copy benchmark. Its numbers do not establish an application-level recommendation; measure the actual consumer and copy path before choosing a transport.

2. **Cross-Node Transfer (Wire-Size Estimate Only):**
   - At an assumed 118 MB/s application payload rate, 8 MiB takes about 71.1 ms and 66,700 B takes about 0.57 ms on the wire (about 126× difference). This is arithmetic from payload sizes, not a measured FASTLINK network speedup; codec, protocol, latency, retransmits and endpoint costs are excluded.
   - The file-sync cache may reuse identical chunks. This `rpc_latency` tensor wire format carries no BLAKE3 chunk IDs, so cache-hit zero-byte reuse is not demonstrated by this transport benchmark.

3. **Asymmetric Dual-GPU Load Balancing:**
   - The RX 7900 XT generates tokens at ~86 tok/s while the RX 6600 XT generates tokens at ~31 tok/s (~2.77:1 compute ratio).
   - In distributed pipelined / tensor-parallel configurations, work allocation should be partitioned proportionally (e.g., 73% of layers/experts to 7900 XT and 27% to 6600 XT) to prevent pipeline stall and maximize collective throughput.
