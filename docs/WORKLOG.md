# FastLink v3 worklog

## 2026-11-07 continuation — live discovery and first implementation
- Live Herdr inventory performed first: w2 agents are Hermes, Codex, Grok, Antigravity, Hermes Heretic and OpenCode. Workspace w4 is on unrelated `videoediting` task; preserved untouched. OpenCode screen exposed GLM-5.3-Flash Free; Codex showed GPT-6-Luna medium and weekly limit warning; Heretic pane is local profile. These are runtime/UI observations, not complete permission audits.
- Created isolated CPU baseline branch/worktree `feat/fastlink-v3-cpu-baseline`; no production/main/source branch edits.
- Implemented test-driven explicit serial link/stage cost model (`qa/cost_model.py`) and 12 tests. OpenCode reviewed read-only and found NaN, fractional RTT, serialization accounting, empty-stage and VRAM input edge cases; tests were made to fail, then fixes applied. 12/12 pass.
- `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test --all-targets` pass (5 Rust tests).
- Commits: `28a4771` model+docs, `0c4ecc3` Python cache ignore, `fb73c4f` task gates.
- OpenCode implemented the CPU-only network/retry simulator on isolated `feat/fastlink-v3-network-sim` (only `qa/network_sim.py`, `qa/test_network_sim.py`); 22/22 passed. Commit `f786cbc` integrated into baseline branch with `a578ecd` after scope and tests verified.
- Combined Python tests: 34/34 passed; Rust fmt, clippy, tests passed (5 Rust tests).
- No GPU benchmark launched; PID 81209 remains active and untouched. No competing inference or duplicate research was started. GPU comparisons wait until the existing service is idle or a maintenance window is available.
