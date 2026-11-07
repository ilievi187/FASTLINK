# FASTLINK v4 — Transfer Analysis, Chunk-Size Sweep & Strategy Comparison (`FL4-TR`)

**Author:** Antigravity (Transport & Synchronization)  
**Date:** 2026-11-08  
**Scope:** CPU-only analysis and deterministic codec/strategy benchmarks (`build-fl-gpu/bin/test-fastlink-rpc-codec --chunk-sweep`). No GPU execution, no model loading, no inference server run, no invented numbers.  
**Target Model Architecture:** `Qwen3.5-4B-3D-Coder-Q4_K_M.gguf` (`2,708,804,096` B = `2.5216 GiB`, `32` layers = **8 full-attention layers** at interval 4 `{3, 7, 11, 15, 19, 23, 27, 31}` + **24 recurrent/SSM layers**; `n_embd = 2560`, `head_count_kv = 4`, `head_dim = 256`, `ssm.inner_size = 4096`, `ssm.state_size = 128`, `ssm.conv_kernel = 4`).

---

## 1. Task 1 — FASTLINK v3 Layer-Split RPC Transfer Analysis

### 1.1 Wire Envelope Constants (from `ggml/src/ggml-rpc/ggml-rpc.cpp` & `fastlink-rpc-codec.h`)

Every RPC message on the TCP stream uses a packed binary envelope:
- **Outer RPC Request Header:** `rpc_cmd` (`1` B) + `request_size` (`8` B `u64` LE) = **`9` B**.
- **Outer RPC Response Header (for synchronous commands):** `response_size` (`8` B `u64` LE) + `response_data` (`response_size` B).
- **Serialized Tensor Descriptor (`sizeof(rpc_tensor)`):** **`296` B** (`id:8`, `type:4`, `buffer:8`, `ne[4]:16`, `nb[4]:16`, `op:4`, `op_params[16]:64`, `flags:4`, `src[10]:80`, `view_src:8`, `view_offs:8`, `data:8`, `name[64]:64`, `use_count:4`).
- **`RPC_CMD_SET_TENSOR` Header:** `sizeof(rpc_tensor)` (`296` B) + `cache_flag` (`1` B) + `offset` (`8` B) = **`305` B** (`+ 9` B outer request header = **`314` B** total framing before raw tensor bytes).
- **`RPC_CMD_FASTLINK_SET_TENSOR` (`FLR3`):** `9` B outer header + `56` B `FLR3` frame header (`"FLR3":4`, `raw_len:8`, `chunk_count:4`, `sha256:32`, `configured_chunk_bytes:4`, `policy_flags:1`, `link_mbps_hint:2`, `reserved:1`) + `305` B `SET_TENSOR` header + `10` B per chunk (`codec_id:1`, `policy_hint:1`, `orig_chunk_size:4`, `encoded_size:4`).
- **`RPC_CMD_SET_TENSOR_HASH`:** `9` B req header + `312` B (`rpc_tensor:296`, `offset:8`, `fnv_hash:8`) + `8` B rsp header + `1` B `result` = **`330` B** round-trip. Only triggered when `size > HASH_THRESHOLD` (**`10 MiB` = `10,485,760` B**) and `buffer->usage == GGML_BACKEND_BUFFER_USAGE_WEIGHTS` (`ggml-rpc.cpp:121,877`).
- **`RPC_CMD_GET_TENSOR`:** `9` B req header + `312` B (`rpc_tensor:296`, `offset:8`, `size:8`) + `8` B rsp header = **`329` B** control overhead + `size` B returned tensor payload.
- **`RPC_CMD_GRAPH_COMPUTE`:** `9` B req header + `20` B (`device:4`, `uid:8`, `n_nodes:4`, `n_tensors:4`) + `8 × n_nodes` B + `296 × n_tensors` B. (For a 10–16 layer remote subgraph with `~500–900` tensors, this is **`~150–280 KiB`** per uncached graph dispatch.)
- **`RPC_CMD_GRAPH_RECOMPUTE`:** `9` B req header + `12` B (`device:4`, `uid:8`) = **`21` B** total wire bytes (`ggml-rpc.cpp:241,1303`).

---

### 1.2 Byte Category Taxonomy & Per-Component Classification

| Category | Definition | Qwen3.5-4B Layer-Split Components | Wire Opcode(s) in v3 | Target Wire Behavior in v4 |
|---|---|---|---|---|
| **`STATIC`** | Immutable model weights & static tensor metadata sent once on cold load | Remote share of `2.5216 GiB` Q4_K_M weights (`~82 MiB/layer`: `attn_q/k/v/output`, `ssm_in/out/conv1d/dt_bias/A/D`, `ffn_gate/up/down`, `norms`) | `RPC_CMD_SET_TENSOR`, `RPC_CMD_FASTLINK_SET_TENSOR`, `RPC_CMD_INIT_TENSOR` | Send once on cold boot; persist in remote VRAM across sessions/requests (`0` B after load) |
| **`REUSABLE`** | Previously synchronized weights, cached compute graphs (`stored_graphs`), and unchanged prefix KV / SSM checkpoints | Cached weight tensors on warm reload; `cgraph` topology across steps; prefix KV blocks (`8` attn layers) & saved SSM state snapshots (`24` SSM layers) | `RPC_CMD_SET_TENSOR_HASH` (`330` B/tensor if `>10 MiB`), `RPC_CMD_GRAPH_RECOMPUTE` (`21` B) | **`0` payload bytes** on warm/repeated requests via persistent handle table & lowered hash threshold (`>=64 KiB`) |
| **`APPEND-ONLY`** | Newly added KV cache entries for the **8 full-attention layers** (`{3, 7, 11, 15, 19, 23, 27, 31}`) | Per token across all 8 attn layers: `16,384` elements = **`32 KiB/tok` (`f16`)**, **`18 KiB/tok` (`q8_0`)**, or **`10 KiB/tok` (`q4_0`)** (`1,280 B/layer/tok` in `q4_0`) | Updated **in-place** on remote GPU during normal decode (`0` wire B); `SET_TENSOR` at `offset` only if migrating/restoring KV from host | In-place remote GPU update (`0` wire B); if host-offloaded/migrated, send **delta-slice (`offset` + new tokens only)** |
| **`DYNAMIC`** | Intermediate activations crossing the layer-split boundary, per-step token positions/masks, and output hidden state / logits | Boundary residual stream `[2560, n_tokens]` `f32` = **`10,240 B/token`** (`10 KiB` for decode `n_tokens=1`; `5.00 MiB` for `512`-token prefill ubatch) + `inp_pos`/`inp_mask` (`~0.5–8 KiB`) + return activation/logits | `ggml_backend_rpc_set_tensor_async` (`RPC_CMD_SET_TENSOR`) & `RPC_CMD_GET_TENSOR` | Minimize split cuts (`<=1` boundary); keep `output.weight` on final device or return only `[2560, 1]` (`10 KiB`) instead of full vocab logits (`~600 KiB`) |
| **`CONTROL`** | RPC handshakes, buffer alloc/free/clear, alloc-size queries, graph UID recompute triggers, sync barriers | `HELLO` (`29` B), `ALLOC_BUFFER` (`29` B), `GET_ALLOC_SIZE` (`3,285` B), `MEMSET_TENSOR` (`322` B), `GRAPH_RECOMPUTE` (`21` B), `GRAPH_COMPUTE` (`~150–280 KiB` on UID miss) | `HELLO`, `ALLOC_BUFFER`, `GET_ALLOC_SIZE`, `GRAPH_COMPUTE`, `GRAPH_RECOMPUTE` | Keep `cgraph->uid` stable so steady decode uses `21` B `GRAPH_RECOMPUTE` (`0` full graph re-sends) |

