# FL3-E1 suite matrix

Run 1 was executed on 2026-11-07 at approximately 19:18 +02:00. CPU-only; no GPU or SSH. Checkout labels are stable across reports; filesystem paths are omitted.

Exact command sequence in each checkout: `cargo fmt --check`; `cargo clippy --all-targets -- -D warnings`; `cargo test --all-targets`.

| Checkout | fmt | clippy | test pass/fail/ignored | Test command result |
|---|---|---|---:|---|
| `<CHECKOUT_05>` | PASS | PASS | 5/0/0 | PASS |
| `<CHECKOUT_09>` | PASS | PASS | 5/0/0 | PASS |
| `<CHECKOUT_10>` | PASS | PASS | 5/0/0 | PASS |
| `<CHECKOUT_06>` | PASS | PASS | 47/0/0 | PASS |

Run 2 was executed on 2026-11-07 at 19:40 +02:00. Exact command sequence per checkout: `cargo fmt --check`; `cargo clippy --all-targets -- -D warnings`; `cargo test --all-targets`. The commands were invoked together with `&&`; test-only retries are separately noted below and do not replace the sequence result.

| Checkout | fmt | clippy | test pass/fail/ignored | Full sequence result |
|---|---|---|---:|---|
| `<CHECKOUT_05>` | PASS | PASS | 5/0/0 | PASS |
| `<CHECKOUT_09>` | FAIL (rustfmt diff in `src/codec.rs`, `src/lib.rs`, `tests/codec_adaptive.rs`) | not run (short-circuited) | 8/0/0 on separate test-only retry | FAIL (fmt prevents combined sequence) |
| `<CHECKOUT_10>` | PASS | PASS | 5/0/0 | PASS |
| `<CHECKOUT_06>` | PASS | FAIL (`recover_atomic_writes` not found at `src/lib.rs:660`) | test-only retry also failed with same compile error; no test count | FAIL |

No source files in other worktrees were edited. Test-only retry in `<CHECKOUT_09>`: 12 passed total (8 library + 1 binary + 3 codec integration), 0 failed, 0 ignored. Retry in `<CHECKOUT_06>` failed compilation before tests ran.

No adapter build test was run: task specified four locations and the checked-out projects exposed Cargo suites.

Run 3 (FL3-E4 Step 3) was executed on 2026-11-07 at 21:31 +02:00 by Antigravity. CPU-only, loopback-only; no GPU, no SSH. Exact command sequence per Rust checkout: `cargo fmt --check`; `cargo clippy --all-targets -- -D warnings`; `cargo test --all-targets`. Network-sim additionally passed `cd qa && python3 -m unittest test_sim_placement test_sim_pipeline test_sim_faults` (57/0, 21+17+19).

| Checkout | SHA | fmt | clippy | test pass/fail/ignored | Full sequence result |
|---|---|---|---|---:|---|
| `<CHECKOUT_05>` | ccec225 | PASS | PASS | 5/0/0 (lib) + 0/0/0 (bin) | PASS |
| `<CHECKOUT_09>` | 5b41ff1 | PASS | PASS | 12/0/0 total (8 lib + 0 bin + 1 wire + 3 codec) | PASS |
| `<CHECKOUT_10>` | a50ecd8 | PASS | PASS | 5/0/0 Rust + 57/0 Python sim | PASS |
| `<CHECKOUT_06>` | c3a89b7 | PASS | PASS | 15/0/0 integration (captured tail; full `cargo test --all-targets` exit 0) | PASS |

Adapter `<CHECKOUT_01>`: CMake (<MODEL_ID> fork), no Cargo suite — not run (CPU-only QA scope). Detailed capture: `qa-mission/e4-run3-suite-2026-11-07T2131+02.txt`. No source files in other worktrees edited.

Run 4 (FL3-E5 Step 3, post A4 deac1d0) was executed on 2026-11-07 at 21:46 +02:00 by Antigravity. CPU-only, loopback-only; no GPU, no SSH. Exact sequence: `cargo fmt --check`; `cargo clippy --all-targets -- -D warnings`; `cargo test --all-targets`. Network-sim also Python sim 57/0.

| Checkout | SHA | fmt | clippy | test pass/fail/ignored (observed) | Full sequence result |
|---|---|---|---|---:|---|
| `<CHECKOUT_05>` | 8aeed68 | PASS | PASS | exit 0 (full `cargo test --all-targets`) | PASS |
| `<CHECKOUT_09>` | deac1d0 | PASS | PASS | exit 0; observed codec_adaptive 3/3, concurrency_isolation 2/2, resume_and_backpressure 3/3 | PASS |
| `<CHECKOUT_10>` | a50ecd8 | PASS | PASS | Rust exit 0 + Python sim 57/0 | PASS |
| `<CHECKOUT_06>` | c3a89b7 | PASS | PASS | integration tail 15/0/0; exit 0 | PASS |

Adapter `<CHECKOUT_01>` (CMake): not run. Detail: `qa-mission/e5-run4-suite-2026-11-07T2146+02.txt`. No source files in other worktrees edited.
