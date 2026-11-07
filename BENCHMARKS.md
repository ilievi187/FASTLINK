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
- This is a deterministic arithmetic model only. No network, GPU, model, actual telemetry or performance benchmark was run.
- Rust checks in same branch: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test --all-targets` — pass, 5 Rust tests.

## GPU A/B results

Not run. The existing v2 inference service was active at inspection. Do not start competing GPU jobs. A controlled maintenance window is required; v2, stock RPC, and candidate v3 must be tested serially with identical settings and multiple runs before claiming improvement.