---

### 1.3 Hybrid Model Layer Analysis: 8 Full-Attention vs 24 Recurrent/SSM Layers

In `Qwen3.5-4B-3D-Coder-Q4_K_M`:
1. **8 Full-Attention Layers (`layers 3, 7, 11, 15, 19, 23, 27, 31`):**
   - State type: **`APPEND-ONLY` KV Cache**.
   - Per-layer per-token KV size: `2 (K,V) × 4 KV_heads × 256 head_dim = 2,048` elements/token:
     - `f16`: `4,096 B/layer/token` (`32,768 B/token` across all 8 layers; `4.00 GiB` at `128K`).
     - `q8_0`: `2,304 B/layer/token` (`18,432 B/token` across all 8 layers; `2.33 GiB` at `128K`).
     - `q4_0`: `1,280 B/layer/token` (`10,240 B/token` across all 8 layers; `1.25 GiB` at `128K`).
   - Prefix truncation / rollback: **Zero-copy index rollback** (`kv_self.head` pointer adjustment; `0` bytes transferred).
2. **24 Recurrent / SSM Layers (all 24 non-4th layers):**
   - State type: **Fixed-size `DYNAMIC` (in-place mutated) recurrent state**.
   - Per-layer state size (independent of context length from `8K` to `128K`):
     - Recurrent state matrix `ssm_s`: `state_size(128) × inner_size(4096)` `f32` = `524,288` floats = **`2,097,152` B (`2.00 MiB`)**.
     - Conv1d history buffer `ssm_conv`: `(conv_kernel(4) - 1) × inner_size(4096)` `f32` $\approx$ **`48–64 KiB`**.
     - Total per SSM layer: **`~2.06 MiB`** (`~49.5 MiB` total across all 24 SSM layers).
   - **Critical Hybrid Difference for Prefix Caching & Reconnect:** Unlike the 8 attention layers (whose history for tokens `0..t` remains intact in the KV buffer), the 24 SSM layers **overwrite their `2.06 MiB` state in-place on every token**. Restoring a prefix at token `t` without re-running prefill requires snapshotting the `2.06 MiB/layer` SSM state at checkpoint boundaries (in remote VRAM or host RAM).
3. **What Crosses the Wire per Decode Step (`n_tokens = 1`) in Layer-Split Mode (`-sm layer`):**
   - Because both the KV cache (for remote attention layers) and the SSM state (for remote recurrent layers) live in **remote GPU buffers**, **neither KV cache nor SSM state crosses the network during normal prefill or decode**.
   - At a single layer-split cut (e.g. `70/30` split: layers `0..21` on Local 7900 XT, layers `22..31` [which includes 2 attention layers `{23, 27, 31}` and 7 SSM layers] on Remote 6600 XT):
     - **Local $\rightarrow$ Remote (`DYNAMIC`):** Hidden state activation `[2560, 1]` `f32` = **`10,240` B** (`+ 314` B `SET_TENSOR` header = `10,554` B) + small position/mask/index inputs (`~1–4 KiB`).
     - **Local $\rightarrow$ Remote (`CONTROL`):** `RPC_CMD_GRAPH_RECOMPUTE` = **`21` B** (when `uid` is cached in `stored_graphs`).
     - **Remote $\rightarrow$ Local (`DYNAMIC`):**
       - If `output_norm` + `output.weight` (`lm_head`) run on Local: `RPC_CMD_GET_TENSOR` fetches `[2560, 1]` `f32` = **`10,240` B** (`+ 329` B framing = `10,569` B) $\rightarrow$ **Total per decode step $\approx 22–26$ KiB** (`0.024 ms` wire time at `926 MiB/s` LAN, dominated by `1–2 × RTT`).
       - If `output.weight` runs on Remote and full vocabulary logits (`vocab_size ≈ 151,936 × 4 B = 607,744 B`) are fetched back to Host via `RPC_CMD_GET_TENSOR` for CPU sampling: **`~608 KiB` per decode step** (`0.63 ms` wire time at `926 MiB/s` LAN, or `19.5 ms/tok` at `250 Mbps`!).

---

### 1.4 Per-Phase Network Transfer Breakdown & v3 Bottlenecks Identified

