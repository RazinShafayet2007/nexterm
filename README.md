# NexTerm — Browser integration for your existing terminal.

> **Mission 3 status:** real product on GNOME Terminal + X11 — `nexterm open`
> creates a browser-tab session (marker shell tab + reparented WebKit surface
> that follows the tab), with `close/list/focus/reload/back/forward`,
> session lifecycle, crash containment, and an honest `doctor`.
> Wayland is unsupported by design (see docs).
> NexTerm is **not** a terminal emulator and **not** a custom shell.
> Full README (install, quickstart, support matrix, security model) lands in Chunk 7.

## What was proven in Chunk 1

- Host: Ubuntu 22.04 / GNOME Terminal 3.44 (VTE 0.68) / X11 / Rust 1.96.
- No Linux terminal exposes an in-tab graphical browser embedding API — see
  `docs/platform-support.md`. MVP mode: **external controlled browser surface**
  (daemon-owned Wry/WebKitGTK window + `nexterm open` + OSC-8 links).
- WebKitGTK 2.50 runtime present; `-dev` headers deferred to Chunk 4 on purpose.

## Docs

- `docs/architecture.md` — system overview, crate layout, IPC/adapter/browser design
- `docs/platform-support.md` — evidence-backed capability matrix
- `docs/technical-decisions.md` — ADRs (Rust, Wry, adapters, IPC, shell safety)

## Build

Browser crates need WebKitGTK compile-time files. With root:

```bash
sudo apt install libwebkit2gtk-4.1-dev libsoup-3.0-dev build-essential pkg-config
cargo build
cargo test
```

Without root (this host), stage a user-local sysroot once:

```bash
./scripts/sysroot-webkit.sh
export PKG_CONFIG_PATH="$HOME/.local/share/nexterm-sysroot/usr/lib/x86_64-linux-gnu/pkgconfig:/usr/lib/x86_64-linux-gnu/pkgconfig:/usr/share/pkgconfig"
cargo build
cargo test
```

Runtime needs only the system WebKitGTK libraries (already on Ubuntu 22.04).

## Roadmap

Chunk 2: CLI + daemon + IPC lifecycle → Chunk 3: URL detector → Chunk 4: real
Wry browser → Chunk 5: first genuine adapter → Chunk 6+: tabs/polish → Chunk 7: OSS release.
