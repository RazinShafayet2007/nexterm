# NexTerm

**A real browser surface inside your existing terminal.**

NexTerm is not a terminal emulator and not a custom shell. It keeps the
terminal you already use — GNOME Terminal on X11 is the fully supported
target — and adds a real WebKit browser surface that follows your terminal
tabs, so you can develop, run a local dev server, and browse its UI without
switching windows.

The browser is a genuine engine (WebKitGTK via wry), not a screenshot, not a
text renderer, and not "an external browser relaunched with a new label."

## Status

Mission 3 (the production milestone) is **complete** on the reference
platform: `nexterm open <url>` creates a browser-tab session — a marker shell
tab plus a reparented WebKit surface that follows that tab — with
`close`/`list`/`focus`/`reload`/`back`/`forward`, session persistence across
daemon restarts, crash containment, and an honest `nexterm doctor`.

**Wayland is unsupported by design.** The surface/reparent mechanism has no
Wayland equivalent; `nexterm doctor` says so and the daemon runs headless
instead of pretending.

## What NexTerm is / is not

| It is | It is not |
|---|---|
| A daemon + CLI that manage a real WebKit window alongside your terminal | A terminal emulator |
| A surface that follows your active terminal tab | A custom shell or a shell wrapper |
| OSC-8 link affordances + `nexterm open` + opt-in click-to-open | A tool that intercepts your commands |
| Honest about what each terminal can and cannot do | A claim of in-tab embedding that no Linux terminal supports |

## Quickstart

```bash
# 1. Build (see Install below for prerequisites)
export PKG_CONFIG_PATH="$HOME/.local/share/nexterm-sysroot/usr/lib/x86_64-linux-gnu/pkgconfig:/usr/lib/x86_64-linux-gnu/pkgconfig:/usr/share/pkgconfig"
cargo build --release

# 2. Check what this machine can do
./target/release/nexterm doctor

# 3. Start the integration and open a page
./target/release/nexterm start
./target/release/nexterm open http://localhost:5173/

# 4. Manage sessions
./target/release/nexterm list
./target/release/nexterm focus 1
./target/release/nexterm close --all
./target/release/nexterm stop
```

## Install

### Build prerequisites

The browser engine links against WebKitGTK. You need its **compile-time**
files (headers, `.pc`, `.so` symlinks); the **runtime** libraries are already
present on Ubuntu 22.04.

With root:

```bash
sudo apt install build-essential pkg-config libwebkit2gtk-4.1-dev libsoup-3.0-dev
cargo build --release
```

Without root (the reference host): stage a user-local sysroot once, then
point pkg-config at it for every build.

```bash
./scripts/sysroot-webkit.sh
export PKG_CONFIG_PATH="$HOME/.local/share/nexterm-sysroot/usr/lib/x86_64-linux-gnu/pkgconfig:/usr/lib/x86_64-linux-gnu/pkgconfig:/usr/share/pkgconfig"
cargo build --release
```

Runtime needs only the system WebKitGTK 4.1 libraries — no sysroot, no root,
no dev packages.

### Install the binaries

`nexterm` locates `nexterm-daemon` either next to itself or on `PATH`, so
install both together:

```bash
cargo install --path crates/daemon
cargo install --path crates/cli
```

Or copy the two release binaries somewhere on your `PATH`:

```bash
install -Dm755 target/release/nexterm        ~/.local/bin/nexterm
install -Dm755 target/release/nexterm-daemon ~/.local/bin/nexterm-daemon
```

Requirements: Linux + X11, Rust 1.96 (2021 edition), GNOME Terminal 3.44 /
VTE 0.68 or compatible. See the [support matrix](docs/support-matrix.md).

## Commands

