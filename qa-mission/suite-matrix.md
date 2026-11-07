# FL3-E1 suite matrix

Run 1 was executed on 2026-11-07 at approximately 19:18 +02:00. CPU-only; no GPU or SSH.

Exact command sequence in each checkout: `cargo fmt --check`; `cargo clippy --all-targets -- -D warnings`; `cargo test --all-targets`.

| Checkout | fmt | clippy | test pass/fail/ignored | Test command result |
|---|---|---|---:|---|
| `/home/nightknight/Projects/FASTLINK` | PASS | PASS | 5/0/0 | PASS |
| `/home/nightknight/Projects/FASTLINK/.worktrees/fastlink-v3-cpu-baseline` | PASS | PASS | 5/0/0 | PASS |
| `/home/nightknight/Projects/FASTLINK/.worktrees/fastlink-v3-network-sim` | PASS | PASS | 5/0/0 | PASS |
| `/home/nightknight/Projects/FASTLINK-heretic-agent` | PASS | PASS | 47/0/0 | PASS |

Run 2: due approximately 2026-11-07 19:38 +02:00; not yet run. No adapter build test was run: task specified four locations and the checked-out projects exposed Cargo suites.