| Phase | Data Crossing the Wire in FASTLINK v3 | Category Mix | Quantified Wire Volume (Qwen3.5-4B, 70/30 Split = 10 Remote Layers) | v3 Bottleneck Found in `ggml-rpc.cpp` |
|---|---|---|---|---|
| **1. Model Load (Cold)** | `HELLO`, `GET_DEVICE_MEMORY`, `GET_ALLOC_SIZE`, `ALLOC_BUFFER`, `INIT_TENSOR`, `SET_TENSOR` / `FASTLINK_SET_TENSOR` for remote layers | `99.9% STATIC`, `0.1% CONTROL` | **`~775 MiB`** (`10/32` of `2.52 GiB`) + `~30 KiB` control | Q4_K_M weights are near-random entropy; `FLR1` RLE falls back to RAW (`ratio 1.0000`). |
| **2. Model Load (Warm Reload, `rpc-server -c`)** | `SET_TENSOR_HASH` (`330` B) for tensors `> 10 MiB`; **full `SET_TENSOR` for all weight tensors `<= 10 MiB`** | `REUSABLE` (partially leaked as `STATIC`) + `CONTROL` | Tensors `> 10 MiB`: `330 B` each (`0` payload). **Tensors `<= 10 MiB`: `~180–320 MiB` re-sent in full!** | **`HASH_THRESHOLD = 10 MiB` (`ggml-rpc.cpp:121`)** skips hash caching for all norms, SSM params, and smaller projection matrices! |
| **3. Prefill (`N_prompt` tokens, `512`-tok ubatches)** | Per ubatch: boundary activation `[2560, 512]` `f32` (`5.00 MiB`) + `inp_pos`/mask (`~16 KiB`) + 1st ubatch `GRAPH_COMPUTE` (`~200 KiB`), next ubatches `GRAPH_RECOMPUTE` (`21` B) + output (`10 KiB` or `608 KiB`) | `96% DYNAMIC`, `4% CONTROL` | **`~5.02–5.62 MiB` per 512-token ubatch** (`~10.3 KiB/token` across split boundary; **`~1.28 GiB` over a `128K` prefill**) | **`ggml_backend_rpc_set_tensor_async` (`ggml-rpc.cpp:1202`) bypasses `GGML_RPC_FASTLINK_PACK`** — activations never use `FLR3` compression! |
| **4. Decode Step (`1` token)** | Boundary activation `[2560, 1]` `f32` (`10,240` B) + `inp_pos`/mask (`~2 KiB`) + `GRAPH_RECOMPUTE` (`21` B) + `GET_TENSOR` return (`10,240` B hidden or `607,744` B logits) | `98% DYNAMIC`, `2% CONTROL` | **`~23 KiB/token`** (if logits sampled on device or `lm_head` local) or **`~620 KiB/token`** (if full `f32` logits fetched from remote) | Synchronous `GET_TENSOR` + `SET_TENSOR` incurs round-trip RTT per step; full remote logits transfer adds `~608 KiB/token` if not reduced on device. |
| **5. KV Extension (`8K -> 128K`)** | Normal append: `0` wire bytes (in-place remote GPU write). If KV buffer reallocated (`FREE_BUFFER` + `ALLOC_BUFFER`): wipes `stored_graphs` (`ggml-rpc.cpp:1626`) | `APPEND-ONLY` (in-place) + `CONTROL` | **`0` B** when pre-allocated to `-c 131072`; **`~200 KiB`** `GRAPH_COMPUTE` penalty if any buffer is freed | `rpc_server::free_buffer` invalidates **all** cached graphs in `stored_graphs`, forcing full `GRAPH_COMPUTE` re-serialization. |
| **6. Repeated Request (Same Prompt / Prefix)** | Weights already in remote VRAM (`0` B). If KV/SSM state retained: `0` B for prefix. If SSM state not checkpointed: must replay SSM layers over prefix (`10 KiB/tok` boundary activations) | `REUSABLE` vs `DYNAMIC` | **`~23 KiB`** (with remote KV + SSM checkpoint hit) vs **`~10.3 KiB × N_prefix`** (if prefix re-evaluated across split) | No remote SSM state checkpointing in v3: partial prefix rollback invalidates the 24 in-place SSM states. |
| **7. Concurrent Sessions** | Shared `rpc_dispatcher` (`ggml-rpc.cpp:740`) serializes commands over one TCP socket; per-slot KV views share remote buffer | `DYNAMIC` + `CONTROL` | Scales linearly with active ubatch tokens (`10,240 B × B_tokens` per step) | Single `rpc_dispatcher` mutex/queue per endpoint; separate client TCP connections block in single-threaded `rpc_serve_client` accept loop. |
| **8. Disconnect / Reconnect** | Socket close destroys stack-allocated `rpc_server` (`ggml-rpc.cpp:2483,2491`), freeing **all** remote GPU buffers & `stored_graphs` | `STATIC` + `REUSABLE` + `DYNAMIC` (all lost in VRAM) | **`~180–775 MiB` re-sent** on reconnect + full KV/SSM state lost | **`rpc_server` lifetime is bound to a single TCP connection** — any transient socket drop destroys all remote VRAM buffers! |
| **9. Cache Hit vs Miss** | Weight hash cache (`SET_TENSOR_HASH`): Hit = `330` B (`0.873 ms/MiB` FNV-1a CPU scan); Miss = `330` B + full tensor (`1,048,890` B/MiB) | `REUSABLE` vs `STATIC` | Hit: **`330` B** (`0.0003` ratio); Miss: **`1,049,220` B/MiB** (`1.0003` ratio) | FNV-1a hashes the entire tensor on CPU (`0.873 ms/MiB`) on every check and ignores tensors `<= 10 MiB`. |

---

## 2. Task 2 — Logical Chunk-Size Sweep (`512B` – `256K`, CPU-Only)

Measured via:
```bash
/mnt/gamez/AI/llama.cpp-fastlink-rpc-adapter/build-fl-gpu/bin/test-fastlink-rpc-codec --chunk-sweep
```
All measurements are on a **1 MiB (`1,048,576` B)** tensor with the `305`-byte `SET_TENSOR` header (`stock_wire_bytes = 1,048,881` B; `+ 9` B RPC command header = `1,048,890` B).

