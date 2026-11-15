# FASTLINK desktop shell (Linux, pre-alpha)

This document describes the first real Linux desktop shell for FASTLINK. Its
milestone scope is deliberately narrow: **local device overview and explicit
local-device opt-in**. It is a native desktop binary (`fastlink-gui`) built with
Rust, [egui](https://github.com/emilk/egui) and eframe.

**Status: pre-alpha.**

## What it does

- Invokes exactly one command: `<FASTLINK CLI> devices --json`.
- Parses the versioned inventory contract below and shows device cards.
- Shows OS, architecture, logical CPU cores, total/available RAM, compute
  backend, GPU entries and network-interface entries **only when reported**.
- Labels missing or `null` values as `Unknown`. It never estimates, probes the
  OS itself, or fills gaps with plausible-looking numbers. (An absent device
  name is labelled `Unnamed device`.)
- Provides manual **Refresh**, device **selection**, and a session-only
  **opt-in** toggle.
- Shows honest empty states for peers, models and chat because those features
  do not exist in this milestone.

## What it does not do

- No network scanning, discovery, listening sockets or pairing.
- No model catalog, model download, inference or chat.
- No remote-device controls.
- No CUDA/ROCm compute or GPU benchmark work.
- No persistence: selection and opt-in are discarded when the window closes.

Window rendering uses the normal OpenGL driver through eframe's glow backend;
that is UI rendering, not FASTLINK compute.

## Inventory contract (schema v1)

The desktop shell consumes this JSON document on stdout:

```json
{
  "schema_version": 1,
  "devices": [
    {
      "id": "local",
      "name": "<hostname>",
      "os": "linux",
      "architecture": "x86_64",
      "cpu_logical_cores": 8,
      "ram_total_bytes": 17179869184,
      "ram_available_bytes": 8589934592,
      "gpu_devices": [],
      "backend": "cpu",
      "network_interfaces": []
    }
  ],
  "status": "local_only"
}
```

Rules applied by the shell:

- `schema_version` must be `1`; other versions fail with a visible error.
- Optional/unknown values may be omitted or `null`; they render as `Unknown`.
- `gpu_devices` and `network_interfaces` have no entry shape in v1. Entries are
  displayed as reported (strings verbatim, objects as compact JSON) rather than
  being interpreted or guessed.
- Duplicate or empty device ids are rejected.
- Unknown additive JSON fields are ignored for forward compatibility.
- An empty `devices` array is shown as an honest empty state, not replaced with
  a synthetic device.

Device inventory is implemented by the separate
`feat/linux-device-inventory` work. On branches without it, the GUI shows the
CLI failure (including the CLI's stderr) and a hint instead of pretending the
device is unavailable or fabricating telemetry.

## Requirements

- Linux with X11 or Wayland and an OpenGL driver.
- Rust 1.95 or newer (required by eframe/egui 0.36).
- No Node, Python, web view or display-server wrapper is needed for the native
  binary.

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
`egui::Context::run_ui` and tessellate the result.

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
cargo build -p fastlink-gui
```

The workspace's `default-members` include the GUI, so the standard commands
above cover both the core crate and the shell. Unix-only command-runner tests
use temporary fake CLI scripts to verify argument passing, error reporting and
the invocation timeout.

## Behavior notes and limitations

- **Pre-alpha.** Linux is the only verified target; Windows/macOS builds are not
  tested by this milestone.
- **One-shot reads.** The inventory is read on startup and when Refresh is
  clicked. There is no background polling.
- **Bounded invocation.** The CLI process is killed after 20 seconds so a hung
  command cannot freeze the UI. The window remains responsive while the worker
  thread waits.
- **Stale snapshots are labelled.** If a refresh fails, the last successful
  snapshot stays visible with a warning banner instead of being replaced by
  fabricated or cleared data.
- **Session-only consent.** Opt-in is local UI state. It does not pair, does not
  configure the CLI, is not persisted, and sends nothing.
- **Trust boundary.** The GUI runs the executable you configure. It does not
  restrict what that executable does; use a FASTLINK binary you trust. The GUI
  itself opens no sockets and performs no discovery.
- **RAM percentage** is derived only when both total and available values are
  reported and consistent. Zero or inconsistent values are explained in the UI
  instead of being clamped or hidden.
- **Flat theme.** The shell uses egui's default dark/light styling with a small
  custom card layout; no branding or custom font assets are bundled.

## Troubleshooting

| Symptom | Meaning |
| --- | --- |
| `could not start ... No such file or directory` | The CLI path is wrong. Pass `--fastlink-bin` or set `FASTLINK_BIN`. |
| `failed (exit status: 2)` with usage text | The selected FASTLINK build has no `devices` subcommand yet. Use a build from the inventory work. |
| `did not return a schema v1 inventory` | The CLI emitted JSON that does not match the contract above. |
| `did not finish within 20 s` | The CLI hung; the GUI killed it. Run the command in a terminal to diagnose. |
| Blank/black window on a remote session | No working OpenGL context. Try a local session, `LIBGL_ALWAYS_SOFTWARE=1`, or verify `glxinfo`. |
