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
