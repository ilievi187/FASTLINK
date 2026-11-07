# FL3-E7 — Reliability harnesses vs integrated build (OpenCode)

**Date:** 2026-11-07  
**Integrated worktree:** `/home/nightknight/Projects/FASTLINK/.worktrees/fastlink-v3-integrate` (`integrate/fastlink-v3-m1`) at **`2bd9698`** (FL3-B3 merge).  
**Harness source:** `e4_harness.py`, `e5_harness.py`, `e6_harness.py` from `/home/nightknight/Projects/FASTLINK/.worktrees/fastlink-v3-network-sim/qa/` (tweaked with `--target-worktree`, committed on `feat/fastlink-v3-network-sim` as `1c945e3`/`a52f5ac` series). Harnesses executed **against the integrated build** via `--target-worktree .../fastlink-v3-integrate`.  
**Environment:** CPU-only, loopback (127.0.0.1), **no GPU, no SSH**, deterministic/fixed seeds & sizes. No invented numbers.

## Exact commands
```bash
# from /home/nightknight/Projects/FASTLINK/.worktrees/fastlink-v3-network-sim
python3 qa/e4_harness.py --target-worktree /home/nightknight/Projects/FASTLINK/.worktrees/fastlink-v3-integrate --results /tmp/e7_e4.json
python3 qa/e6_harness.py --target-worktree /home/nightknight/Projects/FASTLINK/.worktrees/fastlink-v3-integrate --results /tmp/e7_e6.json
python3 qa/e5_harness.py --target-worktree /home/nightknight/Projects/FASTLINK/.worktrees/fastlink-v3-integrate --results /tmp/e7_e5.json
```

## Results (all scenarios byte-exact OR clean-fail; no silent corruption)

### e4 — benchmark + kill/resume (8 MiB, seed 42, release)
| check | result |
|---|---|
| cold byte_exact | **true** (chunks_transferred 128) |
| warm byte_exact / chunks_transferred==0 | **true / true** (0) |
| delta byte_exact | **true** (chunks_transferred 2) |
| kill/resume mid-kill achieved + byte_exact after resume | **true / true** (resume-by-manifest) |
| **all_required_pass** | **true** |

### e5 — sustained load + RSS + concurrent probe (seed 0xE5E5)
| check | result |
|---|---|
| sustained 50×1MiB cold, all byte_exact | **true** |
| sustained 50×1MiB warm, all byte_exact, chunks_transferred==0 | **true / true** |
| server VmRSS delta (kB, bound 32768) / bounded | **736 / true** (no unbounded growth) |
| sustained all_pass | **true** |
| 4-session concurrent stress probe (cpu-baseline read-only, informational) | **all_ok=true, all_byte_exact=true, all_pass=true** |
| **summary** | **sustained pass + concurrent pass** |

### e6 — 4 fault scenarios (seed 0xE6E6)
| scenario | verdict | byte-exact/clean-fail | silent corruption |
|---|---|---|---:|
| (a) digest corruption after successful transfer | `byte_exact_retransfer` | byte-exact | false |
| (b) server restart mid-session (kill K, restart, retry) | `byte_exact_resume` | byte-exact | false |
| (c) duplicate NEED index injection (raw protocol) | `clean_fail_order_enforced` | **clean-fail** (strict `index==expected_index`; no output, cache integrity ok) | false |
| (d) client disconnect during RESUME | `no_partial_then_byte_exact_resume` | byte-exact after resume | false |

**e6 summary:** `all_no_silent_corruption=true`, `all_ok_status=true`, `all_pass=true`.

## Verdict
**PASS** — e4, e5, e6 all passed against integrated `2bd9698` (`integrate/fastlink-v3-m1`). All required scenarios ended **byte-exact or clean-fail; no silent corruption**. This proves the merged/integrated code passes the full reliability suite exercised by these harnesses (CPU-only, loopback).

*Provenance note:* running the harnesses overwrote `qa/results/e{4,5,6}_latest*` in the integration worktree and produced a `_e6_b3` helper; those were restored/removed before committing only this `e7_integrated_reliability.md` result file. Raw E7 captures: `/tmp/e7_e4.json`, `/tmp/e7_e5.json`, `/tmp/e7_e6.json` (ephemeral).