### 2.1 Framing & Metadata Overhead by Chunk Size (`FLR1` vs `FLR3`, 1 MiB Tensor)

- `FLR1` framing = `48` B outer (`"FLR1":4 + raw_len:8 + chunks:4 + sha256:32`) + `5 × N_chunks` B (`mode:1 + encoded_size:4`).
- `FLR3` framing = `56` B outer (`FLR1:48 + configured_chunk_bytes:4 + policy_flags:1 + link_mbps_hint:2 + reserved:1`) + `10 × N_chunks` B (`codec_id:1 + policy_hint:1 + orig_chunk_size:4 + encoded_size:4`).

| Chunk Size | Chunks / 1 MiB | `FLR1` Framing (B) | `FLR3` Framing (B) | `FLR3` vs `FLR1` Delta (B) | `FLR3` Framing % of 1 MiB |
|---|---:|---:|---:|---:|---:|
| `512B` (`512` B) | `2,048` | `10,288` | `20,536` | `+10,248` | `1.9585%` |
| `1K` (`1,024` B) | `1,024` | `5,168` | `10,296` | `+5,128` | `0.9819%` |
| `2K` (`2,048` B) | `512` | `2,608` | `5,176` | `+2,568` | `0.4936%` |
| `4K` (`4,096` B) | `256` | `1,328` | `2,616` | `+1,288` | `0.2495%` |
| `8K` (`8,192` B) | `128` | `688` | `1,336` | `+648` | `0.1274%` |
| `16K` (`16,384` B) | `64` | `368` | `696` | `+328` | `0.0664%` |
| `32K` (`32,768` B) | `32` | `208` | `376` | `+168` | `0.0359%` |
| `64K` (`65,536` B) | `16` | `128` | `216` | `+88` | `0.0206%` |
| `256K` (`262,144` B) | `4` | `68` | `96` | `+28` | `0.0092%` |

### 2.2 Chunk-Size × Codec Cost Table (`512B` – `256K` across `RAW`, `RLE`, `LZ4`, `ZSTD`)

`Total Cost (ms) = encode_ms + wire_ms + decode_ms`, where `wire_ms` includes `FLR3 Wire (B) = 305 + flr3_framing_bytes + encoded_payload_bytes`, evaluated at both:
1. **Measured LAN bandwidth (`926 MiB/s` = `970,981 B/ms`, from `MEMORY_BUDGET.md` §4)**, and
2. **Constrained/tunnel reference link (`250 Mbps` = `31,250 B/ms`)**.