| Command | Purpose |
|---|---|
| `nexterm start` / `stop` / `restart` | Manage the daemon. |
| `nexterm status` | Show daemon + session status. |
| `nexterm doctor` | Diagnose OS, terminal, engine, X11, AT-SPI, capabilities, readiness. |
| `nexterm open <url>` | Create a browser-tab session for the URL (`--ensure` auto-starts the daemon). |
| `nexterm browser` | Open the configured default page. |
| `nexterm list` | List browser sessions. |
| `nexterm focus [id]` | Focus a session's window. |
| `nexterm reload [id]` / `back [id]` / `forward [id]` | Navigate a session. |
| `nexterm close [id]` / `close --all` | Close session(s); `--all` also clears persistence. |
| `nexterm terminals` | List detected terminals and their capabilities. |
| `nexterm capabilities` | Show the current terminal's capabilities. |
| `nexterm adapters` | Show pane-host adapters (terminals with their own pane-control API, e.g. Kitty). |
| `nexterm handler status\|enable\|disable` | Opt-in click-to-open (see Security). |
| `nexterm config [--path]` | Show (and create) the config file. |
| `nexterm logs [-n N]` | Tail the daemon log. |
| `nexterm version` | Print the version. |

## Support matrix

Full matrix: [`docs/support-matrix.md`](docs/support-matrix.md). Summary:

| Platform | Status |
|---|---|
| Linux + X11 + GNOME Terminal | **Fully supported** (all features verified) |
| Linux + X11 + other terminals | **Partially supported** (detect + URL handling; tab-following varies) |
| Linux + Wayland | **Unsupported by design** |
| macOS / Windows | Out of scope (MVP) |

| Terminal | Overall |
|---|---|
| GNOME Terminal (VTE) | Fully supported |
| Kitty, WezTerm, Alacritty | Partially supported |
| Konsole, tmux | Experimental |

No Linux terminal exposes an API to embed an interactive graphical surface in
one of its own tabs. NexTerm refuses to fake that; it uses an external
controlled surface that is reparented into the terminal window. The
evidence behind this is in [`docs/platform-support.md`](docs/platform-support.md).

### Pane-host adapters (in progress)

Terminals with their own pane-control API get a real adapter rather than the
GNOME-Terminal companion path. The **Kitty adapter core is implemented**: it
builds `kitten @` invocations (never through a shell), parses `kitten @ ls`,
and — notably — surfaces each OS window's `platform_window_id`, the *real*
X11 id, so a session can be associated with the actual host window instead of
a marker-title string. It creates and focuses panes through the same API.

Honest status: the adapter layer and `nexterm adapters` reporting are done and
unit-verified; **driving live sessions through it is not yet wired**, and
neither Kitty nor WezTerm is installed on the reference host, so live
verification is still outstanding. Neither host exposes *pixel* pane geometry
over its control API (Kitty reports ids/titles/cwd/cmdline; WezTerm reports
cell `size`), so pixel placement will still come from X11. See
[ADR-007](docs/technical-decisions.md).

## Security

**NexTerm does not run as root, opens no network port, and changes system
state in exactly one place — the opt-in URL handler.**

- IPC is a `0600` Unix socket with a `SO_PEERCRED` UID check; there is no TCP
  listener.
- URLs are data only: whole-string validation, `http`/`https` allowlist,
  length cap, and the URL never reaches a shell.
- `nexterm handler enable` makes NexTerm the default `http`/`https` handler
  while enabled. This is unavoidable for click-to-open (VTE has no
  per-terminal link hook). It is explicit, backed up to
  `handler-restore.json` (written once, never clobbered), and fully reversed
  by `nexterm handler disable`. `status` is read-only.
- X11 has no isolation: any same-user session process can observe/control the
  same windows. Do not rely on NexTerm to hide content from local processes.

Full statement, including what NexTerm does **not** protect against:
[`docs/security-model.md`](docs/security-model.md).

## Known limitations

