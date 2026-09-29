# NexTerm Production Architecture (Mission 3)

Target: GNOME Terminal + X11. Wayland explicitly out of the first milestone.
Source of every mechanism below: a measured PoC (`research/*`), not theory.

## 1. Shape

```text
nexterm CLI (thin: parse args → one IPC round-trip → print)
        │  Unix socket, JSON frames, v0 envelope + new commands
        ▼
nexterm-daemon (single instance, owns all state)
 ├── IPC accept loop ...... serve loop + session tick; hands each client to its
 │                           own bounded handler thread (§8c), so no client can
 │                           delay the tick or another command
 ├── Tracking threads ..... X11 (event-driven: titles/geometry/state) and
 │                           AT-SPI (frames/tabs/selection/content rects;
 │                           adaptive baseline + on-demand walks) → snapshot
 ├── Session manager ...... placeholder tabs, association rule, cleanup
 │                           (behind ONE mutex; decides, never renders)
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
- Persistence / restore: the restorable set (`{url, marker}`,
  `PersistedSession`) is written atomically (temp + rename) to
  `nexterm-sessions.json` on every open/close/reap. On start, if
  `preserve_sessions` is set, the IPC thread waits until the GUI loop is
  pumping (`GuiContext::ready`) and re-opens each URL — restore must not race
  winit's event loop, since `send_event` before `run()` fails. `close --all`
  empties the file; a clean `stop` leaves it so a restart restores.
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
  state bits 25 (`SHOWING`) / 30 (`VISIBLE`) for visibility — indices verified
  against the installed `atspi-constants.h` *and* the live tree by
  `scripts/verify-atspi-state.py` (548 objects, `SHOWING ⇒ VISIBLE` for all,
  `VISIBLE`-without-`SHOWING` observed), since a wrong index would silently
  corrupt every visibility decision. Timeouts per call; any failure degrades to
  title+X-only mode (logged, never fatal).
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

## 7. Pane-host adapters (Priority 7, in progress)

Terminals that expose their own pane-control API get an adapter instead of the
GNOME companion path. Shape (see `terminal-adapters`):

- `pane.rs` — `PaneHost` trait (`list_panes`/`create_pane`/`focus_pane`/
  `close_pane`), `PaneHandle`/`PaneInfo`/`PaneRequest`/`PaneKind`, and an
  injectable `CommandRunner` so adapters are testable without the terminal.
- `kitty.rs` — the Kitty adapter: shell-free `kitten @` argv builders
  (`ls`, `launch --type=… --keep-focus -- …`, `focus-window`/`close-window
  --match id:N`), tolerant `kitten @ ls` JSON parsing, and
  `find_os_window_by_xid` for association by the real X11 id
  (`platform_window_id`).
- `pane_hosts()` returns the adapters to probe; `nexterm adapters` surveys
  them read-only and reports reachability honestly.

Not yet wired: the daemon still routes sessions through the GNOME companion.
Routing pane-host sessions, and live verification against a real Kitty, are
the next increment (ADR-007).

## 8. Latency instrumentation (Priority 8) and tracking responsiveness (E)

Three measurements, all cheap and log/`list`-visible:

- **Milestones.** `Session` records `first_attach_at` / `first_visible_at`; the
  pure `timing_snapshot` turns them into `core::SessionTiming`
  (`since_open_ms`, `attach_ms`, `visible_ms`, event counts). `SessionManager::list`
  attaches it to each `SessionInfo`, so `nexterm list` shows a `LATENCY` column
  and `status` carries it too — no separate metrics channel.
- **Reaction.** The X11 tracking thread stamps `TermSnapshot.updated_at` on
  every real change; `serve` logs `[LATENCY] terminal change → dispatch: N ms`
  (throttled to 1/s) using `now - updated_at`, isolating the part of the lag we
  own from the terminal's and the compositor's. Only X11-stamped changes are
  logged: a tick triggered by a fresh AT-SPI sample can legitimately arrive a
  poll later, and reporting that as our reaction would be a lie.
- **Placeholder identity.** `placeholder_pids`/`find_placeholder_pid` read
  `/proc/<pid>/cmdline` and match `argv[0]` exactly. This replaces `pgrep -f`,
  which also matched the `bash -c` wrapper and the transient `gnome-terminal`
  client (their command lines merely *contain* the sleep name), so a wrong PID
  could be recorded and the real `sleep` leaked on close. Pure `argv0` is
  unit-tested against both the real process and the decoy wrapper.

## 8a. Tracking responsiveness (Priority E)

All three tracking loops no longer poll blindly, and the whole daemon is
**event-driven at rest** — measured on the reference host, idle with no session:
process total **0.82% of one core** with a daemon that has never opened a
surface, and **1.20%** once one has been opened and closed (the delta is the
shared `WebKitNetworkProcess` that stays resident — see the per-process-set
table in `production-readiness.md`, "Idle cost"). The X11 tracker is ~0.3-0.5%
of that (was 24%; the total was 33%, then ~7% before the adaptive AT-SPI
baseline below). Idle with a session open — the AT-SPI baseline back at its
responsive cadence, plus the GTK/WebKit pump and a WebKit web process — measures
~4.0% of a core. Re-measure any of these with
`python3 scripts/idle-cost.py [seconds]`.

- **GTK/WebKit pump.** With no browser window there is nothing for GTK to
  service, so the event loop blocks (`ControlFlow::Wait`) and a user event
  (`Open`, …) wakes it through the proxy; with a window it keeps a bounded
  16 ms pump (`PUMP_INTERVAL`) because GTK sources and WebKit input only run
  inside `gtk::main_iteration_do`. The drain stays capped (~50 iterations) so a
  GTK event flood cannot starve winit user events.
- **X11 discovery.** One `_NET_CLIENT_LIST` read (1 round-trip) instead of
  probing `WM_CLASS` on every root child (~38 windows × ~5 round-trips each on
  the reference desktop), with a `query_tree` fallback when the WM publishes no
  client list. Atoms are interned once per connection (`X11View`/`Atoms`)
  instead of 4-6 per window per pass. One full pass measured **0.46 ms**.
- **Pass scheduling.** Passes run on events (25 ms throttle so a drag cannot
  flood, 1 s safety net), never on a poll tick — and **never with a zero
  timeout**: an idle clamp of 1 ms is what keeps the loop sleeping instead of
  spinning (a zero-wait variant was measured spinning the thread at ~90% of a
  core).

All three tracking loops no longer poll blindly:

- **X11 thread — event-driven, not 20 Hz.** `StructureNotify|PropertyChange`
  on every tracked toplevel wakes the thread the instant the terminal moves,
  resizes, retitles (tab switch) or minimizes. Idle time is spent blocked in
  `libc::poll` on the X11 socket (`wait_for_x11_io`, capped at
  `X11_WAIT_CAP = 100 ms`), so a change is seen in ~0 ms instead of up to a
  poll interval, and idle snapshot passes (a dozen X round-trips each) drop
  from 20/s to ≤10/s.
- **AT-SPI thread — adaptive baseline plus on-demand re-measure.** A full
  accessibility walk is many D-Bus round-trips (~50-80 ms of CPU), so the
  baseline is a slow fallback sweep, and how slow it is depends on whether
  there is anything to measure: `ATSPI_BASELINE_POLL = 5 s` while at least one
  session is live, `ATSPI_IDLE_POLL = 30 s` when none is (`baseline_poll`, pure
  and unit-tested; the session count is published by the session manager).
  Nothing is lost by the longer idle interval: every change that can invalidate
  a measurement also changes the *window set*, which the X11 thread sees, and it
  then *asks* for a walk via a `Condvar` — so the first `open` after an idle
  period is still measured within ~1 s (live-verified). The AT-SPI thread also
  coalesces bursts (`ATSPI_MIN_INTERVAL = 250 ms`, `coalesce_delay`) so a window
  drag cannot turn into a D-Bus flood. `wait_for_wake` checks the demand flag
  *before* parking, because the X11 thread can ask for a walk while a previous
  one is still running — with a 30 s idle interval, a lost wake-up would mean
  "no measurement for up to 30 s after a tab switch".
  Instrumentation: `TermSnapshot.atspi_poll_ms`/`atspi_walks` are published on
  every cycle and reported by `nexterm status` (`AT-SPI: baseline poll 30 s,
  3 walk(s) since start`) — the adaptive claim is checkable, not asserted.
  Measured on the reference host: 2 walks/65 s idle (no session) vs 13
  walks/65 s with a session.
- **Snapshot change classification.** `classify_change` (pure, tested) splits
  an observation into `window_set_changed`, `size_changed`, `moved`,
  `title_changed`, `state_changed`. Size and title changes invalidate/require a
  measurement; a **move deliberately does not** — the content rect travels with
  its window, so parent-relative placement is unchanged.
- **Freshness gate.** `measurement_is_stale(frames_at, size_at)` — a measured
  rect captured before the last *size* change describes the old rectangle, so
  those ticks use the live geometric estimate instead of a stale measurement
  (the old behaviour: the surface sat at the previous rect until the next
  2 s AT-SPI sample — "resize lags a tick"). The fresh measurement lands
  within the 250 ms coalescing floor and re-glues exactly.
- **Degradation.** When AT-SPI returns nothing repeatedly, or the bus is
  unreachable, the thread clears the stored frames (not just the `atspi_ok`
  flag) and bumps `seq`: continuing to glue against rects whose source has gone
  away was the old failure mode. Placement falls back to the geometric
  estimate, which is documented and visible in the logs (`[estimate]`).
- **Budget scope (fixed).** The snapshot budget also has to cover the
  desktop-app scan, not just the tree walk: naming one app is a D-Bus call, and
  a wedged app answers nothing for the per-call timeout. The scan is now gated
  by the same deadline (`spend()`), so a snapshot can never outlast
  `budget + one in-flight call` (`snapshot_upper_bound`).

## 8b. Supervised AT-SPI walks (a wedged peer must never pin a caller)

Observed live: the desktop's AT-SPI registry stopped answering (`GetChildren`
sent to `/org/a11y/atspi/accessible/root`, no reply for >30 s, then healthy
again with no code change). Because a *blocking* D-Bus call can outlive
`ATSPI_CALL_TIMEOUT` (the call is sent, the reactor wakes on the timeout, and
the blocking caller is never resumed), no in-process deadline can bound that
wait — so the bound lives in the caller:

- **`AtspiWalker`.** Each walk runs on its own worker thread; the caller waits
  with `recv_timeout(ATSPI_WALK_BOUND)` where the bound is
  `budget + one in-flight call` (7 s), asserted equal to `snapshot_upper_bound`.
  A stuck worker is abandoned (it cannot be killed) and the caller is released
  with `Walk::Degraded` — the honest "a peer on the bus is wedged" answer.
- **Bounded pressure.** After `ATSPI_MAX_CONSECUTIVE_WEDGES` (3) timed-out
  walks the walker backs off (`ATSPI_RETRY_COOLDOWN`, doubling to
  `ATSPI_MAX_COOLDOWN` = 10 min), so a permanently broken bus costs a bounded
  number of walk attempts and threads per hour instead of one per poll.
  A fast failure (bus unreachable) is *not* a wedge: it retries immediately.
- **One connection per walk** (~3 ms measured) means a wedged bus leaves
  nothing behind but the abandoned worker; nothing to reconnect or reset.
- **Callers.** `nexterm doctor` and `atspi_dump` report the degradation instead
  of hanging (live: returned at exactly 7000 ms with exit code 2); the daemon's
  tracking thread maps `Unavailable`/`Degraded` onto its existing degrade path
  (frames cleared, `atspi_ok = false`, placement falls back to estimates), so
  association keeps running.

Related hardening found in the same investigation: the daemon's IPC thread had
**no timeouts on accepted connections**, so one client that connected and sent
nothing (`read_frame` parked on a silent peer) blocked every other command
(§8c completes the fix). Accepted streams are now bounded with the same
read/write timeout, and `nexterm status` distinguishes "stopped" from "running
but not answering". Covered by the integration test (a stalled client is
dropped and `status` recovers).

## 8c. IPC concurrency (one slow client must delay nobody)

The control plane used to be a single thread that accepted, dispatched *and*
ticked: any client could hold it for as long as it felt like, and a client that
went silent held it for the full 5 s read timeout. Timeouts made that bounded;
this makes it non-blocking for everyone else.

- **One bounded handler thread per connection.** `serve` is now *only* the
  accept/tick loop; each accepted stream is handled by its own
  `nexterm-ipc-conn` thread (`spawn_connection_handler`). Concurrency is capped
  at `MAX_IPC_CONNECTIONS` (32, `connection_capacity` is pure and tested);
  beyond it a client gets an honest `busy: too many concurrent IPC clients`
  instead of an unbounded thread per peer. A `ConnGuard` releases the slot even
  if a handler panics.
- **One mutex, still no split state.** Handlers share nothing but
  `IpcCtx.sessions`, whose lock each mutation holds briefly — the invariant the
  single-threaded design existed to protect is unchanged, and `dispatch` still
  has no session state of its own.
- **The slow half of `open` runs unlocked.** `SessionManager::open` is split
  into `plan_open` (validate, decide reuse, allocate id + marker — cheap, in
  lock) and `install_open` (adopt the spawn result — bookkeeping, in lock). The
  ~0.5 s `spawn_placeholder` (gnome-terminal launch + PID discovery) runs
  *between* them with the lock released, which is what removes the last
  reaction outlier: the tick and every other client keep running while a tab
  spawns. Restore still uses the sequential `open` (it runs before the serve
  loop exists, so nobody is waiting on it).
- **A tab with no owner is not orphaned.** If the browser event loop is gone
  when the placeholder appears, `install_open` kills the placeholder and records
  a `Failed` session, instead of leaving an untracked `sleep infinity` behind.
- **PID-reuse safety.** `kill_placeholder` now verifies `/proc/<pid>/cmdline`
  still has one of our `NEXTERM-SLEEP-` `argv[0]`s before signalling: a session
  whose tab already exited could otherwise SIGTERM an unrelated process that
  inherited the number.

Measured: with a stalled peer parked and a cold `open` spawning a tab, 198
concurrent `status` round-trips over 4 s measured max 17.5 ms (the floor is the
20 ms accept nap, unchanged); the integration test asserts the same property
end to end (`< 2 s` while the peer is still silent).
- **Tick cadence.** `serve` naps 20 ms between `accept()` calls (its tick
  floor) and re-ticks immediately when either producer bumps `seq` — so a new
  measurement is acted on the moment it lands, not on the next 1 s heartbeat.

## 8d. Association and liveness assumptions (Priority C hardening)

Both of the remaining assumptions were about *violations* of the normal case,
and both now fail loudly instead of silently misbehaving.

**A marker title is not necessarily unique on the desktop.** Markers are unique
*per daemon* (`marker_title`), but two daemons — two users, a second instance —
can independently derive the same `🌐 host:port`, and a renamed tab can collide
too. `resolve_host` therefore returns four verdicts instead of an index:

| Verdict | Meaning | Tick action |
|---|---|---|
| `Unique(i)` | exactly one mapped, non-hidden window shows the marker | attach |
| `Sticky(i)` | several do, but one is `last_xid` (the window we were attached to) | attach (no jump) |
| `Ambiguous(_)` | several do and none is our anchor | **detach** (hide path) + one `WARN` |
| `None` | none does | hide path (existing hysteresis) |

The `Ambiguous` path matters because the previous behaviour — take the first
title match — would reparent the surface into a *stranger's* terminal and steal
its focus. Refusing is safe, self-healing (the moment one window remains the
session attaches and goes `Visible`), and honest (session state and the log say
why). Live-verified by planting two duplicate-title windows.

**A tab does not always disappear with its shell.** Under the default GNOME
Terminal profile it does; a profile with `exit-action=hold` (or `restart`) keeps
the tab showing its dead child. The daemon had to distinguish three cases, and
the distinction is about *evidence freshness*, not about the profile:

- `marker_visible(snap, marker)` — live X11 window titles only, mapped and not
  hidden. This is the only evidence about *now*.
- `marker_measured_since(snap, marker, since)` — an AT-SPI frame counts only if
  the measurement was taken at or after `since` (the exit). A stale frame can
  therefore never be mistaken for current evidence.
- Anything else means the tab closed with its shell.

A candidate is confirmed after `LINGER_CONFIRM` (3 s); confirming removes it
from the list, so a lingering tab is reported exactly once. The first version
trusted stale AT-SPI frames and produced a **false positive** moments after a
normal close; the freshness split is what fixed it. Live-verified against a real
held tab (isolated `--app-id` instance + hold profile), with the window
independently confirmed still mapped via `xwininfo`.