| Chunk Size | Codec | Workload Class | Encoded Payload (B) | `FLR1` Wire (B) | `FLR3` Wire (B) | `FLR3` Wire Ratio | Encode (ms) | Decode (ms) | Total Cost @ 926 MiB/s LAN (ms) | Total Cost @ 250 Mbps (ms) |
|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|
| `512B` | `RAW` (`0`) | `incompressible-random` | `1,048,576` | `1,059,169` | `1,069,417` | `1.0196` | `0.0286` | `0.0323` | `1.1622` | `34.2822` |
| `512B` | `RLE` (`1`) | `RLE-friendly` | `32,768` | `43,361` | `53,609` | `0.0511` | `0.5272` | `0.0405` | `0.6230` | `2.2833` |
| `512B` | `LZ4` (`2`) | `LZ4-structured` | `145,408` | `156,001` | `166,249` | `0.1585` | `0.7538` | `0.4737` | `1.3988` | `6.5476` |
| `512B` | `LZ4` (`2`) | `BF16-tensor-like` | `1,054,720` | `1,065,313` | `1,075,561` | `1.0254` | `0.7105` | `0.0656` | `1.8839` | `35.1941` |
| `512B` | `ZSTD` (`3`) | `BF16-tensor-like` | `567,296` | `577,889` | `588,137` | `0.5607` | `3.0687` | `0.6042` | `4.2787` | `22.4934` |
| `1K` | `RAW` (`0`) | `incompressible-random` | `1,048,576` | `1,054,049` | `1,059,177` | `1.0098` | `0.0224` | `0.0225` | `1.1357` | `33.9385` |
| `1K` | `RLE` (`1`) | `RLE-friendly` | `32,768` | `38,241` | `43,369` | `0.0413` | `0.5217` | `0.0376` | `0.6040` | `1.9471` |
| `1K` | `LZ4` (`2`) | `LZ4-structured` | `74,752` | `80,225` | `85,353` | `0.0814` | `0.5135` | `0.4779` | `1.0793` | `3.7227` |
| `1K` | `LZ4` (`2`) | `BF16-tensor-like` | `540,112` | `545,585` | `550,713` | `0.5250` | `0.5109` | `0.2888` | `1.3669` | `18.4226` |
| `1K` | `ZSTD` (`3`) | `BF16-tensor-like` | `305,152` | `310,625` | `315,753` | `0.3010` | `3.2405` | `1.1007` | `4.6665` | `14.4454` |
| `2K` | `RAW` (`0`) | `incompressible-random` | `1,048,576` | `1,051,489` | `1,054,057` | `1.0049` | `0.0196` | `0.0198` | `1.1250` | `33.7692` |
| `2K` | `RLE` (`1`) | `RLE-friendly` | `32,768` | `35,681` | `38,249` | `0.0365` | `0.4833` | `0.0330` | `0.5557` | `1.7403` |
| `2K` | `LZ4` (`2`) | `LZ4-structured` | `39,424` | `42,337` | `44,905` | `0.0428` | `0.4118` | `0.4884` | `0.9464` | `2.3371` |
| `2K` | `LZ4` (`2`) | `BF16-tensor-like` | `539,600` | `542,513` | `545,081` | `0.5197` | `0.4511` | `0.2688` | `1.2813` | `18.1625` |
| `2K` | `ZSTD` (`3`) | `BF16-tensor-like` | `166,400` | `169,313` | `171,881` | `0.1639` | `2.3145` | `1.2646` | `3.7561` | `9.0792` |
| `4K` | `RAW` (`0`) | `incompressible-random` | `1,048,576` | `1,050,209` | `1,051,497` | `1.0025` | `0.0205` | `0.0202` | `1.1236` | `33.6886` |
| `4K` | `RLE` (`1`) | `RLE-friendly` | `32,768` | `34,401` | `35,689` | `0.0340` | `0.4900` | `0.0271` | `0.5539` | `1.6592` |
| `4K` | `LZ4` (`2`) | `LZ4-structured` | `21,760` | `23,393` | `24,681` | `0.0235` | `0.3161` | `0.4684` | `0.8099` | `1.5743` |
| `4K` | `LZ4` (`2`) | `BF16-tensor-like` | `539,344` | `540,977` | `542,265` | `0.5170` | `0.4215` | `0.2749` | `1.2549` | `18.0489` |
| `4K` | `ZSTD` (`3`) | `BF16-tensor-like` | `97,280` | `98,913` | `100,201` | `0.0955` | `2.0195` | `1.2550` | `3.3777` | `6.4810` |
| `8K` | `RAW` (`0`) | `incompressible-random` | `1,048,576` | `1,049,569` | `1,050,217` | `1.0013` | `0.0184` | `0.0191` | `1.1191` | `33.6445` |
| `8K` | `RLE` (`1`) | `RLE-friendly` | `32,768` | `33,761` | `34,409` | `0.0328` | `0.5066` | `0.0310` | `0.5730` | `1.6387` |
| `8K` | `LZ4` (`2`) | `LZ4-structured` | `12,928` | `13,921` | `14,569` | `0.0139` | `0.4319` | `0.6370` | `1.0839` | `1.5351` |
| `8K` | `LZ4` (`2`) | `BF16-tensor-like` | `284,432` | `285,425` | `286,073` | `0.2727` | `0.4575` | `0.4450` | `1.1971` | `10.0568` |
| `8K` | `ZSTD` (`3`) | `BF16-tensor-like` | `62,976` | `63,969` | `64,617` | `0.0616` | `1.8556` | `1.1475` | `3.0696` | `5.0708` |
| `16K` | `RAW` (`0`) | `incompressible-random` | `1,048,576` | `1,049,249` | `1,049,577` | `1.0007` | `0.0218` | `0.0234` | `1.1262` | `33.6317` |
| `16K` | `RLE` (`1`) | `RLE-friendly` | `32,768` | `33,441` | `33,769` | `0.0322` | `0.7378` | `0.0361` | `0.8087` | `1.8546` |
| `16K` | `LZ4` (`2`) | `LZ4-structured` | `8,512` | `9,185` | `9,513` | `0.0091` | `0.4024` | `0.5984` | `1.0106` | `1.3052` |
| `16K` | `LZ4` (`2`) | `BF16-tensor-like` | `153,672` | `154,345` | `154,673` | `0.1475` | `0.4644` | `0.4511` | `1.0748` | `5.8650` |
| `16K` | `ZSTD` (`3`) | `BF16-tensor-like` | `43,200` | `43,873` | `44,201` | `0.0421` | `1.2810` | `0.8889` | `2.2154` | `3.5843` |
| `32K` | `RAW` (`0`) | `incompressible-random` | `1,048,576` | `1,049,089` | `1,049,257` | `1.0004` | `0.0196` | `0.0189` | `1.1191` | `33.6147` |
| `32K` | `RLE` (`1`) | `RLE-friendly` | `32,768` | `33,281` | `33,449` | `0.0319` | `0.4909` | `0.0261` | `0.5514` | `1.5874` |
| `32K` | `LZ4` (`2`) | `LZ4-structured` | `6,336` | `6,849` | `7,017` | `0.0067` | `0.2878` | `0.5274` | `0.8224` | `1.0397` |
| `32K` | `LZ4` (`2`) | `BF16-tensor-like` | `88,512` | `89,025` | `89,193` | `0.0850` | `0.4635` | `0.4861` | `1.0414` | `3.8037` |
| `32K` | `ZSTD` (`3`) | `BF16-tensor-like` | `32,800` | `33,313` | `33,481` | `0.0319` | `1.2666` | `0.8356` | `2.1367` | `3.1737` |
| **`64K`** | `RAW` (`0`) | `incompressible-random` | `1,048,576` | `1,049,009` | `1,049,097` | `1.0002` | `0.0181` | `0.0182` | `1.1167` | `33.6074` |
| **`64K`** | `RLE` (`1`) | `RLE-friendly` | `32,768` | `33,201` | `33,289` | `0.0317` | `0.4807` | `0.0246` | **`0.5397`** | **`1.5706`** |
| **`64K`** | `LZ4` (`2`) | `LZ4-structured` | `5,216` | `5,649` | `5,737` | `0.0055` | `0.2552` | `0.4740` | **`0.7352`** | **`0.9129`** |
| **`64K`** | `LZ4` (`2`) | `BF16-tensor-like` | `58,128` | `58,561` | `58,649` | `0.0559` | `0.4728` | `0.5729` | **`1.1061`** | `2.9224` |
| **`64K`** | `ZSTD` (`3`) | `BF16-tensor-like` | `25,568` | `26,001` | `26,089` | `0.0249` | `1.2007` | `0.7860` | `2.0136` | **`2.8216`** |
| **`256K`** | `RAW` (`0`) | `incompressible-random` | `1,048,576` | `1,048,949` | `1,048,977` | `1.0001` | `0.0171` | `0.0175` | `1.1150` | `33.6019` |
| **`256K`** | `RLE` (`1`) | `RLE-friendly` | `32,768` | `33,141` | `33,169` | `0.0316` | `0.4684` | `0.0243` | **`0.5269`** | **`1.5542`** |
| **`256K`** | `LZ4` (`2`) | `LZ4-structured` | `4,388` | `4,761` | `4,789` | `0.0046` | `0.2431` | `0.4839` | **`0.7319`** | **`0.8802`** |
| **`256K`** | `LZ4` (`2`) | `BF16-tensor-like` | `38,780` | `39,153` | `39,181` | `0.0374` | `0.3671` | `0.5127` | **`0.9201`** | **`2.1336`** |
| **`256K`** | `ZSTD` (`3`) | `BF16-tensor-like` | `20,136` | `20,509` | `20,537` | `0.0196` | `1.0615` | `0.8272` | `1.9099` | `2.5459` |

