# NexTerm Production Architecture (Mission 3)

Target: GNOME Terminal + X11. Wayland explicitly out of the first milestone.
Source of every mechanism below: a measured PoC (`research/*`), not theory.

## 1. Shape

```text
nexterm CLI (thin: parse args → one IPC round-trip → print)
        │  Unix socket, JSON frames, v0 envelope + new commands
        ▼
nexterm-daemon (single instance, owns all state)
 ├── IPC thread ........... serve loop; pure dispatch; session decisions
 ├── Terminal-manager thread X11 poll (titles/geometry/state) + AT-SPI poll
 │                           (frames/tabs/selection/content rects) → snapshot
 ├── Session manager ...... placeholder tabs, association rule, cleanup
 │                           (lives on IPC thread; decides, never renders)
 └── MAIN thread (GUI) .... winit event loop + wry/WebKitGTK windows
                             (renders, reparents, focuses; never discovers)
```

Shared state (all `Arc<Mutex<…>>`, never cross-thread GTK/wry handles):

- `TermSnapshot`: per toplevel `{xid, title, geo, mapped, hidden}` +
  per frame `{title, tabs, selected, content_rect?}`.
- `Sessions`: `id → BrowserSession {id, url, marker, host_xid?, state}`.
- `BrowserSummary`: per-session `{open, url, windows}` for `status`.

## 2. Session lifecycle (single source of truth)

```text
Creating → Attaching → Visible ⇄ Hidden → Closing → Closed
                ↘ Failed (placeholder/browser unrecoverable)
```

- `open <url>`: validate (existing strict rules) → allocate id + unique
  marker title (`🌐 host:port`, `(2)`-suffixed on collision) → spawn
  placeholder (`gnome-terminal --tab --title=… -- bash --norc … OSC-title …
  exec -a NEXTERM-SLEEP-<id> sleep infinity`) → session `Creating`.
- Association rule (every GUI iteration, change-gated): marker title visible
  on a mapped, non-minimized toplevel → reparent browser into it at the
  AT-SPI content rect, map, focus (`Visible`); else unmap, focus back
  (`Hidden`). Rect from AT-SPI `terminal` extents; geometric estimate only
  as fallback (logged).
- `close [id]`: kill placeholder (tab auto-closes) → destroy window →
  `Closed`. Daemon shutdown: close all, remove socket/pid (stale placeholders
  reaped by name pattern on next start).
- Terminal disappears: session stays, browser unmapped; re-attaches if an
  equal-titled window returns, else reported `Hidden` until closed.

## 3. IPC (v0 envelope kept, commands extended)

Envelope unchanged: `{v:0, cmd, args} → {v:0, ok, data?, error?}` (no
released clients; additive only). New commands: `open` (now returns
`{session_id, marker, url}`), `close {id?}`, `list` (`{sessions:[…]}`),
`focus {id}`, `reload {id}`, `back {id}`, `forward {id}`. Navigation uses
engine JS (`history.back()/forward()`, `location.reload()`) — wry 0.45 has
no native back/forward API; `WebView::url()` exists for verification.

## 4. Terminal manager (new crate `terminal-manager`)

- X11 (`x11rb`, proven patterns): toplevel discovery by `WM_CLASS`,
  `_NET_WM_NAME` titles, geometry, `_NET_WM_STATE` hidden, mapped state.
- AT-SPI (new: `zbus` blocking client): bus address via `org.a11y.Bus`,
  root `/org/a11y/atspi/accessible/root`, walk by role *name*, `Selection`
  for tab index, `Component.GetExtents(SCREEN=0)` for content rects,
  state bit 25 (`SHOWING`) for visibility. Timeouts per call; any failure
  degrades to title+X-only mode (logged, never fatal).
- Pure, tested helpers: state-bit test, marker uniqueness, placement
  change-gating, rect-select (measured vs estimate).

## 5. What is deliberately NOT built

- No GTK embedding, no XEmbed, no overlay-as-product (reparent only).
- No Python in the product path (`rect.py` logic moves into Rust/zbus).
- No Wayland support (honest `doctor` warning instead).
- No default-browser takeover without explicit user action (opt-in command
  later; Mission 3 wires `nexterm open` only).
- No PoC code copied blindly: change-gating, background measurement,
  verified hides, and cleanup discipline are extracted as named rules.

## 6. Security model (carried over + extended)

Unix socket 0600 + `SO_PEERCRED` UID check (existing). URLs are data
(validated whole-string, schemes http/https only, length-capped). Placeholder
processes run as the user with fixed argv (no shell interpolation of the
URL — the URL never appears in shell text). Stale-XID races handled by
re-validating the xid→title binding before every operation. Focus is asserted
only toward our own windows or back to the terminal.
