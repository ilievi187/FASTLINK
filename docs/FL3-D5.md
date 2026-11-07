# FL3-D5 — memory budget, placement cost, chunk framing

CPU-only work on `agent/heretic-m1-hardening`. No GPU, no SSH, no change to wire format v0.1.

## QA run 2 symbol

The mission board records that QA run 2 (2026-11-07 19:40 +02:00) failed clippy and test compilation in this worktree because `recover_atomic_writes` was unresolved at `src/lib.rs:660`.

`git grep` on `cdfc5f3^` (`1b31e5a`) finds no `recover_atomic_writes`. Commit `cdfc5f3` adds the function and calls it from `server_sync` and `fastlink serve`. This session's `cargo test --all-targets` compiled that call. The symbol resolves at `cdfc5f3` and at the FL3-D5 commit on top of it.

## Memory-budget cache

`MemoryBudgetCache` (`src/cache_budget.rs`) is an in-memory content-addressed set of verified chunks.

- The budget is the sum of resident payload lengths. Map nodes and allocator overhead are outside that sum, so resident RSS can sit above the budget.
- Insert refuses an empty payload, a payload larger than 64 KiB, a payload larger than the budget, and bytes whose BLAKE3 digest is not the address.
- A refused insert leaves the cache as it was.
- Eviction is FIFO by a monotonic admission sequence. The smallest sequence is dropped until the new payload fits. The same insert sequence produces the same evictions.
- `get` and a repeat insert of the same verified bytes do not refresh the sequence.
- The cache does not delete files under `chunks/` or `outputs/`. The on-disk content-addressed store still has no quota.

Unit tests cover FIFO order, repeatability, admission stickiness, rejected inserts, and a zero budget. `tests/d5_cache_schedule.rs` transfers two 64 KiB chunks over `127.0.0.1`, evicts the older payload from a one-chunk memory budget, and checks that the committed output file is unchanged.

## Cost-based scheduler

`src/scheduler.rs` is a direct port of the declared WP-C formulas read from `feat/fastlink-v3-network-sim` at `e9c96ad`:

- `qa/sim_common.py` devices, links, and the 27B workload
- `qa/sim_placement.py` layer split, rank, and cold/prefill/decode costs
- `qa/cost_model.py` `placement_is_feasible`
- `qa/network_sim.py` `transfer` with loss 0 and retry budget 0 (queue charged once, then RTT + serialization + wire per round trip)

The Rust code does not import or execute that worktree. Constants are copied. Link throughput is the declared 4-decimal value (`95.3674` MiB/s remote, `238.4186` MiB/s local), which the unit test recomputes as `round(line_rate_bits / 8 / 2^20 * 0.8, 4)`.

Ranking matches the reference: feasible splits first, then higher modelled decode tokens/s, then lower modelled prefill latency, then lower local layer count. If both a local site and a remote site fit and their modelled latencies are equal, the site stays local. A strictly cheaper feasible remote site is selected. A site that misses its declared VRAM budget is not selected.

### Modelled reference row

These figures are model arithmetic checked against `sim_placement.compute_row`, not measurements. Workload: 27B-class Q4 weights, 131072-token context, q4 KV, `LOCAL_DEDICATED` (budget 19 GiB) and `REMOTE_DEVICE` (budget 6 GiB), `LINK_REMOTE`.

| Field | Value |
| --- | --- |
| Feasible `k` | 48, 49, 50, 51, 52, 53 |
| Selected split | 53 local / 11 remote |
| Weights local / remote | 12.233977031428367 / 2.539127308409661 GiB |
| KV local / remote | 6.625 / 1.375 GiB |
| Cold remote load | 27.264930920435 s |
| Prefill | 103955.94146960336 ms (39.401307343242784 tok/s) |
| Decode step | 27.87488155123564 ms (35.87459190317786 tok/s) |
| Boundary overhead | 2.663840054358198 ms/token |

The all-local split `k = 64` is infeasible on this budget and has zero modelled boundary and zero modelled cold-load time. Its modelled decode rate is 54.15224020670045 tok/s, and the rank still selects `k = 53` because that split fits. The 131072-token fp16 matrix on `LOCAL_MIXED` has no feasible split. The `k = 32` row's rounded deficit is 17.3866 GiB, matching the Python row.

Calling `select_placement` twice returns the same `k` and the same `decode_step_ms` bit pattern.

## Per-chunk overhead

A v0.1 CHUNK frame is `index:u32` + `codec:u8` + `encoded_length:u32` + payload: 9 bytes plus the payload. On a full 65536-byte RAW chunk that header is `9/65536` of the payload, which is 0.01373291015625%. Dropping the index and keeping codec and length would remove 4 bytes, 0.006103515625% of that payload. The wire layout is unchanged.

### Measured write loop

Harness: `qa/measure_chunk_frame.rs`, compiled with `rustc -O` on this host and run twice. Each sample is 128 frames of 65545 bytes (8,389,760 bytes) to `127.0.0.1`, `TCP_NODELAY` left unset, five trials, median reported. The harness does not hash, touch the disk cache, or load a GPU. MiB/s below is `8389760 / median_seconds / 2^20`, derived from that median.

| Run | Split writes (kind 0) | Staged copy (kind 1) | `write_vectored` (kind 2) |
| --- | --- | --- | --- |
| 1 | 2,105,558 ns (3800.0 MiB/s) | 1,785,942 ns (4480.0 MiB/s) | 1,449,996 ns (5518.0 MiB/s) |
| 2 | 2,001,842 ns (3996.9 MiB/s) | 1,603,838 ns (4988.7 MiB/s) | 1,507,545 ns (5307.4 MiB/s) |

Kind 2 was the lowest median in both runs (68.9% and 75.3% of the kind 0 median). The client now submits each CHUNK frame with `write_vectored`. A unit test checks the bytes against the four-field layout, including a writer that accepts one byte per call. Existing loopback protocol tests still pass, so the server reads the same framing.

The same harness also timed a second `Vec` clone of a 64 KiB buffer against a move (2000 iterations, five-trial median). Run 1: 894,203 ns with the extra clone, 908,942 ns with the move. Run 2: 1,260,217 ns and 1,124,379 ns. The two runs do not agree, and `read_chunk` still decodes RAW by copying into the returned buffer.

This microbench is not an end-to-end FASTLINK cold/warm measurement. Full-transfer MiB/s was not re-recorded for this commit.

## Gates

Commands, from this worktree, after the change:

- `cargo fmt --check`
- `cargo clippy --all-targets -- -D warnings`
- `cargo test --all-targets --offline`

Result: fmt clean, clippy clean, 57 library tests, 0 binary tests, 17 integration tests (15 protocol + 2 D5), 0 failed. No GPU and no remote host.
