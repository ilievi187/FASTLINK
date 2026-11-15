# FASTLINK Linux Desktop Packaging

> **Pre-alpha status:** `fastlink-gui` currently only displays **passive local device inventory** collected by `fastlink devices --json`. **Pairing, model loading, and inference are not yet implemented.** Launching the desktop shell does not scan networks, open ports, download models, or execute GPU workloads.

## Contents

- `fastlink-gui.desktop` — Freedesktop desktop entry for `fastlink-gui`
- `fastlink-gui.svg` — Scalable SVG application icon (`fastlink-gui`)

## Build

Build the GUI binary (and the `fastlink` CLI used to query passive local inventory):

```bash
cargo build --release -p fastlink-gui
cargo build --release -p fastlink
```

The compiled binaries are written to `target/release/fastlink-gui` and `target/release/fastlink`.
Ensure `fastlink` is available on `PATH`, or pass `--fastlink-bin /path/to/fastlink` (or set `FASTLINK_BIN`).

## Per-user installation (manual)

Install the binaries, desktop entry, and scalable icon into standard XDG user directories:

```bash
install -Dm755 target/release/fastlink-gui ~/.local/bin/fastlink-gui
install -Dm755 target/release/fastlink ~/.local/bin/fastlink
install -Dm644 packaging/linux/fastlink-gui.desktop ~/.local/share/applications/fastlink-gui.desktop
install -Dm644 packaging/linux/fastlink-gui.svg ~/.local/share/icons/hicolor/scalable/apps/fastlink-gui.svg
```

Ensure `~/.local/bin` is on your desktop session `PATH` (or adjust `Exec=` in `~/.local/share/applications/fastlink-gui.desktop`).
If your desktop environment caches launcher or icon indexes, refresh them:

```bash
update-desktop-database ~/.local/share/applications 2>/dev/null || true
gtk-update-icon-cache -f -t ~/.local/share/icons/hicolor 2>/dev/null || true
```

## Validation

Validate the desktop entry and SVG XML locally before installing:

```bash
desktop-file-validate packaging/linux/fastlink-gui.desktop
xmllint --noout packaging/linux/fastlink-gui.svg
```

## Removal

Remove all per-user installed files and refresh caches:

```bash
rm -f ~/.local/share/applications/fastlink-gui.desktop
rm -f ~/.local/share/icons/hicolor/scalable/apps/fastlink-gui.svg
rm -f ~/.local/bin/fastlink-gui
rm -f ~/.local/bin/fastlink
update-desktop-database ~/.local/share/applications 2>/dev/null || true
gtk-update-icon-cache -f -t ~/.local/share/icons/hicolor 2>/dev/null || true
```
