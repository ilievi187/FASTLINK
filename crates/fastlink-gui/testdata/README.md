# Device inventory fixtures

`devices-v1.json` has the exact field set and compact JSON layout emitted by the
integrated `fastlink devices --json` command (`fastlink.devices.v1`). It is used
by parser, state and headless UI tests.

`discovery-v1.json` is a sanitized `fastlink.discover.v1` fixture for the
explicit opt-in discovery UI. Provider messages, provider names, candidate names
and node IDs are synthetic. No real LAN/Tailscale data, addresses or provider
error output is committed or used by tests.

All values are synthetic. Real hostnames, machine IDs, node hashes, kernel
releases and GPU identifiers are never committed. The live-binary contract check
in `tests/command_runner.rs` runs the actual CLI from `FASTLINK_BIN` without
recording its output; discovery tests use fake CLI scripts only.
