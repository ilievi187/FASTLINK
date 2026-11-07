# FASTLINK v4 baseline benchmark harness (FL4-HARN)

Hardened, deterministic benchmark harness for FL4 baseline configs (A–F) across the **8K / 32K / 64K / 128K** context matrix. CPU-only to inspect/plan; **actual GPU/remote inference is executed by the commander/operator on approved hardware** (no GPU inference is started by the agent during hardening).

## Files

| File | Purpose |
|---|---|
| `qa/v4/bench_client.py` | Deterministic llama-server client (pure stdlib). Builds prompt, validates occupancy, runs one streaming completion, reports per-token decode latency (p50/p95). |
| `qa/v4/baseline_runner.py` | Matrix runner (configs A–F, ≥3 repetitions, median). Captures environment/provenance metadata; starts servers only when explicitly run with infrastructure available. |
| `qa/v4/summary.py` | Reads `baseline_results.json` and prints a table (prefill_tps, decode_tps, ttft, prompt_tokens, output_tokens). No crash on missing configs/fields. |
| `qa/v4/baseline_results.json` | Machine-readable results (merged/updated per run). |

## Configs (A–F)

| Config | Description | Host | Default contexts | Notes |
|---|---|---|---|---|
| A | CPU-only | NightKnightPC (local) | 8K,32K,64K,128K | `--device none -ngl 0` |
| B | Single GPU RX 7900 XT (~3.4 GiB free) | NightKnightPC (local) | 8K,32K | `--device Vulkan0 --fit on --fit-target 0 -ngl 99` |
| C | Single GPU RX 6600 XT (~7 GB free) | detti (remote, Win11 via SSH/tunnel path as configured) | 8K,32K,64K,128K | 128K target uses q4_0 KV per MEMORY_BUDGET |
| D | Hybrid CPU/GPU offload | NightKnightPC (local) | 8K,32K,64K,128K | fit/auto offload |
| E | **Two independent replicas, one per host, measured concurrently** | NightKnightPC + detti | 8K,32K | **INDEPENDENT PARALLEL INFERENCE, NOT distributed/layer-split.** Separate requests to separate replicas. |
| F | Distributed dual-machine inference via FASTLINK v3 RPC (layer split 70/30) | local coordinator + detti RPC worker | 8K,32K,128K | `--rpc 127.0.0.1:15123 -dev Vulkan0,RPC0 -sm layer -ts 0.70,0.30`, `GGML_RPC_FASTLINK_RLE=1` |

Exact server args/KV/layer placement are recorded per run in `baseline_results.json` (see `server_args*`, `kv_cache`, `layer_placement*`).

## Deterministic prompt construction

Implemented in `bench_client.py:build_prompt`:
- Fixed seed text (`SEED_TEXT`, constant) is tokenized via `/tokenize`, repeated deterministically, then truncated via `/detokenize` of the first `prompt_target_tokens` token IDs.
- **Target prompt tokens** = `max(128, target_context_tokens - n_predict)`, leaving room for `n_predict` (default 64) within the slot/context.
- **Deterministic seed** for generation: `seed=42` base (runner uses `42+i` per repetition; recorded). `temperature=0.0`, `cache_prompt=false`, streaming.
- **Matrix validation**: each run records `target_context_tokens`, `prompt_target_tokens`, `prompt_tokens` (actual), `prompt_occupancy_ok` (tolerance ±2%/±4 tokens). Context matrix: **8192, 32768, 65536, 131072**.

## Metrics collected (per config+context)

