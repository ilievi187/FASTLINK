# FASTLINK desktop shell (Linux, pre-alpha)

This document describes the first real Linux desktop shell for FASTLINK. Its
milestone scope is deliberately narrow: **local device overview and explicit
local-device opt-in**. It is a native desktop binary (`fastlink-gui`) built with
Rust, [egui](https://github.com/emilk/egui) and eframe.

**Status: pre-alpha.** The shell is aligned with the implemented
`fastlink devices --json` inventory. It also consumes the opt-in
`fastlink.discover.v1` contract behind an explicit **Discover devices** action;
the backend CLI in this integration base does not implement `discover` yet, so
that action fails closed with a safe error until a supporting CLI is supplied.

## What it does

- Invokes `<FASTLINK CLI> devices --json` on startup/Refresh for local inventory.
- Invokes `<FASTLINK CLI> discover --json` **only** after the user clicks
  **Discover devices**. It never runs on app launch, local inventory refresh,
  background timers, or provider listing.
- Parses the implemented `fastlink.devices.v1` local-only inventory.
- Parses `fastlink.discover.v1` from the M2 opt-in discovery contract:
  `user_opt_in_lan_tailscale` scope, provider availability, and unverified
  candidate metadata.
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
  **opt-in** toggle for the local node only.
- Lets the user enter or paste an explicit `.gguf` path. The catalog can hold
  several such paths. Each registration checks one path on a background worker
  and stores the file name and byte size. It does not copy the file, read
  weight bytes, scan a directory, or write the path to disk.
- Offers an explicit **Confirm load** state for a future backend. The button
  is enabled for a selected path that is not already confirmed. Confirmation
  re-checks the file and still does not load weights.
- Shows an honest empty state for peers. Chat stays visibly unavailable, and
  the shell never generates tokens, until a real inference backend is connected.
  No such backend exists in this milestone.

## What it does not do

- No automatic or background discovery. The discover command is click-only.
- No pairing, trust establishment or remote controls. Every discovery candidate
  is labelled unpaired and unverified; there is no Pair button.
- No address, IP, endpoint, MAC, raw Tailscale peer address or raw provider
  error text is displayed, logged, persisted or committed.
- No model download, weight load, inference, token generation, or chat backend.
- No directory browser and no background scan for `.gguf` files. Only the single
  path the user submits is checked.
- No persistence of the GGUF catalog or its load confirmation.
- No CUDA/ROCm compute or GPU benchmark work. The integration tests are
  CPU/software headless tests and do not launch a GUI window.
- No persistence: selection, local opt-in, the GGUF catalog, load confirmation,
  discovery results and errors are discarded when the window closes.

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
  disables selection/opt-in for that node rather than inventing an id.
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

## Opt-in discovery (M2 slice)

The GUI exposes an explicit **Discover devices** button below the local device
view. Clicking it runs exactly:

```bash
<FASTLINK CLI> discover --json
```

The command uses the same `--fastlink-bin` / `FASTLINK_BIN` / `PATH` selection,
bounded process runner, timeout, output cap and cleanup as local inventory.
There is no discovery at launch, on local **Refresh**, on a timer, or from the
provider list. While a discovery is active, the button is disabled and a
**Cancel** action is shown. On Linux, Cancel terminates the CLI's process group,
including provider helper processes, and the UI stays busy until that cleanup
result arrives. After a run finishes, the same discover button acts as a refresh
action.

The sanitized contract shape is:

```json
{
  "schema": "fastlink.discover.v1",
  "scope": "user_opt_in_lan_tailscale",
  "providers": [
    {"provider": "lan_mdns", "status": "available", "message": null},
    {"provider": "tailscale", "status": "unavailable", "message": "safe class only"}
  ],
  "nodes": [
    {
      "node_id": "opaque-id",
      "display_name": "Example device",
      "os": null,
      "arch": null,
      "logical_cpu_count": null,
      "ram_total_bytes": null,
      "ram_available_bytes": null,
      "gpus": [],
      "transports": ["lan"],
      "worker_state": "unverified",
      "trust_state": "unpaired"
    }
  ]
}
```

Privacy and honesty rules:

- Only `available`, `unavailable` and unknown provider statuses are shown.
  Provider `message` values are discarded and never rendered: they may contain
  raw provider error text or addresses.
- Address-like fields (`ip`, `addr`, `address`, `endpoint`, `mac`,
  `mac_address`, `peer_ip`, `tailscale_ip`) cause the whole discovery response
  to fail closed. Unknown additive fields are otherwise dropped.
- Display names that look like IP/MAC/endpoint data are withheld. This includes
  endpoint-style DNS-style hostnames with a valid nonzero numeric port, whether
  single-label, dotted or internationalized/Unicode (for example `gpu-box:18777`,
  `büro:41641`, `büro.example:0443`, or decomposed `bu\u{0308}ro:41641`), in
  display names, OS or architecture values. Non-ASCII host labels are handled
  conservatively when a numeric port is present; ordinary labels such as
  `cuda:0`, `Qwen2.5:72b`, `Radeon RX 7900 XT` and `service:port`, plus DNS names
  and ordinary Unicode names without a port, remain usable. Opaque node IDs are
  validated against a safe identifier alphabet; unsafe IDs fail closed.
- Candidate OS/architecture metadata is sanitized, and unknown resource fields
  stay `Unknown`. GPU entries are counted only; their contents are dropped.
- Every candidate is labelled **Unpaired · unverified**. Pairing is not
  implemented, so there is no Pair button and no candidate enters a scheduler or
  worker-ready state.
- A Tailscale transport label is not evidence of a FASTLINK worker. The UI says
  this explicitly on every tailnet candidate.
- Invalid schema/scope, forbidden address fields, malformed JSON, oversized
  arrays, unsafe candidates or command failures produce a fixed safe error with
  no raw stderr, provider text, addresses or candidates.
- Discovery results and errors are in-memory only and disappear when the window
  closes.

A sanitized discovery fixture lives at
`crates/fastlink-gui/testdata/discovery-v1.json`. All provider names, candidate
names and IDs are synthetic.

## Session GGUF catalog

The central panel has a **GGUF catalog** section. The user types or pastes one
path and presses **Register GGUF path**. The shell accepts the path only when
all of the following are true:

- the trimmed path is non-empty;
- the final extension is `.gguf`, compared case-insensitively, including when
  the name is not valid UTF-8;
- opening that one path succeeds and `fstat` on the opened handle reports a
  regular file, not a directory, FIFO, socket, or other node. On Unix the open
  uses `O_NONBLOCK`, so a path swapped to a FIFO cannot block the worker. On
  Linux the canonical display path comes from the opened descriptor
  (`/proc/self/fd/N`); a separate opened-file identity uses the handle's device
  and inode on Unix. Windows and other non-Unix targets retain a best-effort
  canonical-path identity, where a symlink swapped during the check is not
  fully excluded. Neither identity reads or fingerprints file contents, so an
  in-place edit to the same filesystem object is not detected. Confirmation
  requires both the stored path and opened-file identity to match and fails
  closed with a "path changed" error otherwise, including replacement at the
  same pathname on Unix. On Windows, `\\.\`, `\??\`, UNC shares and
  named pipes (`\\server\pipe\…`), and non-local `\\?\` forms are refused
  before `CreateFile`; extended local paths such as `\\?\C:\…` and
  `\\?\Volume{…}\…` stay accepted. DOS reserved names (`NUL.gguf`,
  `COM1.gguf`, `LPT1.gguf`, …) are refused on Windows only, so those names stay
  valid Linux files. The shell does not impose a timeout or cancellation on OS
  filesystem calls. Unix `O_NONBLOCK` only prevents waiting on a FIFO open; it
  does not bound `open`, metadata, or canonicalization on a stalled filesystem.
  A stalled local or remote filesystem can leave the model worker busy until the
  OS call returns. A normal Windows file open likewise cannot be cancelled.

The check runs on one background worker at a time, keeping the egui frame
responsive while the OS call runs. The check itself has no cancellation or
timeout. The Register, Confirm, and Remove buttons and row selection are
disabled while it runs; the path field remains editable and drafts entered during
the check are preserved. A stalled filesystem call therefore leaves those
actions unavailable until the OS returns.
A result carries the
catalog generation from when it was queued, so a removal that did race the
worker cannot rewrite or resurrect another entry. The open handle is closed
without a `read` call. The catalog records the path the user first submitted,
the canonical file name (a symlink alias does not replace it), and the byte
size from that `fstat`. A later registration of the same file refreshes the
size and keeps the original path, identity, and file name. A successful
registration clears the draft when it is still the submitted path. A repointed
symlink alias refreshes that same row instead of adding a stale duplicate. The painted path
and file name replace control, bidi, line/paragraph separator, tag, and
invisible filler characters. Home is taken from `HOME` or `USERPROFILE`, with
`/` and `\` treated as the same separator, and a long value keeps only its
tail. Permission errors are not described as a missing file.
GGUF magic, tensor headers, and weight bytes are not inspected, so a readable
file with the right extension is registered even when it is not a valid model.
That keeps validation from becoming a weight-content read.

A second registration of the same file, including a `..` alias that resolves to
it, selects the existing entry instead of adding a duplicate. A rejected path
leaves the existing entries in place. Confirmation is cleared only when the
rejected path is the confirmed one. **Remove
selected** drops one entry from the in-memory catalog and does not delete the
file. Sibling `.gguf` files are never added, because the shell does not list
directories.

**Confirm load** is enabled whenever a row is selected and the model worker is
idle, including when that path is already confirmed, so the user can re-check
it. The fresh inspection must match both the stored path and the stored
identity; a mismatch clears the confirmation, shows a "path changed since it
was registered" error, and does not rename the entry. A
deleted or non-file re-check, including registering that same path again, is
refused and any stale confirmation is cleared. Registering a different invalid
path leaves the existing confirmation in place. A successful check stores an
explicit pending-backend confirmation and refreshes the byte size. It does not
allocate weights, start a runtime, or change `loaded` from false. The loaded
and chat lines are rendered from those flags; the backend is not connected, so
chat stays unavailable and no tokens are generated. Selecting or registering a
different file clears that confirmation. Chat remains **Not available** in the workspace,
with the statement that no inference backend is connected and that this window
does not generate tokens. There is no token list and no completion stream.

Registration does not start discovery, refresh, or an application-managed
network request. However, inspecting a path on NFS/CIFS, another remote mount,
or a mapped drive may cause OS-managed network I/O. Registration is not
guaranteed offline for such paths; use responsive local storage when offline
behavior is required. Closing the window drops the catalog; the next process
starts empty.

## Requirements

- Linux with X11 or Wayland and an OpenGL driver.
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

`tests/model_catalog.rs` creates temporary files and checks one explicit path
at a time. It proves metadata registration, rejection of a non-`.gguf` path, a
missing path, a directory (including one that contains another `.gguf`), a FIFO,
and a symlink to a directory. The unreadable-file case expects `NotReadable`
only when `File::open` itself fails, so a Windows read-only attribute or root
DAC override does not fail the test. It also proves duplicate and `..` aliases
collapse, a symlink alias keeps the canonical file name and identity, a
repeat registration refreshes size, an already confirmed path can be re-checked,
a failed re-registration of that path clears confirmation, a deleted file cannot
be confirmed, an identity or path mismatch is rejected, non-UTF-8 names stay
visible, control, bidi, line-separator, tag and filler characters are replaced
before rendering, painted paths are redacted for both `HOME` and `USERPROFILE`,
Windows device/UNC/NT namespace and DOS reserved names are classified by
platform-independent helpers while Linux files with DOS-looking names still
register, a repointed alias refreshes its row, a changed identity surfaces an
error, a background model check does not block discovery polling
or cancel, selection and removal are refused while that check runs, a stale
result cannot rewrite a removed entry, and the headless UI paints the derived
status, the confirmation, and a validation error without painting weight bytes.

Discovery tests use only the sanitized fixtures and fake CLI scripts. They prove
that launch and local refresh never invoke `discover`, an explicit action passes
`discover --json`, provider/candidate metadata renders honestly, unavailable and
empty providers are handled, bad schema/address fields fail closed, raw provider
stderr is not surfaced, and the shared runner timeout remains bounded. Privacy
regressions cover IP/MAC literals, endpoint-style single-label, dotted and
Unicode/NFC `host:port` values (`büro:41641`), decomposed/NFD endpoint labels
(`bu\u{0308}ro:41641`), ordinary non-endpoint colon labels (`cuda:0`,
`service:port`, `Qwen2.5:72b`), ordinary Unicode names without a port, and safe
UI/error rendering without echoed endpoints. A Linux regression test starts a fake discovery CLI with a long-lived
provider child, cancels from state, and verifies the CLI and provider PIDs are
gone promptly. A deterministic drain-window regression sets cancellation after
the child exits but before buffered output is accepted, proving the completed
JSON is rejected. No actual LAN/Tailscale scan is performed by the test suite.

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
- **One-shot reads.** Local inventory is read on startup and when Refresh is
  clicked. Discovery runs only when **Discover devices** is clicked. There is no
  background polling or automatic network action.
- **Bounded invocation.** A still-running CLI process is killed after 20
  seconds so a hung command cannot freeze the UI. If the child has already
  exited when the worker observes it, a separate bounded 50 ms post-exit drain
  still reads buffered output even when the overall deadline has just passed;
  this prevents completed JSON from being misreported as empty or malformed.
  The hard byte cap is enforced during that drain too. The window remains
  responsive while the worker thread waits. An active discovery can be cancelled
  with **Cancel**: on Linux the CLI is spawned in its own process group and the
  whole group is signalled, so provider children are terminated as well. The UI
  remains busy until the worker reports the cancellation; it does not merely
  hide the result. Cancellation is checked after the post-exit drain as well, so
  buffered JSON from a child that just exited is rejected if Cancel arrived
  during the drain window. A completed run can be refreshed by clicking Discover
  again.
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
- **Process cleanup.** On Linux/Unix, timeout, capture-limit, wait-error and
  explicit cancellation kill the CLI's process group, which includes provider
  helper processes. The direct child is always reaped. Normal successful exit
  does not signal the group. On non-Unix fallback targets only the direct child
  can be killed; provider descendants may survive. Linux is the supported
  runtime for this milestone.
- **Non-Unix fallback risk.** Other targets compile against a temporary-file
  polling fallback. Its 1 MiB check is best-effort rather than OS-enforced: a
  fast writer can overshoot between polls, and a surviving descendant may keep
  an inherited file growing until it exits. Cancellation on those targets kills
  only the direct CLI child, not the provider process tree. Linux is the only
  supported and runtime-tested path for this milestone.
- **Stale snapshots are labelled.** If a refresh fails, the last successful
  snapshot stays visible with a warning banner instead of being replaced by
  fabricated or cleared data.
- **Session-only consent.** Opt-in is local-node-only UI state. It does not
  pair, does not configure the CLI, is not persisted, and sends nothing.
- **Session-only GGUF metadata.** The catalog is not a model loader. Byte size
  is the file length reported by the operating system. Quantization, parameter
  count, architecture, and context length stay unknown because reading them
  would read the file. Chat cannot be used from a confirmed path.
- **Trust boundary.** The GUI runs the executable you configure. It does not
  restrict what that executable does; use a FASTLINK binary you trust. The GUI
  itself opens no sockets. Opt-in discovery is delegated to the CLI only after
  an explicit click, and its results never imply trust or worker readiness.
- **Discovery backend dependency.** The integration base for this slice does not
  yet implement `fastlink discover --json`; with that binary the action fails
  closed with a safe error. Tests use fixtures/fake CLI scripts only.
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
| `Discovery response did not match the expected schema` | The discovery JSON was invalid or contained forbidden address fields. No candidates are shown. |
| `Discovery timed out` | The bounded discovery command did not finish in time. Run it directly to diagnose without exposing provider output in the GUI. |
| `Discovery command failed` | The CLI exited unsuccessfully; raw stderr/provider text is intentionally not shown. |
| `did not finish within 20 s` | The CLI hung; the GUI killed it. Run the command in a terminal to diagnose. |
| Blank/black window on a remote session | No working OpenGL context. Try a local session, `LIBGL_ALWAYS_SOFTWARE=1`, or verify `glxinfo`. |
| GGUF path rejected | The path is empty, not a `.gguf` file, missing, a directory, or not readable. On Windows, device/UNC/pipe namespaces and DOS reserved names are also refused. The shell does not search nearby files. |
| Path changed since registration | Confirmation re-opened the file and the stored path or identity no longer matches. Nothing was confirmed; re-register the path to refresh the row. |
| Load confirmed, chat still unavailable | Confirmation is only a session flag. No inference backend is connected, so no weights are loaded and no tokens are generated. |

## Run a GGUF on Linux

Build the desktop app with `cargo build --release -p fastlink-gui` and launch
`target/release/fastlink-gui`. In Devices and models, register a local GGUF and
choose **Confirm load**. Open **Chat**, enter the path to a compatible
`llama-server`, and choose **Load model (CPU)**. Send a prompt when the model is
ready. **Cancel response** ends the current stream. **Stop server** releases
the coordinator process and its model.

### Distribute across trusted SSH workers

Use matching RPC-enabled llama.cpp builds on the coordinator and workers. Use
the CPU build on the coordinator if you want to reserve its accelerators for
other jobs. On each worker, start an RPC server bound to IPv4 loopback. For a CPU
worker with the current upstream binary:

```sh
ggml-rpc-server --host 127.0.0.1 --port 50052 --device CPU --threads 2
```

Verify each SSH host key out of band and enroll it in OpenSSH's known-hosts
store. Set up key-based login using a default OpenSSH identity or `ssh-agent`.
FASTLINK uses direct OpenSSH connections with strict host-key verification,
batch authentication, and loopback-only forwarding. Each worker's SSH server
must permit forwarding to its local RPC port.

Enter one worker per line under **SSH workers (optional)**:

```text
alice@worker-a 22 50052
bob@worker-b 22 50052
```

Choose **Distribute and load model**. llama.cpp selects layer placement based
on the available device memory and sends model tensors from the coordinator
through the SSH forwards as it loads. The GGUF source file stays on the
coordinator. FASTLINK owns the forwards for this session and closes them when
you stop the server. If a tunnel exits, the app stops the session. Restore the
worker and load the model again. Worker processes are administered separately.
Use workers you trust with the model and inference data.
