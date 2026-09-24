# NexTerm Production Readiness (Mission 3)

## What was implemented (and from which PoC it was extracted)

| Component | Source | Production shape |
|---|---|---|
| X11 tracking (titles/geometry/state) | `x11-browser-controller` | `terminal-manager`: same ops, noúe demo scaffolding |
| AT-SPI tab data + content rects | `rect.py` + `detect.py` | `terminal-manager` `AtspiClient` (Rust/zbus, no Python) |
| Change-gated placement | companion `last_place` | `Session.last_attach` + GUI-side gate |
| Background measurement | companion rect thread | AT-SPI polling thread (X11 stays on its own fast thread) |
| Verified hides | companion viewability check | `Hide` event + focus-back + state machine |
| Placeholder tabs | companion/`run.sh` | `SessionManager`: unique markers, liveness, stale reaping |
| Strict URL validation | Chunk 3/4 crates | unchanged (`validate_open_url`, detector) |
| IPC framing + UID check | Chunk 2 | unchanged, commands extended additively |
| Daemon lifecycle | Chunk 2 | kept + `catch_unwind` GUI containment + shared shutdown flag |

## Assumptions still standing on evidence, not proof

1. **Placeholder tabs auto-close** when their sleep dies (observed repeatedly
   under the default profile; a user profile with "hold open" would break
   the close-by-tab path — detected via liveness, reported, but the tab
   would linger).
2. **AT-SPI `GetState` bitfield layout** (verified against libatspi on one
   object class; other toolkits are irrelevant here — only VTE/GTK is read).
3. **Focus-back target**: first mapped toplevel — correct in single-window
   use, approximate with several windows (focus lands *a* terminal, maybe
   not *the* terminal).
4. **No concurrent Title condition**: marker titles are unique per daemon
   run, but two daemons (two users/sessions) could collide — single-instance
   per user mitigates; multi-user same-machine same-URL markers are
   theoretically ambiguous (different UIDs can't see each other's windows
   usefully anyway).

## Known X11 limitations

- No resize propagation *into* the child: the rule re-places on configure
  events (measured working); between event and re-glue there is a 1-tick lag.
- Content rect needs AT-SPI; without it the geometric estimate returns
  (flagged `estimate` in logs).
- Focus is asserted (`XSetInputFocus`), not negotiated — correct per tests,
  inelegant by design.
- Undecorated child + reparenting is invisible to the WM: no taskbar entry
  (desired), but also no WM-managed minimize animations for the surface
  itself (it follows the parent).
- The GUI pump is capped at ~60Hz (`WaitUntil`); idle CPU is low, but this
  is still a wakeful loop, not a zero-cost sleeper.

## Wayland limitation

Unchanged from research: not supported, honestly reported by `doctor`.
AT-SPI *detection* likely survives there; every surface mechanism does not.

## GNOME Terminal version dependencies

- Tab titles must be settable/observable (`--title` + OSC 0 + `_NET_WM_NAME`):
  standard since 3.x; verified on 3.44.
- `--tab -- <cmd>` placeholder spawning: standard CLI, verified.
- AT-SPI exposure of `page tab list` + `terminal` roles with `Component`
  extents: verified on VTE 0.68; older VTE may differ (untested).
- `sleep infinity` (coreutils ≥ 8.x): verified on 8.32.

## Security limitations

- X11 has no isolation: any same-user, same-X client can observe/control the
  same windows NexTerm uses. This is the platform, not a NexTerm bug — but
  it means NexTerm must never be relied upon to *hide* content from local
  processes.
- IPC is UID-gated (`SO_PEERCRED`) + 0600 socket; URLs are data-only
  (whole-string validation, scheme allowlist, length cap, no shell).
- Placeholder argv contains only the marker + fixed words; the URL never
  reaches a shell.
- `pgrep -f` matching uses the unique `NEXTERM-SLEEP-<id>` token only.
- IME/compose input is disabled in browser windows (`set_ime_allowed(false)`)
  AND winit 0.29's XIM focus/destroy paths are vendor-patched to
  log-and-continue (`vendor/winit`, see `vendor/winit/README.nexterm.md`):
  upstream `.expect()`s turn any `BadWindow` around reparented/raced windows
  into a dead daemon. IME stays off as defense-in-depth; revisit both if a
  newer winit makes these paths non-panicking (then drop the vendor dir).

## Remaining bugs / rough edges

- First-attach latency (~2–10s: tab spawn + WebKit init + tick) — no fake
  instant open; the session reports `Attaching` meanwhile.
- `focus` on a non-active tab errors honestly instead of switching tabs
  (tab switching is not automatable — platform fact).
- Reused-session `open` returns `focused` optimistically; the tick performs it.
- AT-SPI snapshot has no per-call timeout (zbus blocking default); a wedged
  *third-party* app could stall the AT-SPI thread (X11 tracking is on a
  separate thread and unaffected; association degrades to last-good data).
- Debug builds enable WebKit devtools; release builds do not.

## Exact test commands

```bash
export PKG_CONFIG_PATH="$HOME/.local/share/nexterm-sysroot/usr/lib/x86_64-linux-gnu/pkgconfig:/usr/lib/x86_64-linux-gnu/pkgconfig:/usr/share/pkgconfig"
cargo test --workspace            # 65 unit tests, no display needed
cargo run -p nexterm-terminal-manager --example atspi_dump   # live AT-SPI proof
./target/debug/nexterm start && ./target/debug/nexterm doctor
./target/debug/nexterm open http://localhost:5173/ && ./target/debug/nexterm list
```

## Recommended next milestone

1. Click-to-open without touching the global default browser (per-terminal
   affordance: OSC-8 underline + hint flow, or opt-in handler registration
   with explicit consent + restore).
2. Session restore across daemon restarts (persist `{url, marker}`; re-open
   tabs on start when `preserve_sessions` is set).
3. Kitty-protocol host adapter (the portable story) behind the same
   `SessionInfo` model.
4. Revisit IME once winit's XIM handling is safe with reparented windows.
