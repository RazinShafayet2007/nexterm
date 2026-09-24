# NexTerm Architecture (Chunk 1 — Feasibility Baseline)

> Status: ADR baseline established 2026-09-22 on Ubuntu 22.04 / GNOME Terminal 3.44 / VTE 0.68.
> Stack locked per user directive: Rust CLI + Rust daemon, Wry browser backend.

## 1. What NexTerm Is / Is Not

- **Is:** a CLI-first integration layer that adds a real-browser surface to the
  developer's *existing* terminal workflow. Shell, PTY, tabs, and config stay untouched.
- **Is NOT:** a terminal emulator, a custom shell, a text HTML renderer, or an AI terminal.
  Uninstalling NexTerm must leave the normal terminal workflow intact.

## 2. System Overview

```text
User's existing terminal (bash, GNOME Terminal, Kitty, WezTerm, …)
        │  (unchanged — no PTY takeover, no command rewriting)
        │
  nexterm CLI (Rust binary, clap)
        │  IPC: Unix domain socket + JSON, localhost only
        ▼
  nexterm daemon (Rust background process, single instance)
        ├── Terminal integration (adapter trait, capability-driven)
        ├── Browser manager (Wry window, real engine)
        │       ├── Linux:   WebKitGTK
        │       ├── macOS:   WebKit (WKWebView)
        │       └── Windows: WebView2
        ├── URL detector (parse-only, never executes)
        ├── Session manager (tabs, URLs, selected tab; persisted)
        └── Configuration (TOML, no root required)
```

## 3. Crate Layout (Rust workspace)

```text
nexterm/
├── Cargo.toml                  # workspace
├── crates/
│   ├── cli/                    # `nexterm` binary (clap) → thin IPC client
│   ├── daemon/                 # `nexterm-daemon` binary → lifecycle, IPC server
│   ├── core/                   # shared types: commands, status, errors
│   ├── ipc/                    # Unix-socket JSON protocol (length-prefixed frames)
│   ├── config/                 # TOML load/save, defaults, XDG paths
│   ├── url-detector/           # pure function: text → Vec<DetectedUrl>
│   ├── terminal-adapters/      # TerminalAdapter trait + per-terminal impls
│   ├── browser/                # Wry wrapper (STUB until Chunk 4; no wry dep yet)
│   └── shared/                 # logging, paths, version helpers (optional)
├── docs/
│   ├── architecture.md         # this file
│   ├── platform-support.md
│   └── technical-decisions.md
└── README.md                   # minimal stub until Chunk 7
```

Dependency direction: `cli, daemon → core, ipc, config → shared`.
`browser` and `terminal-adapters` are only touched by `daemon`, never by `cli`
directly. `url-detector` is a pure library with zero I/O (easy to fuzz/test).

## 4. IPC Design (to be implemented in Chunk 2)

- Transport: Unix domain socket at `$XDG_RUNTIME_DIR/nexterm/nexterm.sock`
  (fallback `~/.local/share/nexterm/nexterm.sock`). No TCP port, no remote exposure.
- Framing: 4-byte big-endian length prefix + UTF-8 JSON body. Max frame 1 MiB.
- Auth: socket file mode `0700` dir / `0600` socket + `SO_PEERCRED` UID check
  (Linux). Requests from another UID are rejected.
- Protocol (v0 sketch): `{ "v": 0, "cmd": "status|open|stop|…", "args": {...} }`
  → `{ "v": 0, "ok": true|false, "data": …, "error": … }`.
- Daemon lifecycle: PID file + file lock prevents duplicates; `stop` does a
  graceful shutdown (close IPC, persist session, exit 0).

## 5. Terminal Adapter Design (to be implemented in Chunk 5)

```rust
trait TerminalAdapter {
    fn id(&self) -> &'static str;          // "gnome-terminal", "kitty", …
    fn detect(&self) -> bool;              // env / process sniffing
    fn capabilities(&self) -> Capabilities;
    fn open_browser(&self, url: &str) -> Result<()>;
    fn close_browser(&self) -> Result<()>;
}

struct Capabilities {
    embedded_browser: bool,  // can host interactive browser INSIDE a tab/pane
    split_pane: bool,        // can programmatically create a split
    tab_integration: bool,   // can open/control a native tab
    graphics_protocol: bool, // Sixel / Kitty / iTerm2 inline images
    hyperlink_support: bool, // OSC-8 clickable links
}
```

The daemon selects the highest-capability *detected* adapter at startup and
reports it via `nexterm doctor` / `nexterm capabilities`. See
`platform-support.md` for the honest finding: **no Linux terminal offers
`embedded_browser = true`**, so the MVP integration mode is
**"external controlled browser surface"** (daemon-owned Wry window +
OSC-8 hyperlink affordances + `nexterm open`), NOT fake in-terminal embedding.

## 6. Browser Subsystem (implemented in Chunk 4)

- Engine: wry `=0.45` (thin WebView wrapper). Real engine per OS, full JS/WebSocket/WebGL.
- Threading: the daemon's MAIN thread runs the winit event loop + GTK pump
  (wry requirement); the IPC server runs on a background thread and injects
  `BrowserEvent::{OpenUrl, Shutdown}` via `EventLoopProxy`. Shared
  `Arc<Mutex<BrowserSummary>>` feeds `status`. No display → headless fallback,
  `open` reports "browser unavailable" instead of faking a window.
- Build prerequisite: `libwebkit2gtk-4.1-dev + libsoup-3.0-dev` system-wide,
  or the rootless equivalent `scripts/sysroot-webkit.sh` + `PKG_CONFIG_PATH`
  (see README). Runtime needs only the system WebKitGTK `.so.0` files.
- Session model (daemon-owned, persisted as JSON):
  `Session { tabs: Vec<Tab { id, url, title, history }>, selected: TabId }`.
- Security: URL validation + allowlist for auto-open (localhost/RFC-1918 only
  with explicit opt-in); detected URLs are never executed as shell.

## 7. Vertical Slice Order (why this sequence)

1. Chunk 1 (now): prove feasibility + compilable skeleton.
2. Chunk 2: daemon/CLI/IPC lifecycle — proves persistence without a browser.
3. Chunk 3: URL detector — pure logic, heavily tested, zero GUI dependency.
4. Chunk 4: real Wry window + `nexterm open` — proves "real browser" claim.
5. Chunk 5: first genuine adapter — proves "integration" claim honestly.
6. Chunk 6+: tabs, persistence, polish.

No screenshots-as-browser, no HTML-parser-as-Chromium, no silent `chrome --app`
passed off as "native tab embedding" — violations of any of these fail the MVP.
