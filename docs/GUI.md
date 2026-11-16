# FASTLINK desktop shell (Linux, pre-alpha)

This document describes the first real Linux desktop shell for FASTLINK. Its
milestone scope is deliberately narrow: **local device overview and a
session-only, non-operational visual-intent marker**. It is a native desktop binary (`fastlink-gui`) built with
Rust, [egui](https://github.com/emilk/egui) and eframe.

**Status: pre-alpha.** The shell is aligned with the implemented
`fastlink devices --json` backend (`fastlink.devices.v1`).

## What it does

- Invokes exactly one command: `<FASTLINK CLI> devices --json`.
- Parses the implemented `fastlink.devices.v1` local-only inventory.
- Treats the sole node in `scope:"local_only"` as the local device.
- Preserves the backend's opaque hashed `node_id`; it never expects a literal
  `"local"` id and never rewrites the hash.
- Shows hostname, OS, OS release, architecture, logical CPU count, total /
  available / free RAM, GPU metadata, capabilities and the CLI's explicit
  `unknown_fields` list.
- Labels missing or `null` values as `Unknown`. It never estimates, probes the
  OS itself, or fills gaps with plausible-looking numbers.
- Renders capabilities only from reported fields. `capacity_bytes`,
  `throughput_bps` and `network_connectivity` stay `Unknown` when the backend
  reports `null` or the documented `"unknown"` sentinel. Network speed and
  connectivity are never inferred.
- Provides manual **Refresh**, local-node **selection**, and a session-only
  **visual-intent marker** for the local node only. The marker has **no
  operational effect**: it does not enable compute, pair devices, open
  connections, or send data.
- Shows honest empty states for peers, models and chat because those features
  do not exist in this milestone.

## What it does not do

- No network scanning, discovery, listening sockets or pairing.
- No model catalog, model download, inference or chat.
- No remote-device controls. The marker exists only for the local node and
  changes only in-memory presentation state.
- No CUDA/ROCm compute or GPU benchmark work. The integration tests are
  CPU/software headless tests and do not launch a GUI window.
- No persistence: selection and the visual-intent marker are discarded when the
  window closes.

Window rendering uses the normal OpenGL driver through eframe's glow backend;
that is UI rendering, not FASTLINK compute.

## Inventory contract (schema v1)

The desktop shell consumes this JSON document on stdout. The shape below is the
implemented `fastlink devices --json` output; values are illustrative and
sanitized.

```json
{
  "schema": "fastlink.devices.v1",
  "schema_version": 1,
  "scope": "local_only",
  "discovery_mode": "passive_host_evidence",
  "node_count": 1,
  "nodes": [
    {
      "node_id": "<opaque hashed id>",
      "node_id_source": "machine-id",
      "hostname": "<hostname>",
      "os": "linux",
      "os_release": "<kernel release>",
      "arch": "x86_64",
      "logical_cpu_count": 8,
      "ram_total_bytes": 17179869184,
      "ram_available_bytes": 8589934592,
      "ram_free_bytes": 4294967296,
      "gpus": [
        {
          "card": "card0",
          "name": "<GPU name or null>",
          "driver": "<driver or null>",
          "vendor_id": "0x0000",
          "pci_device_id": "0x0000",
          "vram_total_bytes": 8589934592,
          "vram_free_bytes": 6442450944
        }
      ],
      "capabilities": {
        "capacity_bytes": null,
        "throughput_bps": null,
        "network_connectivity": "unknown",
        "remote_peers": []
      },
      "unknown_fields": [
        "capabilities.capacity_bytes",
        "capabilities.throughput_bps",
        "capabilities.network_connectivity"
      ]
    }
  ]
}
```

Rules applied by the shell:

- `schema` must be `fastlink.devices.v1`; `schema_version` must be `1`.
- `scope` must be `local_only`; `node_count` must match `nodes.len()`.
- `local_only` describes at most one node. When exactly one node is present it
  is the local device.
- `node_id` is an opaque backend identifier. A missing id shows `Unknown` and
  disables selection and the visual-intent marker for that node rather than
  inventing an id.
- Empty or duplicate non-null `node_id` values are rejected.
- Optional/unknown values may be `null`; they render as `Unknown`.
- GPU entries and capability fields have the backend-defined structure above;
  unknown GPU fields stay unknown.
- `unknown_fields` is displayed verbatim as the CLI's explicit list of values it
  could not determine.
- Unknown additive JSON fields are ignored for forward compatibility.
- An empty `nodes` array is shown as an honest empty state, not replaced with a
  synthetic device.

A sanitized fixture with the exact backend field set and JSON layout lives at
`crates/fastlink-gui/testdata/devices-v1.json`. Real hostnames, machine IDs,
node hashes and GPU identifiers are never committed; the fixture uses synthetic
values with the same types.

## Requirements

- Linux with X11 or Wayland and an OpenGL driver.
- The live `fastlink devices` inventory is Linux-only and returns
  `Unsupported` elsewhere. Fixture parsing remains platform-independent for
  tests.
- Rust 1.95 or newer (required by eframe/egui 0.36).
- No Node, Python, web view or display-server wrapper is needed for the native
  binary.

On the Linux/Unix capture path, no writable temporary directory is required:
CLI output is read through nonblocking pipes. The non-Unix fallback uses
temporary files, but that path is not supported or runtime-tested in this
milestone.

The GUI is a separate Cargo package (`crates/fastlink-gui`) so the core CLI and
protocol keep their small dependency set.

## Build

```bash
# Debug binary: target/debug/fastlink-gui
cargo build -p fastlink-gui

# Release binary: target/release/fastlink-gui
cargo build --release -p fastlink-gui
```

## Run

```bash
# 1. Use `fastlink` from PATH (default)
cargo run -p fastlink-gui

# 2. Point at a specific FASTLINK build
cargo run -p fastlink-gui -- --fastlink-bin /path/to/fastlink

# 3. Or set the environment variable
FASTLINK_BIN=/path/to/fastlink cargo run -p fastlink-gui

# 4. Launch the built binary directly
./target/debug/fastlink-gui --fastlink-bin /path/to/fastlink
```

Resolution order for the CLI path:

1. `--fastlink-bin PATH` (also accepts `--fastlink-bin=PATH`)
2. `FASTLINK_BIN` environment variable
3. `fastlink` looked up on `PATH`

The source command is always visible in the window footer. Argument help:

```bash
cargo run -p fastlink-gui -- --help
```

## Tests

All parser, state and UI tests run headlessly; no display server, GPU or window
is required. UI tests execute the real `ui::draw` function through
`egui::Context::run_ui` and tessellate the result on the CPU. No GUI window is
launched during integration verification.

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
cargo build -p fastlink-gui
```

The workspace's `default-members` include the GUI, so the standard commands
above cover both the core crate and the shell. Unix-only command-runner tests
use temporary fake CLI scripts to verify argument passing, error reporting and
the invocation timeout. They also cover output larger than the OS pipe capacity,
a fast 64 MiB writer, oversized-stream diagnostics, retained/truncated stderr
with accurate byte/character accounting, timeout child cleanup, and inherited
readers/writers that must neither block nor keep consuming capture resources.
Additional deterministic pipe tests inject a poll interval longer than the
timeout to prove that output from a child which completed just after the
deadline is still drained and capped, including the `timeout=0` case.

### Actual-binary contract check

`tests/command_runner.rs` also contains an environment-gated test that invokes
the real integrated CLI from `FASTLINK_BIN` and parses its live output with the
GUI parser. It skips with a printed reason when `FASTLINK_BIN` is unset so the
default suite works in GUI-only environments:

```bash
cargo build -p fastlink
FASTLINK_BIN="$PWD/target/debug/fastlink" \
  cargo test -p fastlink-gui --test command_runner \
  actual_fastlink_binary_from_environment_satisfies_parser -- --nocapture
```

The check asserts the real output's schema, version, local-only scope, node
count, sole local node and opaque hashed node id. It does not print or commit
the hostname, machine id or hash.

## Behavior notes and limitations

- **Pre-alpha.** Linux is the only verified target; Windows/macOS builds are not
  tested by this milestone.
- **One-shot reads.** The inventory is read on startup and when Refresh is
  clicked. There is no background polling.
- **Bounded invocation.** A still-running CLI process is killed after 20
  seconds so a hung command cannot freeze the UI. If the child has already
  exited when the worker observes it, a separate bounded 50 ms post-exit drain
  still reads buffered output even when the overall deadline has just passed;
  this prevents completed JSON from being misreported as empty or malformed.
  The hard byte cap is enforced during that drain too. The window remains
  responsive while the worker thread waits.
- **Bounded output capture (Linux/Unix).** stdout and stderr are nonblocking
  pipes drained by the same worker loop; no reader threads are created. The
  reader enforces a 1 MiB counted cap per stream as bytes arrive and kills the
  child when it is exceeded. The kernel pipe buffer may transiently hold one
  pipe capacity beyond the counted bytes, but capture never writes a growing
  regular file and cannot consume unbounded disk. An inherited writer is cut
  off when the read ends close.
- **Stderr diagnostics.** At most 64 KiB of stderr is retained. The rendered
  detail is separately compacted to at most 900 characters and 12 lines, and
  the error header reports bytes captured, bytes retained, retained characters,
  and that UI cap. It never claims that all captured bytes are visible.
- **Process cleanup.** On timeout, capture-limit or wait errors the owned CLI
  child is killed and reaped before the worker returns. Only the direct child is
  terminated; inherited descendants are not killed cross-platform, but pipe
  closure means they cannot keep the GUI worker waiting or consume capture disk.
- **Non-Unix fallback risk.** Other targets compile against a temporary-file
  polling fallback. Its 1 MiB check is best-effort rather than OS-enforced: a
  fast writer can overshoot between polls, and a surviving descendant may keep
  an inherited file growing until it exits. Linux is the only supported and
  runtime-tested path for this milestone.
- **Stale snapshots are labelled.** If a refresh fails, the last successful
  snapshot stays visible with a warning banner instead of being replaced by
  fabricated or cleared data.
- **Session-only consent.** Opt-in is local-node-only UI state. It does not
  pair, does not configure the CLI, is not persisted, and sends nothing.
- **Trust boundary.** The GUI runs the executable you configure. It does not
  restrict what that executable does; use a FASTLINK binary you trust. The GUI
  itself opens no sockets and performs no discovery.
- **RAM percentage** is derived only when total and available values are
  reported and consistent. Zero or inconsistent values are explained in the UI
  instead of being clamped or hidden.
- **Network capabilities remain unknown** unless the backend reports them. A
  reported zero is shown as zero; a missing value is not treated as zero.
- **Flat theme.** The shell uses egui's default dark/light styling with a small
  custom card layout; no branding or custom font assets are bundled.

## Troubleshooting

| Symptom | Meaning |
| --- | --- |
| `could not start ... No such file or directory` | The CLI path is wrong. Pass `--fastlink-bin` or set `FASTLINK_BIN`. |
| `failed (exit status: 1)` with a CLI error | The selected FASTLINK build failed. Run `fastlink devices --json` in a terminal. |
| `did not return a fastlink.devices.v1 inventory` | The CLI emitted JSON that does not match the implemented contract above. |
| `exceeded the ... capture limit` | The CLI wrote more than 1 MiB to stdout or stderr. Run it directly to find the noise. |
| `stderr:` header in an error | Reports captured bytes, retained bytes/characters, and the 900-character/12-line UI detail cap. |
| `unsupported inventory scope` | This milestone only supports `local_only`; use a matching CLI/GUI pair. |
| `did not finish within 20 s` | The CLI hung; the GUI killed it. Run the command in a terminal to diagnose. |
| Blank/black window on a remote session | No working OpenGL context. Try a local session, `LIBGL_ALWAYS_SOFTWARE=1`, or verify `glxinfo`. |
