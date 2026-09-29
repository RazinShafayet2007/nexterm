# NexTerm Technical Decisions (Chunk 1 ADRs)

## ADR-001 — Language: Rust for CLI + Daemon

- **Decision:** single Rust workspace for `nexterm` CLI and `nexterm-daemon`.
- **Why:** one toolchain already on host (cargo/rustc 1.96); static binaries ease
  `curl | sh` distribution; strong std for Unix sockets, file locks, process
  management; `clap`, `serde/serde_json`, `toml`, `anyhow/thiserror` cover
  CLI/IPC/config needs; memory safety matters for a security-sensitive daemon.
- **Alternatives rejected:** Go (good, but team stack directive says Rust; also
  weaker WebView story), Node.js/TS (heavy runtime, worse daemon semantics),
  Python (fine for detector prototyping, wrong for single-binary distribution).
- **Consequence:** all crates compile with stable Rust; workspace builds without
  system WebKit headers until Chunk 4.

## ADR-002 — Browser Engine: Wry (WebKitGTK on Linux)

- **Decision:** Wry as the WebView wrapper; Linux backend WebKitGTK,
  macOS WebKit, Windows WebView2 (per stack diagram).
- **Evaluation:**
  | Option | Linux | Real engine (JS/WS/WebGL) | Binary size | Integration difficulty | License |
  |---|---|---|---|---|---|
  | **Wry** | **WebKitGTK, runtime 2.50 confirmed on host** | Yes (full WebKit) | Small (system WebKit) | Moderate (wry event loop in daemon thread) | Apache-2/MIT |
  | Tauri | Same WebKit underneath + app framework | Yes | Small | Higher (pulls app runtime we don't need) | Apache-2/MIT |
  | CEF | Yes | Yes (full Chromium) | Huge (~100–200 MB), manual subprocess mgmt | High | BSD |
  | Electron | Yes | Yes | Huge (~80–150 MB) | High, second runtime | MIT |
  | Custom HTML renderer / screenshot view | N/A | **No — forbidden fakes** | — | — | — |
- **Why Wry wins for MVP:** real engine, smallest distribution (links system
  WebKitGTK already on Ubuntu), no app-framework baggage, per-OS backend matches
  the long-term matrix, DevTools-capable later (`with_devtools`).
- **Consequence:** Chunk 4 requires `libwebkit2gtk-4.1-dev + libsoup-3.0-dev`;
  `browser` crate is a stub until then. Version pin: wry 0.4x line (4.1 API on
  Jammy); exact pin verified at Chunk 4 start.

## ADR-003 — Terminal Integration: Capability-Driven Adapters, Honest Fallback

- **Decision:** `TerminalAdapter` trait with explicit `Capabilities`
  (`embedded_browser`, `split_pane`, `tab_integration`, `graphics_protocol`,
  `hyperlink_support`); daemon detects at runtime, never assumes.
- **Why:** evidence (§platform-support) shows no Linux terminal offers embedding;
  only per-terminal detection prevents false "embedded ✓" claims.
- **MVP mode:** `external-controlled-surface` for GNOME Terminal (OSC-8 + `open`
  + daemon-owned window). Kitty/WezTerm split-pane control comes later as a
  *pane-management* affordance, never marketed as embedded browsing.
- **Forbidden:** screenshots-as-browser, terminal HTML renderers, silent
  `chrome --app` relabeled as integration, PTY/command interception.

## ADR-004 — IPC: Unix Domain Socket + JSON

- **Decision:** `$XDG_RUNTIME_DIR/nexterm/nexterm.sock` (fallback
  `~/.local/share/nexterm/`), length-prefixed JSON, `0600` socket, `SO_PEERCRED`
  UID check.
- **Alternatives rejected:** TCP localhost (port conflicts, broader attack
  surface), named pipes/FIFO (half-duplex awkwardness), D-Bus session bus
  (GNOME-only, harder cross-platform story).
- **Security:** bind localhost-only by construction (no TCP at all); validate +
  sanitize every URL; detector output is data, never shell; no auto-open outside
  explicit user action or localhost opt-in.

## ADR-005 — Shell Safety: No Interception

- **Decision:** NexTerm never wraps commands, never requires a `nexterm` prefix,
  never rewrites stdin/stdout, ships no custom shell. Localhost detection in MVP
  is explicit (`nexterm open`) plus OSC-8 link affordances; any future
  passive detection (prompt hooks, VTE watcher) is opt-in and read-only.
- **Why:** spec §10 — `npm install`, `git status`, `docker compose up` must behave
  identically with NexTerm installed, running, or uninstalled.

## ADR-006 — Sequencing: Prove Vertically, Abstraction Later

- **Decision:** Chunk order 1 (feasibility+skeleton) → 2 (lifecycle/IPC) →
  3 (detector) → 4 (real browser) → 5 (first adapter) → 6+ (tabs/polish).
- **Why:** each chunk has an independently verifiable claim; the two
  highest-risk claims ("real browser renders JS apps", "integration is genuine")
  are proven before cross-platform adapter sprawl. Anti-overengineering rule (§30).

## ADR-007 — Second Backend: Terminal Pane Adapters, Not a VS Code Companion

- **Decision (Priority 7):** NexTerm's second backend is per-terminal
  *pane-host adapters* (starting with Kitty), built on the `PaneHost` trait in
  `nexterm-terminal-adapters` — not a VS Code webview companion.
- **Why:** the product premise is "a browser surface inside *your existing
  terminal*". Pane adapters extend that premise to terminals that expose their
  own pane-control API and remove the fragile parts of the GNOME path:
  `kitten @ ls` reports each OS window's `platform_window_id` (the real X11
  id), so association keys on the actual window rather than a marker title, and
  panes are created natively instead of via a `bash -c` placeholder shim.
