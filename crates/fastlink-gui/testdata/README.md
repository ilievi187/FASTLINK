# Device inventory fixtures

`devices-v1.json` has the exact field set and compact JSON layout emitted by the
integrated `fastlink devices --json` command (`fastlink.devices.v1`). It is used
by parser, state and headless UI tests.

All values are synthetic. Real hostnames, machine IDs, node hashes, kernel
releases and GPU identifiers are never committed. The live-binary contract check
in `tests/command_runner.rs` runs the actual CLI from `FASTLINK_BIN` without
recording its output.