- **First-attach latency is real and now measured.** On the reference host a
  cold open is ~6 s to the WebKit surface and ~9 s to first visible; later
  opens are ~2–3 s. Run `nexterm list` for a per-session `LATENCY` column
  (`vis`/`att`) and `nexterm logs` for `[LATENCY]` reaction times. The session
  reports `Attaching` meanwhile; there is no fake instant open.
- **`focus` on a non-active tab cannot work**: NexTerm cannot switch terminal
  tabs (GNOME Terminal exposes no API). It now says so precisely — names the
  session's state and the tab to switch to — and exits non-zero, rather than
  printing a vague caveat.
- **No CJK/compose (IME) input in pages.** IME is disabled because winit
  0.29's XIM handling panics on reparented windows; the vendored winit
  patches the paths to log-and-continue so the daemon survives.
- **Tab switches are tracked at ~1 s granularity** by design (event push for
  titles + poll for geometry).
- **Content sizing falls back to an estimate** when AT-SPI is unavailable
  (logged as `[estimate]`; measured is `[measured]`).
- Reparenting into the terminal window is invisible to the window manager
  (no taskbar entry — intended), so the surface has no WM-managed minimize
  animation of its own.

See [`docs/troubleshooting.md`](docs/troubleshooting.md) for symptoms and
fixes.

## Configuration

Config lives at `~/.config/nexterm/nexterm.toml` and is created with defaults
on first `nexterm config`. Unknown keys are ignored (forward compatible).

```toml
[browser]
default_url = "about:blank"
reuse_tabs = true
preserve_sessions = true     # restore {url, marker} sessions on daemon start
auto_open_localhost = false  # opt-in automatic localhost detection

[localhost]
enabled = true
ports = [3000, 4173, 5173, 8080]

[integration]
preferred_mode = "auto"
```

## Documentation

| Doc | What it covers |
|---|---|
| [`docs/support-matrix.md`](docs/support-matrix.md) | Release-facing platform/terminal/feature support. |
| [`docs/security-model.md`](docs/security-model.md) | Trust boundaries, handler caveat, what is not protected. |
| [`docs/production-readiness.md`](docs/production-readiness.md) | What shipped, assumptions, bugs/rough edges, test commands. |
| [`docs/production-architecture.md`](docs/production-architecture.md) | Threading, session lifecycle, IPC, terminal manager. |
| [`docs/platform-support.md`](docs/platform-support.md) | Evidence baseline and per-terminal capability matrix. |
| [`docs/technical-decisions.md`](docs/technical-decisions.md) | ADRs (language, engine, adapters, IPC, shell safety, sequencing). |
| [`docs/troubleshooting.md`](docs/troubleshooting.md) | Symptom → cause → fix. |
| [`docs/gnome-terminal-embedding-research.md`](docs/gnome-terminal-embedding-research.md) | Why no true in-tab embedding exists. |
| [`docs/x11-integration-research.md`](docs/x11-integration-research.md) | Reparenting/placement mechanism evidence. |
| [`docs/wayland-integration-research.md`](docs/wayland-integration-research.md) | Why Wayland is unsupported. |
| [`CHANGELOG.md`](CHANGELOG.md) | Notable changes by release. |

## Development

```bash
export PKG_CONFIG_PATH="$HOME/.local/share/nexterm-sysroot/usr/lib/x86_64-linux-gnu/pkgconfig:/usr/lib/x86_64-linux-gnu/pkgconfig:/usr/share/pkgconfig"
cargo test --workspace                                          # unit tests, no display needed
cargo run -p nexterm-terminal-manager --example atspi_dump      # live AT-SPI proof
./target/debug/nexterm start && ./target/debug/nexterm doctor
./target/debug/nexterm open http://localhost:5173/ && ./target/debug/nexterm list
```

`vendor/winit` is a pinned 0.29.15 with a documented NexTerm patch; see
[`vendor/winit/README.nexterm.md`](vendor/winit/README.nexterm.md).

## License

MIT OR Apache-2.0, at your option.