- **Rejected — VS Code companion:** a webview-based VS Code extension is
  effectively a different product (extension host, marketplace distribution),
  shifts away from the "existing terminal" premise, and overlaps VS Code's own
  Simple Browser. `TERM_PROGRAM=vscode` stays a reported *capability*, not a
  second product.
- **Honest limits:** neither Kitty nor WezTerm exposes *pixel* pane geometry
  over its control API (Kitty: ids/titles/cwd/cmdline; WezTerm: cell `size`),
  so pixel placement still uses X11; and both are absent on the reference host,
  so the adapter is unit- and stub-verified only until a live host exists.
- **Status:** adapter layer + `nexterm adapters` reporting implemented;
  wiring sessions through it is the next increment.

## ADR-008 — Keep the WebKit Engine Warm While Idle

- **Decision:** do **not** release WebKitGTK when the last browser surface is
  closed. A `WebKitNetworkProcess` and the daemon's WebKit-initialized heap
  stay resident until `nexterm stop`.
- **Why:** the alternative buys memory and costs latency. Shutting the engine
  down when idle would free ~85 MB (network process) plus ~100 MB of daemon RSS,
  but the network process measures **0.00% of a core** while idle, and
  re-initializing it puts roughly **2 s** back on the next `open` (measured:
  `attach` 3090 ms cold vs 1027 ms warm). A browser surface that takes three
  seconds to appear after you type `nexterm open` is the product's core promise
  failing, so the trade is rejected.
- **Rejected — release on idle / release after a timeout:** adds a
  lifecycle state machine (shutdown, re-init, failure during re-init) and a
  latency cliff, to reclaim memory that is not being burned as CPU. If users
  complain about RSS, the intended shape is an **opt-in config**, not a change
  to the default.
- **Honest limits:** the resident floor means "no sessions" is not "no
  processes", and the idle cost of a *used* daemon (daemon + network process,
  ~147 + ~85 MB, ~1.20% of a core) is larger than that of a fresh one
  (~45 MB, ~0.82%). Both figures and the measurement method are in
  `docs/production-readiness.md`; `scripts/idle-cost.py` reproduces them.
- **Status:** decided; behaviour unchanged, now stated explicitly rather than
  implied by the absence of cleanup code.

## ADR-009 — Distribution: a Debian Package *plus* the Source Build

- **Decision:** ship both. `packaging/build-deb.sh` produces
  `nexterm_<version>_amd64.deb` (+ a `-dbgsym`-free stripped pair) for
  Ubuntu 22.04/amd64, and building from source stays documented and supported.
- **Why:** source-only is free for us and expensive for everyone else — it
  requires a Rust toolchain *and* the WebKitGTK `-dev` packages (root, or the
  `scripts/sysroot-webkit.sh` workaround plus a `PKG_CONFIG_PATH` export). It
  also breaks `nexterm handler enable`, which records `current_exe()`, so a
  dev build pins the system's http/https handler to `target/debug/`. Installed
  at `/usr/bin/nexterm` that path is correct, `nexterm` is on `PATH`, and the
  install is two commands.
- **Depends are computed, not hand-written:** `dpkg-shlibdeps` runs over the
  staged binaries, so the field follows what the binaries actually link.
  `libatspi2.0-0` is correctly *absent* — it has zero direct `NEEDED` entries
  in the daemon and arrives transitively through `libwebkit2gtk-4.1-0`.
- **Deliberately not done by the package:** it ships no `.desktop` file and
  does not enable the daemon at login. Registering the URL handler stays an
  explicit, reversible `nexterm handler enable`.
- **Honest limits:** the reference host has no `sudo`, so the package is
  verified by extraction (`dpkg-deb -x`) and by running both binaries from the
  extracted tree — not by a real `dpkg -i` plus desktop-database update.
  `lintian` is not installed, so there is no independent policy check, and
  there is no CI artifact yet.

## Open Questions (for Chunk 4 / 5)

1. ~~Pin exact `wry` 0.4x patch against WebKitGTK 2.50 on Jammy; confirm 4.0 vs 4.1 API.~~
   RESOLVED (Chunk 4): `wry = "=0.45.0"` (4.1 API: WebKitGTK 4.1 + libsoup3,
   matching Jammy's WebKitGTK 2.50 runtime). No root on the build host, so
   compile-time files (`.pc`, headers, `.so` symlinks) are staged user-locally
   via `scripts/sysroot-webkit.sh` (`~/.local/share/nexterm-sysroot`); the
   runtime links the already-installed system `.so.0` files. With root, prefer
   `sudo apt install libwebkit2gtk-4.1-dev libsoup-3.0-dev`.
   wry 0.45 no longer re-exports tao: windowing is `winit 0.29` directly, with
   the documented `gtk::init()` + `gtk::main_iteration_do` pump and an Xlib
   error hook ignoring benign error 170. Verified: X11 session, real window,
   JS/fetch/WebSocket execution, navigation reuse.
2. Window-placement protocol on X11 vs Wayland (`winit` hints differ) — verify focus/raise behavior.
   PARTIAL (Chunk 4): X11 confirmed working incl. `focus_window()` on reuse.
   Wayland is honestly rejected at startup (wry X11 backend requirement);
   headless fallback keeps the daemon + IPC alive with `open` reporting
   "browser unavailable".
3. Whether an opt-in shell hook (e.g. `PROMPT_COMMAND` scanner) is wanted, or
   explicit `nexterm open` + OSC-8 suffices for MVP.