### Key Conclusions from the 9-Chunk-Size Sweep
1. **Small chunks (`512B – 4K`) severely degrade both compression ratio and CPU time for dictionary codecs (`LZ4`, `ZSTD`):**
   - At `512B`, `LZ4` cannot find cross-block matches on `BF16-tensor-like` (`wire_ratio = 1.0254`, expanding the frame) and `ZSTD` takes `3.67 ms` CPU time for only `0.5607` ratio (`588,137` B).
   - Even for small decode-step tensors (`10,240` B = `10 KiB` boundary activation `[2560, 1]`), chunking into `20 × 512B` chunks adds `200` B of chunk headers and destroys match history; encoding a `< 64 KiB` tensor as **1 single chunk (`orig_chunk_size = tensor_bytes`)** is strictly superior.
2. **`64K` (`65,536` B) to `256K` (`262,144` B) is the sweet spot for large transfers (`>= 128 KiB`):**
   - Framing overhead drops to **`0.0206%` (`216` B/MiB) at `64K`** and **`0.0092%` (`96` B/MiB) at `256K`**.
   - On `BF16-tensor-like`, moving from `8K` to `64K` improves `LZ4` wire ratio by **`4.9×`** (`0.2727` $\rightarrow$ `0.0559`) and `ZSTD` wire ratio by **`2.5×`** (`0.0616` $\rightarrow$ `0.0249`), while **reducing** `ZSTD` CPU time from `3.00 ms` to `1.99 ms`.
3. **High-speed LAN (`926 MiB/s`) vs Constrained Link (`250 Mbps`) Codec Crossover:**
   - At **`926 MiB/s` LAN**, `1 MiB` of raw wire time is only **`1.0802 ms`**. Therefore:
     - `RLE` (`0.53 ms` total) and `LZ4` (`0.73–0.92 ms` total at `64K–256K`) **beat raw transfer (`1.12 ms`)** when software SHA-256 (`2.68 ms/MiB`) is omitted or replaced by hardware-accelerated/lightweight checksums.
     - `ZSTD` (`1.91–2.01 ms` CPU+wire at `64K–256K`) is **slower than `LZ4` (`0.92–1.11 ms`) and `send-raw` (`1.12 ms`) on a `926 MiB/s` LAN**, and only wins when link bandwidth drops below **`~300 Mbps`**!

---

## 3. Task 3 — Strategy Comparison Across Representative Tensor Classes (`1 MiB`)

Measured on `1 MiB` (`1,048,576` B tensor + `305` B `SET_TENSOR` header + `9` B RPC cmd = `1,048,890` B baseline):
- **`send-raw (SET_TENSOR)`:** Stock uncompressed `RPC_CMD_SET_TENSOR` (`1,048,890` B wire; `0.0366 ms` CPU `memcpy`).
- **`compressed (FLR3 + SHA256)`:** Current `encode_v3` + `decode_with_metadata` (`64K` chunks) including full software SHA-256 on sender and receiver (`~5.35 ms` combined SHA-256 overhead).
- **`compressed (codec-only, no SHA256)`:** Same `FLR3` adaptive chunk codec (`64K` chunks) without redundant software SHA-256 over TCP/Tailscale (which already provides Poly1305/WireGuard or TCP CRC integrity).
- **`delta-slice (5% offset SET_TENSOR)`:** Sub-tensor range update (`RPC_CMD_SET_TENSOR` at `offset` for only the `5%` = `52,428` B modified/appended slice, e.g. `APPEND-ONLY` KV growth).
- **`delta-xor (5% dirty + FLR3)`:** Full-tensor XOR against previous version (`5%` dirty bytes) compressed with `FLR3`.
- **`hash-only (v3 SET_TENSOR_HASH)`:** CPU `fnv_hash` scan over `1 MiB` (`0.8732 ms`) + `330` B wire round-trip.
- **`reference-only (v4 persistent handle)`:** Lookup by `(buffer_id, tensor_id, epoch)` when tensor is immutable/clean on remote worker (`0` B tensor wire, `0.0002 ms` table lookup).
- **`recompute (GRAPH_RECOMPUTE 21B)`:** `21` B wire command (`RPC_CMD_GRAPH_RECOMPUTE`) + remote GPU op execution (`~0.035 ms`).

