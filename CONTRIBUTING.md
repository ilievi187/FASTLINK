# Contributing

FASTLINK welcomes contributions to the independent, FOSS implementation.

1. Open an issue for a large protocol or API change before implementation.
2. Keep changes small and reviewable.
3. Run `cargo fmt --check`, `cargo test` and `cargo clippy --all-targets -- -D warnings`.
4. Include correctness tests and end-to-end performance data with performance claims.
5. Do not add proprietary implementations, unlicensed code, credentials, personal machine details, model weights, or recorded private traffic.
6. Do not connect unauthenticated raw RPC endpoints to the public internet.

All original contributions are accepted under Apache-2.0. See `SECURITY.md` for the current threat model.

## Reporting a reproducible bug

Before opening an issue, check for an existing report. A useful report includes:

- **Version and environment:** the FASTLINK commit or release, operating system, architecture, and output of `rustc --version` and `cargo --version`.
- **Reproduction:** the exact sanitized command(s), a minimal non-sensitive input, and ordered steps that consistently trigger the problem.
- **Expected vs. actual:** what should happen, what happened instead, and whether the problem reproduces on a fresh checkout.
- **Evidence:** trimmed logs, an exit status or a test failure, with secrets and identifying information removed. Do not post raw internal addresses, hostnames, tokens, model files, private traffic, or machine-specific configurations.

For security-sensitive findings, follow `SECURITY.md` instead of publishing exploit details in a public issue. Clearly distinguish a failed test from an untested platform or feature; FASTLINK's GPU and distributed inference roadmap items are not yet advertised as working functionality.

## Checks before a pull request

From the repository root, run:

```bash
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
```

Document any platform-specific checks you could not run. If a change includes performance claims, supply the workload, environment, cold/warm cache conditions, and end-to-end measurements; avoid comparing against an unrelated baseline.