- **Identity/provenance:** model path, model SHA-256 (computed or known from `docs/v4/MEMORY_BUDGET.md` with provenance), exact server args, llama.cpp/server version text, KV cache type (`--cache-type-k/v`), layer placement (local/remote GPU layers, `-ngl/-dev/-ts/-sm/--rpc` parsed; *actual loaded layers may differ if `--fit` adjusts* — provenance recorded).
- **System state:** system RAM snapshot before/after (`/proc/meminfo`), GPU VRAM snapshot before/after where queryable (**read-only**: `/sys/class/drm/*/mem_info_vram_*`, `rocm-smi --showmeminfo vram --json` best-effort; `unavailable` if not queryable). No GPU workload started by queries.
- **Performance (≥3 repetitions, median reported):** `prefill_tps`, `decode_tps`, `ttft_s`, `total_s`, `prompt_tokens` (actual), `output_tokens`, per-token/inter-token decode latency **p50/p95** (ms) from streaming token timestamps, plus per-run `repetitions[]`.
- **Config F (distributed) network-bytes-per-token (best-effort):** `/proc/net/dev` host delta around benchmark (`bytes_per_generated_token_host_total_delta`), `rpc_call_count` and `flr3_wire_accounting` recorded as **unavailable with provenance** unless log/instrumented adapter counters are wired (see RPC adapter `GGML_RPC_FASTLINK_PACK`/FLR3 docs). **No invented numbers.**
- **Validity:** `prompt_occupancy_ok`, status/error, timestamps.

## Exact reproduction commands

> **Operator/commander runs these on approved hardware.** The agent does not start GPU inference or touch the active 27B service.

### Inspect (CPU-only, safe)
```bash
cd /home/nightknight/Projects/FASTLINK/.worktrees/fastlink-v4
python3 qa/v4/baseline_runner.py --list
python3 qa/v4/baseline_runner.py --config A B C D E F --dry-run   # plan + static metadata, no servers
python3 qa/v4/summary.py                    # reads existing baseline_results.json
python3 qa/v4/summary.py --json             # machine-readable
python3 qa/v4/bench_client.py http://127.0.0.1:8210 8192 64 --seed 42   # single run against an already-running server
```

### Run configs (requires infrastructure; commander)
```bash
# A — CPU-only, NightKnightPC (local model path MODEL)
python3 qa/v4/baseline_runner.py --config A --repetitions 3

# B — RX 7900 XT, NightKnightPC (uses free VRAM; respects active services)
python3 qa/v4/baseline_runner.py --config B --ctx 8192 --repetitions 3

# C — RX 6600 XT, detti (remote; requires SSH detti + fl4_bench.ps1 on detti)
python3 qa/v4/baseline_runner.py --config C --repetitions 3

# D — hybrid offload, NightKnightPC
python3 qa/v4/baseline_runner.py --config D --repetitions 3

# E — two INDEPENDENT replicas concurrently (NOT distributed); operator orchestrates
# NightKnightPC replica (local) + detti replica, then measure concurrently per runbook.
# Harness provides dry-run/config definition; concurrent multi-host execution is operator-driven.
python3 qa/v4/baseline_runner.py --config E --dry-run

# F — distributed FASTLINK v3 RPC layer split (requires detti ggml-rpc-server + tunnel 15123)
python3 qa/v4/baseline_runner.py --config F --repetitions 3
```

### Regenerate `baseline_results.json`
1. Run the desired configs as above (results are merged by `config_context` key). Back up existing JSON if needed.
2. `python3 qa/v4/summary.py qa/v4/baseline_results.json` to view the table.
3. Optional integrity: `python3 qa/v4/baseline_runner.py --config A --dry-run --compute-model-hash` to record computed model SHA-256 (reads the 2.7 GiB model file if present).

## Honesty / limitations
- Proven vs measured vs unavailable is recorded per field (`provenance`, `unavailable`, errors). Missing counters are **not invented**.
- Layer placement is parsed from args; `--fit` may change actual loaded layers — confirm via server logs when benchmarking.
- Config F per-request RPC call count / FLR3 wire metadata requires instrumented adapter or log parsing not wired in this harness; best-effort host network delta is reported with caveats.
- Config E concurrent multi-host execution is operator-driven; harness defines/labels it and supports planning/dry-run.