| Tensor Class (1 MiB) | Strategy | Wire Bytes (B) | Wire Ratio | CPU Host+Peer (ms) | Wire @ 926 MiB/s LAN (ms) | Total @ 926 MiB/s LAN (ms) | Total @ 250 Mbps (ms) |
|---|---|---:|---:|---:|---:|---:|---:|
| **`incompressible-random`** *(Q4_K_M weights, high-entropy activations)* | `send-raw (SET_TENSOR)` | `1,048,890` | `1.0000` | `0.0366` | `1.0802` | **`1.1168`** *(best cold)* | **`33.6011`** *(best cold)* |
| `incompressible-random` | `compressed (FLR3 + SHA256)` | `1,048,890` | `1.0000` | `3.3309` | `1.0802` | `4.4111` | `36.8954` |
| `incompressible-random` | `compressed (codec-only, no SHA256)` | `1,048,890` | `1.0000` | `0.0372` | `1.0802` | `1.1175` | `33.6017` |
| `incompressible-random` | `delta-slice (5% offset SET_TENSOR)` | `52,742` | `0.0503` | `0.0018` | `0.0543` | **`0.0561`** *(best partial)* | **`1.6896`** *(best partial)* |
| `incompressible-random` | `delta-xor (5% dirty + FLR3)` | `8,794` | `0.0084` | `6.1644` | `0.0091` | `6.1734` | `6.4458` |
| `incompressible-random` | `hash-only (v3 SET_TENSOR_HASH)` | `330` | `0.0003` | `0.8732` | `0.0003` | `0.8735` | `0.8838` |
| `incompressible-random` | `reference-only (v4 persistent handle)` | **`0`** | `0.0000` | `0.0002` | `0.0000` | **`0.0002`** *(best warm)* | **`0.0002`** *(best warm)* |
| `incompressible-random` | `recompute (GRAPH_RECOMPUTE 21B)` | `21` | `0.0000` | `0.0350` | `0.0000` | `0.0350` | `0.0357` |
| **`zero-filled`** *(empty KV buffers, padding, cleared state)* | `send-raw (SET_TENSOR)` | `1,048,890` | `1.0000` | `0.0366` | `1.0802` | `1.1168` | `33.6011` |
| `zero-filled` | `compressed (FLR3 + SHA256)` | `8,786` | `0.0084` | `5.3018` | `0.0090` | `5.3108` | `5.5829` |
| `zero-filled` | `compressed (codec-only, no SHA256)` | `8,786` | `0.0084` | `0.3453` | `0.0090` | **`0.3544`** *(best full send)* | **`0.6265`** *(best full send)* |
| `zero-filled` | `delta-slice (5% offset SET_TENSOR)` | `52,742` | `0.0503` | `0.0018` | `0.0543` | `0.0561` | `1.6896` |
| `zero-filled` | `delta-xor (5% dirty + FLR3)` | `8,794` | `0.0084` | `6.3547` | `0.0091` | `6.3637` | `6.6361` |
| `zero-filled` | `hash-only (v3 SET_TENSOR_HASH)` | `330` | `0.0003` | `0.8732` | `0.0003` | `0.8735` | `0.8838` |
| `zero-filled` | `reference-only (v4 persistent handle)` | **`0`** | `0.0000` | `0.0002` | `0.0000` | **`0.0002`** | **`0.0002`** |
| `zero-filled` | `recompute / MEMSET_TENSOR (322B)` | `21` | `0.0000` | `0.0350` | `0.0000` | **`0.0350`** *(best init)* | **`0.0357`** *(best init)* |
| **`RLE-friendly`** *(sparse masks, quantized zero-heavy blocks)* | `send-raw (SET_TENSOR)` | `1,048,890` | `1.0000` | `0.0366` | `1.0802` | `1.1168` | `33.6011` |
| `RLE-friendly` | `compressed (FLR3 + SHA256)` | `33,298` | `0.0317` | `6.3877` | `0.0343` | `6.4220` | `7.4532` |
| `RLE-friendly` | `compressed (codec-only, no SHA256)` | `33,298` | `0.0317` | `0.4889` | `0.0343` | **`0.5232`** *(best full send)* | **`1.5545`** *(best full send)* |
| `RLE-friendly` | `delta-slice (5% offset SET_TENSOR)` | `52,742` | `0.0503` | `0.0018` | `0.0543` | **`0.0561`** *(best partial)* | `1.6896` |
| `RLE-friendly` | `delta-xor (5% dirty + FLR3)` | `8,794` | `0.0084` | `7.2645` | `0.0091` | `7.2736` | `7.5459` |
| `RLE-friendly` | `hash-only (v3 SET_TENSOR_HASH)` | `330` | `0.0003` | `0.8732` | `0.0003` | `0.8735` | `0.8838` |
| `RLE-friendly` | `reference-only (v4 persistent handle)` | **`0`** | `0.0000` | `0.0002` | `0.0000` | **`0.0002`** *(best warm)* | **`0.0002`** *(best warm)* |
| `RLE-friendly` | `recompute (GRAPH_RECOMPUTE 21B)` | `21` | `0.0000` | `0.0350` | `0.0000` | **`0.0350`** *(best mask)* | **`0.0357`** *(best mask)* |
| **`LZ4-structured`** *(repeating block tables, index/pos tensors)* | `send-raw (SET_TENSOR)` | `1,048,890` | `1.0000` | `0.0366` | `1.0802` | `1.1168` | `33.6011` |
| `LZ4-structured` | `compressed (FLR3 + SHA256)` | `5,746` | `0.0055` | `8.1511` | `0.0059` | `8.1570` | `8.3349` |
| `LZ4-structured` | `compressed (codec-only, no SHA256)` | `5,746` | `0.0055` | `0.8432` | `0.0059` | **`0.8491`** *(best full send)* | **`1.0271`** *(best full send)* |
| `LZ4-structured` | `delta-slice (5% offset SET_TENSOR)` | `52,742` | `0.0503` | `0.0018` | `0.0543` | **`0.0561`** *(best partial)* | `1.6896` |
| `LZ4-structured` | `delta-xor (5% dirty + FLR3)` | `8,794` | `0.0084` | `8.2454` | `0.0091` | `8.2544` | `8.5268` |
| `LZ4-structured` | `hash-only (v3 SET_TENSOR_HASH)` | `330` | `0.0003` | `0.8732` | `0.0003` | `0.8735` | `0.8838` |
| `LZ4-structured` | `reference-only (v4 persistent handle)` | **`0`** | `0.0000` | `0.0002` | `0.0000` | **`0.0002`** *(best warm)* | **`0.0002`** *(best warm)* |
| `LZ4-structured` | `recompute (GRAPH_RECOMPUTE 21B)` | `21` | `0.0000` | `0.0350` | `0.0000` | **`0.0350`** | **`0.0357`** |
| **`BF16-tensor-like`** *(strided 16-bit FP activations / KV)* | `send-raw (SET_TENSOR)` | `1,048,890` | `1.0000` | `0.0366` | `1.0802` | **`1.1168`** *(best LAN full)* | `33.6011` |
| `BF16-tensor-like` | `compressed (FLR3 + SHA256)` | `26,098` | `0.0249` | `10.9398` | `0.0269` | `10.9667` | `11.7749` |
| `BF16-tensor-like` | `compressed (codec-only, no SHA256)` | `26,098` | `0.0249` | `3.1898` | `0.0269` | `3.2166` (`LZ4@256K: 0.9201`) | **`4.0249`** (`LZ4@256K: 2.1336`) |
| `BF16-tensor-like` | `delta-slice (5% offset SET_TENSOR)` | `52,742` | `0.0503` | `0.0018` | `0.0543` | **`0.0561`** *(best partial)* | **`1.6896`** *(best partial)* |
| `BF16-tensor-like` | `delta-xor (5% dirty + FLR3)` | `8,794` | `0.0084` | `7.1200` | `0.0091` | `7.1290` | `7.4014` |
| `BF16-tensor-like` | `hash-only (v3 SET_TENSOR_HASH)` | `330` | `0.0003` | `0.8732` | `0.0003` | `0.8735` | `0.8838` |
| `BF16-tensor-like` | `reference-only (v4 persistent handle)` | **`0`** | `0.0000` | `0.0002` | `0.0000` | **`0.0002`** *(best warm)* | **`0.0002`** *(best warm)* |
| `BF16-tensor-like` | `recompute (GRAPH_RECOMPUTE 21B)` | `21` | `0.0000` | `0.0350` | `0.0000` | **`0.0350`** | **`0.0357`** |

