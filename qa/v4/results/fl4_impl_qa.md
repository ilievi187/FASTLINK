# FL4-IMPL QA (OpenCode) — network efficiency + correctness/reliability/regression

- **Timestamp:** 2026-11-08T04:07:32+0200
- **Env:** CPU-only, loopback-only, **no GPU, no model, no push**.
- **v4 worktree:** `/home/nightknight/Projects/FASTLINK/.worktrees/fastlink-v4` @ `a5b25bd` (feat/fastlink-v4)
- **v3 integrate:** `/home/nightknight/Projects/FASTLINK/.worktrees/fastlink-v3-integrate` @ `ff9f0ec`
- **src v4 vs v3 integrate identical:** **True**

## 1) Network efficiency (measured, protocol-level; 4 MiB deterministic, loopback)

> **Honest status:** v4 *persistent remote tensor cache + adaptive incremental sync* is **NOT implemented** in `feat/fastlink-v4` src (identical to v3 integrate). Therefore **bytes/token and RPC/token reduction (v4 residency vs v3 always-send) cannot be measured as a delta**. No numbers invented. Below: **always-send baseline measured** on v4 build and v3 integrate build; v4 residency reduction = **N/A (implementation absent)**. "Per generated token" not measured (no LLM inference/model).

### v4 build (always-send session model, chunk-cache behavior observed)
| phase | wire_bytes_total | wire_tx | wire_rx | chunks total/reused/transferred | cache hit rate | RPC calls proxy/sync* | sync events | manifest ms | encode/transfer ms | decode ms | byte_exact |
|---|---:|---:|---:|---|---:|---:|---:|---:|---:|---|---:|
| cold (first sync) | 2385967 | 2385700 | 267 | 64/0/64 | 0.0 | 67 | 1 | 83.064 | 36.712 | n/a (CLI) | True |
| warm resend (same content) | 2093 | 2082 | 11 | 64/64/0 | 1.0 | 3 | 1 | 43.363 | 2.977 | n/a (CLI) | True |
| delta (1% mutated) | 67641 | 67626 | 15 | 64/63/1 | 0.984375 | 4 | 1 | 82.209 | 2.997 | n/a (CLI) | True |

\*RPC calls proxy = protocol-level `1(manifest)+1(need)+chunks_transferred+1(status)` per sync op. **Not LLM inference RPC/token calls.** Decode CPU ms not emitted by CLI (reported n/a with provenance).

### v4 persistent residency vs v3 always-send (requested comparison)
| metric | v3 always-send (measured, current build) | v4 persistent residency (measured) | reduction |
|---|---:|---:|---:|
| bytes/sync (cold, 4 MiB logical) | 2385967 | **N/A** | **N/A — residency not implemented** |
| RPC/sync proxy (cold) | 67 | **N/A** | **N/A** |
| bytes/token | **N/A (no inference)** | **N/A (no inference/impl)** | **N/A** |
| RPC/token | **N/A (no inference)** | **N/A** | **N/A** |
| warm resend cache hit rate | 1.0 (chunk cache; 0 chunks transferred, manifest/need still exchanged) | **N/A** | **N/A** |

Modelled projection (clearly labeled, not measured): Persistent residency (FL4-IMPL core) absent from current src; reduction % cannot be measured. Warm resend demonstrates chunk-cache hits (0 chunks transferred, wire=manifest/need overhead) under existing always-send session model.

## 2) Correctness
- **Byte-exact deterministic outputs (v4 vs v3 integrate, same input):** PASS (src_sha=5a9bc8b3c1150c0e…, v4==v3 output, match src)
- **Cache invalidation (corrupt cached chunk -> recover/resync; mutated content -> resync, no stale reuse):** PASS — {"after_corruption": {"output_byte_exact": true, "retransferred_or_recovered": true, "send_ok": true, "silent_corruption": false}, "corrupted_cached_chunk": true, "mutated_content_resync": {"chunks_transferred": 1, "output_byte_exact": true, "send_ok": true}, "note": "byte-exact or clean-fail required; no stale reuse/silent corruption", "pass": true}
- **Worker restart/reconnect (mid-transfer kill, restart, resume; no partial):** PASS — mid_kill=True, byte_exact_after_resume=True, chunks_transferred_on_resume=16

## 3) Concurrency (4 sessions) + memory stability
- **4-concurrent sessions (unique, byte-exact, 0 corruption):** PASS — all_byte_exact=True, corruption_detected=False
- **Memory stability (RSS bounded, 80 transfers, 512 KiB files):** PASS — min/max/delta=3692/4044/352 kB, bound=65536 kB, bounded=True

## 4) Regression vs v3 (v4 build)
- **cargo test --all-targets (v4 worktree):** PASS (rc=0)
- **e4_harness.py:** PASS (rc=0)
- **e5_harness.py:** PASS (rc=0)
- **e6_harness.py:** PASS (rc=0)
- **sim_run.sh:** PASS

## Summary (pass/fail)
```json
{
  "determinism": true,
  "cache_invalidation": true,
  "restart_reconnect": true,
  "concurrent4": true,
  "mem_stability": true,
  "cargo_test": true,
  "e4": true,
  "e5": true,
  "e6": true
}
```
**all_core_pass = True**

## Honesty / limitations
- CPU-only, loopback-only. **No GPU, no model inference.** “Per generated token” and LLM RPC/token counts **not measured** (proxies/n-a labeled).
- **v4 persistent-residency not implemented in current src** (identical to v3 integrate) → residency-vs-always-send **reduction % not measurable**. No invented numbers.
- Decode CPU ms not emitted by integrated CLI; encode/transfer/manifest timing reported where available.
