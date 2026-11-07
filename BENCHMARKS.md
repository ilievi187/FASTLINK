# Benchmark log

All benchmark numbers must identify the exact build, model, quantization, devices, context, prompt, output limit, sampling, cache state, and run count. Report medians and dispersion, full wire bytes, CPU and device telemetry sources. Never infer v3 speedup from the CPU cost model.

## Existing v2 prototype result (2026-11-05)
See `docs/BENCHMARKS.md`: local CPU file-sync test only, highly compressible 8 MiB synthetic file, payload-only accounting. Not an RPC or inference result.

## Existing separate adapter smoke (documented 2026-11-06)
`FASTLINK-heretic-agent/docs/GPU_RPC_ADAPTER_VERIFIED.md` reports five smoke trials: stock RPC median 0.764 s, adapter 0.758 s. This establishes a compatibility smoke only; it is too small/noisy and is not end-to-end inference evidence. Preserve the distinction between the public FASTLINK repo and the separate patched llama.cpp worktree.

## 2026-11-07 — CPU-only serial placement cost model

- Implementation: `qa/cost_model.py`, branch `feat/fastlink-v3-cpu-baseline`.
- Measures: computed cost from caller-specified RTT, link throughput, serialization, stage compute, queue delay and stage bytes; declared-memory feasibility; modeled single-token rate.
- Test results: `python3 -m unittest discover -s qa -p 'test_cost_model.py' -v` — 12 passed after read-only review identified non-finite inputs, fractional RTT counts, serialization accounting ambiguity, empty pipelines and invalid memory declarations. Regressions were observed failing before fixes.
- Deterministic network fault simulator: `qa/network_sim.py`; 22 tests cover serial transfer formula, invalid inputs, bounded retry, disconnect, multi-stage short-circuit and same-seed reproducibility. All outcomes are modeled arithmetic; no live network was probed.
- CPU-only benchmark harness: `qa/wire_bench.py` runs local FASTLINK server/client through a byte-counting TCP relay, validates SHA256, and records complete application TCP stream bytes (both directions, excludes TCP/IP/Ethernet headers).
- Three 1 MiB loopback runs each: random cold median 130 ms / 1,049,350 stream bytes / 1,048,576 payload bytes; random warm 46 ms / 566 stream bytes / 0 payload; one-byte delta 135 ms / 66,115 stream bytes / 65,536 payload; sparse cold 137 ms / 2,153 stream bytes / 1,548 payload. All output SHA256 matched. Raw machine-readable records: `qa/results/loopback-cpu-1mib-repeats-20261107.json`.
- Combined Python suite: 38/38 pass. Rust checks in same branch: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test --all-targets` — pass, 5 Rust tests.
- These are localhost CPU file-sync metrics only, not LAN, llama.cpp RPC or inference measurements. They do not prove GPU speedup.

## GPU A/B results

Not run. The existing v2 inference service was active at inspection. Do not start competing GPU jobs. A controlled maintenance window is required; v2, stock RPC, and candidate v3 must be tested serially with identical settings and multiple runs before claiming improvement.
