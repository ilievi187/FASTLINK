# FL3-E1 suite matrix

Run 1 was executed on 2026-11-07 at approximately 19:18 +02:00. CPU-only; no GPU or SSH.

Exact command sequence in each checkout: `cargo fmt --check`; `cargo clippy --all-targets -- -D warnings`; `cargo test --all-targets`.

| Checkout | fmt | clippy | test pass/fail/ignored | Test command result |
|---|---|---|---:|---|
| `/home/nightknight/Projects/FASTLINK` | PASS | PASS | 5/0/0 | PASS |
| `/home/nightknight/Projects/FASTLINK/.worktrees/fastlink-v3-cpu-baseline` | PASS | PASS | 5/0/0 | PASS |
| `/home/nightknight/Projects/FASTLINK/.worktrees/fastlink-v3-network-sim` | PASS | PASS | 5/0/0 | PASS |
| `/home/nightknight/Projects/FASTLINK-heretic-agent` | PASS | PASS | 47/0/0 | PASS |

Run 2 was executed on 2026-11-07 at 19:40 +02:00. Exact command sequence per checkout: `cargo fmt --check`; `cargo clippy --all-targets -- -D warnings`; `cargo test --all-targets`. The commands were invoked together with `&&`; test-only retries are separately noted below and do not replace the sequence result.

| Checkout | fmt | clippy | test pass/fail/ignored | Full sequence result |
|---|---|---|---:|---|
| `/home/nightknight/Projects/FASTLINK` | PASS | PASS | 5/0/0 | PASS |
| `/home/nightknight/Projects/FASTLINK/.worktrees/fastlink-v3-cpu-baseline` | FAIL (rustfmt diff in `src/codec.rs`, `src/lib.rs`, `tests/codec_adaptive.rs`) | not run (short-circuited) | 8/0/0 on separate test-only retry | FAIL (fmt prevents combined sequence) |
| `/home/nightknight/Projects/FASTLINK/.worktrees/fastlink-v3-network-sim` | PASS | PASS | 5/0/0 | PASS |
| `/home/nightknight/Projects/FASTLINK-heretic-agent` | PASS | FAIL (`recover_atomic_writes` not found at `src/lib.rs:660`) | test-only retry also failed with same compile error; no test count | FAIL |

No source files in other worktrees were edited. The cpu-baseline test-only retry command was `cargo test --all-targets`: 12 passed total (8 library + 1 binary + 3 codec integration), 0 failed, 0 ignored. The heretic-agent test-only retry was `cargo test --all-targets`, which failed compilation before tests ran.

No adapter build test was run: task specified four locations and the checked-out projects exposed Cargo suites.

Run 3 (FL3-E4 Step 3) was executed on 2026-11-07 at 21:31 +02:00 by Antigravity. CPU-only, loopback-only; no GPU, no SSH. Exact command sequence per Rust checkout: `cargo fmt --check`; `cargo clippy --all-targets -- -D warnings`; `cargo test --all-targets`. Network-sim additionally passed `cd qa && python3 -m unittest test_sim_placement test_sim_pipeline test_sim_faults` (57/0, 21+17+19).

| Checkout | SHA | fmt | clippy | test pass/fail/ignored | Full sequence result |
|---|---|---|---|---:|---|
| `/home/nightknight/Projects/FASTLINK` | ccec225 | PASS | PASS | 5/0/0 (lib) + 0/0/0 (bin) | PASS |
| `/home/nightknight/Projects/FASTLINK/.worktrees/fastlink-v3-cpu-baseline` | 5b41ff1 | PASS | PASS | 12/0/0 total (8 lib + 0 bin + 1 wire + 3 codec) | PASS |
| `/home/nightknight/Projects/FASTLINK/.worktrees/fastlink-v3-network-sim` | a50ecd8 | PASS | PASS | 5/0/0 Rust + 57/0 Python sim | PASS |
| `/home/nightknight/Projects/FASTLINK-heretic-agent` | c3a89b7 | PASS | PASS | 15/0/0 integration (captured tail; full `cargo test --all-targets` exit 0) | PASS |

Adapter `/mnt/gamez/AI/llama.cpp-fastlink-rpc-adapter`: CMake (llama.cpp fork), no Cargo suite — not run (CPU-only QA scope). Detailed capture: `qa-mission/e4-run3-suite-2026-11-07T2131+02.txt`. No source files in other worktrees edited.

Run 4 (FL3-E5 Step 3, post A4 deac1d0) was executed on 2026-11-07 at 21:46 +02:00 by Antigravity. CPU-only, loopback-only; no GPU, no SSH. Exact sequence: `cargo fmt --check`; `cargo clippy --all-targets -- -D warnings`; `cargo test --all-targets`. Network-sim also Python sim 57/0.

| Checkout | SHA | fmt | clippy | test pass/fail/ignored (observed) | Full sequence result |
|---|---|---|---|---:|---|
| `/home/nightknight/Projects/FASTLINK` | 8aeed68 | PASS | PASS | exit 0 (full `cargo test --all-targets`) | PASS |
| `/home/nightknight/Projects/FASTLINK/.worktrees/fastlink-v3-cpu-baseline` | deac1d0 | PASS | PASS | exit 0; observed codec_adaptive 3/3, concurrency_isolation 2/2, resume_and_backpressure 3/3 | PASS |
| `/home/nightknight/Projects/FASTLINK/.worktrees/fastlink-v3-network-sim` | a50ecd8 | PASS | PASS | Rust exit 0 + Python sim 57/0 | PASS |
| `/home/nightknight/Projects/FASTLINK-heretic-agent` | c3a89b7 | PASS | PASS | integration tail 15/0/0; exit 0 | PASS |

Adapter `/mnt/gamez/AI/llama.cpp-fastlink-rpc-adapter` (CMake): not run. Detail: `qa-mission/e5-run4-suite-2026-11-07T2146+02.txt`. No source files in other worktrees edited.
