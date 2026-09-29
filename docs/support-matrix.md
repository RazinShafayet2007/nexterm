# NexTerm Support Matrix

Release-facing status. Every claim here is backed by a measured proof or an
explicit "not supported" — never a guess. The evidence baseline (versions,
probe commands, per-terminal capability table) lives in
`docs/platform-support.md`; this file is the product-facing summary.

Last verified: 2026-09-27, Rust 1.96, on the reference host below.

## Reference host

| | |
|---|---|
| OS | Ubuntu 22.04.5 (Jammy), x86_64, kernel 6.8 |
| Session | X11 (`XDG_SESSION_TYPE=x11`, `DISPLAY=:0`) |
| Desktop | GNOME |
| Terminal | GNOME Terminal 3.44 (VTE 0.68) |
| Browser engine | WebKitGTK 2.50.4 (wry 0.45, WebKitGTK 4.1 + libsoup3) |
| Shell | bash, `TERM=xterm-256color` |

## Platform support

| Platform | Status | Notes |
|---|---|---|
| Linux + X11 + GNOME Terminal | **Fully supported** | The primary target; all features below verified on the reference host. |
| Linux + X11 + other terminal | **Partially supported** | Detection + URL handling work; tab-following placement requires a terminal whose windows expose titles/geometry (see terminal table). |
| Linux + Wayland | **Unsupported by design** | The reparented-surface mechanism has no Wayland equivalent. `nexterm doctor` states this; the daemon runs headless (IPC alive, `open` reports "browser unavailable"). |
| macOS / Windows | **Out of scope (MVP)** | Architecture reserves adapter slots; no status is claimed until implemented. |

## Terminal support (Linux)

| Terminal | Embedded browser | Tab-following surface | Split-pane control | OSC-8 links | Overall |
|---|---|---|---|---|---|
| GNOME Terminal (VTE) | No (VTE renders a text grid; no embed API) | **Yes** (reparented Wry surface follows the marker tab) | No | Yes | **Fully supported** |
| Kitty | No | Adapter core: real X11 id via `kitten @ ls` + native pane creation (not yet wired into sessions) | Adapter creates native panes; session wiring pending | Yes | **Partially supported** |
| WezTerm | No | Detect + URL handling only (trait-ready) | Not yet (adapter planned) | Yes | **Partially supported** |
| Alacritty | No | Detect + URL handling only | No | Yes | **Partially supported** |
| Konsole | No | Detect + URL handling only | No | Yes | **Experimental** |
| tmux (multiplexer) | No | Detect + URL handling only | Not yet | Passthrough | **Experimental** |

"No embedded browser" is a statement about the terminal, not about NexTerm:
no Linux terminal examined exposes an API to embed an interactive graphical
surface in one of its own tabs. NexTerm does not fake it (see ADR-003).

## Feature support

| Feature | Status | Notes |
|---|---|---|
| `nexterm open <url>` | Yes | Creates a marker tab + browser session; strict URL validation (http/https only). |
| Tab-following (show/hide with the active tab) | Yes | X11 + AT-SPI tracking; the surface hides when its marker tab is not active. |
| `list` / `focus` / `close` / `reload` / `back` / `forward` | Yes | Localhost-first workflow. |
| Session restore across daemon restarts | Yes | On by default (`browser.preserve_sessions`); `close --all` clears it. |
| Click-to-open clicked links in NexTerm | Opt-in only | `nexterm handler enable` — changes the system http/https handler while active; reversible. See security model. |
| Automatic localhost detection | Opt-in only | `browser.auto_open_localhost` (default off). |
| `nexterm doctor` | Yes | Reports OS, terminal, daemon, engine, X11, AT-SPI, capabilities, readiness. |
| `nexterm adapters` | Yes | Read-only survey of pane-host adapters (Kitty, …); reports reachability honestly, never creates/places/closes a pane. |
| Latency visibility (`nexterm list` `LATENCY` column, `[LATENCY]` logs) | Yes | Per-session `attach`/`visible` milliseconds and terminal-change→dispatch reaction time. |
| CJK / compose (IME) input in pages | No | IME disabled; winit 0.29 XIM paths are vendor-patched to avoid panics. |
| In-terminal (in-tab) browser embedding | No | Not offered by any Linux terminal; NexTerm uses an external controlled surface reparented into the terminal window. |

## Browser engine

| | |
|---|---|
| Engine | WebKitGTK (via wry `=0.45.0`) |
| Build-time dependency | `libwebkit2gtk-4.1-dev`, `libsoup-3.0-dev` (or the user-local sysroot from `scripts/sysroot-webkit.sh`) |
| Runtime dependency | System WebKitGTK 4.1 (`libwebkit2gtk-4.1.so.0`); no bundled engine |
| Devtools | Enabled in debug builds only |

## Legend

- **Fully supported** — verified end-to-end on this platform/terminal.
- **Partially supported** — core features work; some terminal-specific
  affordances (split-pane control) are not implemented.
- **Experimental** — detected and handled, but not verified against a live
  instance in the reference environment.
- **Unsupported** — mechanism is impossible or not implemented; `doctor`
  explains the fallback.
