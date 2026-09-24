# NexTerm Platform Support (Chunk 1 — Evidence Baseline)

> Inspected 2026-09-22. Host: Ubuntu 22.04.5 (Jammy), kernel 6.8.0-138-generic,
> x86_64, X11 (`XDG_SESSION_TYPE=x11`, `DISPLAY=:0`), GNOME desktop,
> shell `bash`, `TERM=xterm-256color`, terminal **GNOME Terminal 3.44 / VTE 0.68**,
> Rust 1.96, WebKitGTK runtime 2.50.4 (4.0 + 4.1 `.so` present, `-dev` headers absent),
> Chrome 142 + Firefox 156 installed, `tmux` absent.

## 1. Headline Finding (do not fake this)

**No existing Linux terminal examined exposes an API to embed an interactive,
graphical Chromium/WebKit surface inside one of its own tabs or panes via a
generic CLI.** A generic `nexterm` binary cannot inject a live browser into
arbitrary terminal emulators. Any claim of universal "embedded browser tabs"
would be false.

Therefore the MVP integration mode is:

> **External controlled browser surface** — a daemon-owned, real-engine (Wry /
> WebKitGTK) window managed alongside the terminal, plus terminal-native
> affordances (OSC-8 clickable links, `nexterm open`) — with capabilities
> detected and reported honestly per terminal.

## 2. Per-Terminal Capability Matrix (Linux)

| Terminal | Detect signal | embedded_browser | split_pane | tab control | graphics | OSC-8 hyperlinks | Status |
|---|---|---|---|---|---|---|---|
| GNOME Terminal (VTE 0.68, this host) | `XDG_CURRENT_DESKTOP=GNOME`, `VTE_VERSION`, process `gnome-terminal-server` | **NO** — VTE renders a text cell grid only; no widget-embedding API | NO programmable CLI API | NO (GUI only, `--tab` spawns shell tabs, not browser surfaces) | NO (no Sixel / Kitty / iTerm2 protocol in VTE) | **YES** (VTE ≥ 0.50 supports OSC-8) | Partially supported |
| Kitty | `$KITTY_WINDOW_ID`, `kitty @` remote control | NO — graphics protocol shows *images*, panes host PTYs only, not interactive WebViews | YES via `kitty @` (only with `allow_remote_control`) | YES via remote control | YES (Kitty protocol) | YES | Partially supported |
| WezTerm | `$WEZTERM_PANE`, `wezterm cli` | NO — panes host PTYs only | YES via `wezterm cli split-pane` | YES via `wezterm cli` | YES (Sixel/iTerm2) | YES | Partially supported |
| Alacritty | `ALACRITTY_WINDOW_ID` | NO | NO (no split/tab API; single window) | NO | NO | YES (≥ 0.8) | Partially supported |
| Konsole | `$KONSOLE_VERSION`, D-Bus `org.kde.konsole` | NO | Partial (D-Bus window mgmt, not browser embedding) | Partial | Partial (Sixel in recent builds) | YES | Experimental |
| tmux (multiplexer, not emulator) | `$TMUX` | NO | YES (`split-window`) | YES | Depends on outer terminal | Passthrough-dependent | Experimental |

Legend: **Fully supported** = none on Linux for true embedding (by design — we
refuse to fake it). **Partially** = URL detection + session + `nexterm open` +
controlled window + hyperlink/split affordances where the terminal allows.
**Experimental** = detected but untested. **Unsupported** = detection fails;
doctor explains fallback.

## 3. Why "External Controlled Surface" Is the Honest MVP

1. VTE (GNOME Terminal's engine) has no concept of child GUI widgets — verified
   by absence of any embedding D-Bus/CLI surface in `gnome-terminal --help` and
   VTE 0.68 capabilities. Rendering is a text grid + OSC sequences.
2. Kitty/WezTerm remote-control APIs create *terminal panes*, not browser views.
   Their graphics protocols are one-way image blits, not interactive DOM surfaces.
3. The only technically legitimate "integrated" browser on this host is a real
   OS window (Wry/WebKitGTK 2.50, runtime confirmed present) whose lifecycle the
   daemon owns: open/position/focus/close + tab session + localhost workflow.
4. Anything else (screenshot-in-terminal, terminal HTML renderer, silent
   `google-chrome --app <url>` relabeled as "embedded tab") is explicitly
   forbidden by the product spec §36 and fails the MVP success criteria.

## 4. MVP Target Selection

- **First target:** the detected terminal on the dev host — **GNOME Terminal** —
  in **Partially supported** mode: OSC-8 link affordances + `nexterm open` +
  daemon-owned Wry window. No split-pane or image protocol claimed.
- **Second targets (Chunk 6+):** Kitty and WezTerm adapters, exploiting their
  real split-pane CLIs without claiming embedded browsing.
- **Out of scope for MVP:** macOS (Terminal/iTerm2), Windows (Windows Terminal)
  — architecture reserves `adapters/macos`, `adapters/windows` but no fake status.

## 5. Browser Engine Availability (this host)

- WebKitGTK **runtime** 2.50.4 present (`libwebkit2gtk-4.0/4.1.so`, JSC, libgtk-3):
  Wry can run here.
- WebKitGTK **dev headers** (`libwebkit2gtk-4.1-dev`, `webkit2gtk-4.0` include dir)
  NOT installed — expected; Chunk 4 will document
  `sudo apt install libwebkit2gtk-4.1-dev libsoup-3.0-dev build-essential pkg-config`
  as a build prerequisite. The workspace in Chunks 1–3 intentionally avoids a
  `wry` dependency so `cargo build` works before that install.
- Chrome 142 / Firefox 156 exist but are **not** the integration path (launching
  an unmanaged external browser and calling it "native integration" is forbidden).

## 6. What `nexterm doctor` Must Report on This Host (Chunk 2 contract)

```text
OS: Linux (Ubuntu 22.04)  Arch: x86_64  Shell: bash
Terminal: gnome-terminal 3.44 (VTE 0.68)
Daemon: running|stopped  Browser engine: available (WebKitGTK 2.50 runtime; dev headers missing)
Capabilities: embedded_browser NO | split NO | tabs NO | graphics NO | hyperlinks YES
Overall: Partially supported — external controlled browser surface.
         True in-terminal embedding is not exposed by this terminal.
```
