# Heterogeneous profile fixtures

Version 1 JSON fixtures in `fixtures/` are deterministic synthetic test inputs, not
measurements or recommendations. Validate with `python3 validate_profiles.py`
and run tests with `python3 -m unittest discover -s tests -v`.

Each fixture declares its node count, explicit opt-in, IDs, link edges, objective,
provenance and evidence labels. Unknown telemetry deterministically falls back
to local-only. The 70B-class fixture models an idealized FP16/BF16 weight payload only:
70,000,000,000 parameters × 2 bytes = 140,000,000,000 bytes = 140 decimal GB
= approximately 130.385 GiB. This is arithmetic-only, not a fit claim; it
excludes quantization metadata, context/KV/SSM, runtime workspace, and reserve.
The N=7 fixture includes a phone excluded for slow Wi-Fi. Never interpret these
profiles as hardware benchmark results.
