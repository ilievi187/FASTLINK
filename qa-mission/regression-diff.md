# FL3-E3 cross-branch regression diff

Compared `cargo test --all-targets` on main and the three feature-branch checkouts on 2026-11-07. Exact same Rust test function names were matched across all outputs.

- Main: 5 passed, 0 failed. Names: `tests::checksums_change_when_content_changes`, `tests::chunk_count_boundaries`, `tests::reject_rle_bomb`, `tests::rle_sparse_roundtrip`, `tests::incompressible_roundtrip`.
- `feat/fastlink-v3-cpu-baseline`: same five tests; 5 passed, 0 failed.
- `feat/fastlink-v3-network-sim`: same five tests; 5 passed, 0 failed.
- `agent/heretic-m1-hardening`: those five plus additional tests (33 library + 14 protocol integration); 47 passed, 0 failed.

Observed regression differences: no test passing on main failed on any feature branch; no test passing on a feature branch failed on main. Additional feature-branch-only test names are not regressions in either direction; they were not present in main's test suite, so cross-version behavior for those cases is not comparable from this run.

Method limitation: this is suite/test-name and pass/fail comparison, not a shared test binary run against each branch. Each checkout ran its own `cargo test --all-targets`.