---

## 4. Clear Recommendations for FASTLINK v4 (`FL4-IMPL`)

| Data Class / Tensor Type | Cheapest Strategy @ `926 MiB/s` LAN | Cheapest Strategy @ `<= 250 Mbps` Link | Concrete v4 Action (`FL4-IMPL`) |
|---|---|---|---|
| **1. `STATIC` Model Weights (`Q4_K_M`, `2.52 GiB`, `incompressible-random`)** | **Cold:** `send-raw` (`1.12 ms/MiB`, skip codec/SHA256 scan via 256-B entropy probe)<br>**Warm:** `reference-only` (`0.0002 ms`, `0` B) or `hash-only` (`330` B) | **Cold:** `send-raw` (`33.6 ms/MiB`)<br>**Warm:** `reference-only` (`0` B) or `hash-only` (`330` B) | 1. Lower `HASH_THRESHOLD` in `ggml-rpc.cpp` from `10 MiB` to **`64 KiB`** so **all** weight tensors (not just `>10 MiB`) hit the remote cache on reload.<br>2. Decouple `rpc_server` weight buffers from TCP socket lifetime so a reconnect re-attaches by handle (`0` B re-sent). |
| **2. `REUSABLE` Compute Graphs & Unchanged State** | **`reference-only` + `GRAPH_RECOMPUTE` (`21` B, `0.035 ms`)** | **`reference-only` + `GRAPH_RECOMPUTE` (`21` B, `0.036 ms`)** | Preserve `stored_graphs` across non-graph buffer frees (only invalidate graphs referencing the freed `remote_ptr`), keeping decode on the `21`-byte `GRAPH_RECOMPUTE` fast path. |
| **3. `APPEND-ONLY` Full-Attention KV Cache (`8` layers: `{3,7,11,15,19,23,27,31}`)** | **In-place remote GPU write (`0` B)**; when host-synced: **`delta-slice` (`offset` `SET_TENSOR`, `0.056 ms` for 5%)** | **In-place remote GPU write (`0` B)**; when host-synced: **`delta-slice` (`1.69 ms` for 5%)** | Never re-send full KV tensors; transfer only newly appended token slices (`1,280 B/layer/token` in `q4_0`) via offset `SET_TENSOR`. |
| **4. Recurrent/SSM State (`24` layers, `2.06 MiB/layer` fixed `f32`)** | **In-place remote GPU update (`0` B/tok)**; on prefix checkpoint restore: **`LZ4` (`64K–256K` chunks, no software SHA-256)** | **In-place remote GPU update (`0` B/tok)**; on checkpoint restore: **`LZ4` or `ZSTD` (`64K–256K` chunks)** | Keep SSM state buffers resident on the remote GPU; snapshot `2.06 MiB/layer` SSM state in remote VRAM (or host cache) at prompt boundaries so prefix cache hits do not require re-evaluating the 24 SSM layers. |
| **5. `DYNAMIC` Zero / Sparse / Mask Tensors (`zero-filled`, `RLE-friendly`)** | **`RPC_CMD_MEMSET_TENSOR` (`322` B) / `recompute` (`0.035 ms`)** or **`RLE` (`64K`, no SHA-256: `0.35–0.52 ms/MiB`)** | **`MEMSET_TENSOR` / `recompute`** or **`RLE` (`64K`, `0.63–1.55 ms/MiB`)** | Prefer `RPC_CMD_MEMSET_TENSOR` for zero/constant initialization; for sparse masks that must cross the wire, use `RLE` at `64K` chunk size without software SHA-256. |
| **6. `DYNAMIC` Boundary Activations (`[2560, n_tokens]`, `BF16` / `F32`)** | **Decode (`10 KiB`):** `send-raw` (`0.011 ms` wire; `<64 KiB` skips pack)<br>**Prefill (`5 MiB`):** `send-raw` (`1.12 ms/MiB`) or `LZ4 @ 256K` (no SHA-256: `0.92 ms/MiB`) | **Decode (`10 KiB`):** `send-raw` or single-chunk `LZ4`<br>**Prefill (`5 MiB`):** `LZ4 @ 64K–256K` (`2.13–2.92 ms/MiB` vs `33.6 ms/MiB` raw) | 1. Keep `n_cuts <= 1` (`-sm layer`) and keep `output.weight` (`lm_head`) on the final device or sample on device so decode returns `10 KiB` (or `4` B token ID) instead of `~608 KiB` logits.<br>2. Replace software `sha256.c` (`2.68 ms/MiB`) in `FLR3` with fast FNV-1a/XXH64 or optional checksum flag (`policy_flags` bit 1) so `LZ4`/`RLE` compression is net-positive even on `926 MiB/s` LAN. |
