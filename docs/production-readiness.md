# NexTerm Production Readiness (Mission 3)

## What was implemented (and from which PoC it was extracted)

| Component | Source | Production shape |
|---|---|---|
| X11 tracking (titles/geometry/state) | `x11-browser-controller` | `terminal-manager`: same ops, no demo scaffolding |
| AT-SPI tab data + content rects | `rect.py` + `detect.py` | `terminal-manager` `AtspiClient` (Rust/zbus, no Python) |
| Change-gated placement | companion `last_place` | `Session.last_attach` + GUI-side gate |
| Background measurement | companion rect thread | AT-SPI polling thread (X11 stays on its own fast thread) |
| Verified hides | companion viewability check | `Hide` event + focus-back + state machine |
| Placeholder tabs | companion/`run.sh` | `SessionManager`: unique markers, liveness, stale reaping |
| Strict URL validation | Chunk 3/4 crates | unchanged (`validate_open_url`, detector) |
| IPC framing + UID check | Chunk 2 | unchanged, commands extended additively |
| Daemon lifecycle | Chunk 2 | kept + `catch_unwind` GUI containment + shared shutdown flag |
| Pane-host adapters (Kitty) | new (Priority 7) | `terminal-adapters`: `PaneHost` trait + Kitty `kitten @` builder/parser, XID association, injectable runner |
| Latency instrumentation | new (Priority 8) | `core::SessionTiming` + `TermSnapshot.updated_at` + tracked-attach/visible milestones; `nexterm list` LATENCY column; `[LATENCY]` reaction logs; event-driven X11 wait + on-demand AT-SPI re-measure + measurement freshness gate (Priority E) |
| Daemon IPC integration test | new | `crates/daemon/tests/ipc_smoke.rs`: real binary, real socket, headless, covers ping/status/0600/unknown-cmd/clean shutdown |
| Debian package | new | `packaging/build-deb.sh` + `packaging/control.in`: stripped release binaries to `/usr/bin`, docs to `/usr/share/doc/nexterm`, `Depends` computed by `dpkg-shlibdeps`; no `.desktop` file (the URL handler stays opt-in). Built and run from an extracted temp root on the reference host; **CI** builds it on every push, installs it with `dpkg -i`, runs the installed binaries, and uploads the `.deb` as an artifact |
| Lifecycle decision tests | new | `nexterm-daemon` tests driving the real `tick`/`confirm_lingering`: a normal close warns about nothing, a held tab warns exactly once, hides need 3 consecutive misses, a collision warns once per occurrence (re-arming on resolve) and never attaches to a guess |

## Assumptions still standing on evidence, not proof

1. ~~**Placeholder tabs auto-close** when their sleep dies~~ **RESOLVED (P3.1).**
   Under the default profile they do. Under a profile that holds tabs open
   (`exit-action=hold`) the tab latches onto its dead child instead, and the
   daemon now tells the two cases apart rather than guessing: a live window
   title is the only evidence about *now* (`marker_visible`), an AT-SPI frame
   counts only if it was measured *after* the exit (`marker_measured_since`),
   and a tab that outlives its shell by `LINGER_CONFIRM` (3 s) gets one `WARN`
   naming it. Either way the session is reported as "shell exited; session
   closed" — never left looking live.
2. ~~**AT-SPI `GetState` bitfield layout**~~ **VERIFIED (P3.3).**
   `scripts/verify-atspi-state.py` parses the installed
   `/usr/include/at-spi-2.0/atspi/atspi-constants.h`, asserts `SHOWING=25` and
   `VISIBLE=30` equal the Rust constants, then walks the live tree decoding
   **every** object with the production rule. Latest run: 548 objects,
   `SHOWING ⇒ VISIBLE` held for all of them, `SHOWING` seen on 12 object
   classes, and `VISIBLE`-without-`SHOWING` (the divergence the product relies
   on) observed. Exit 0 = pass.
3. ~~**Focus-back target**: first mapped toplevel~~ **RESOLVED.** Focus-back
   now prefers the window the surface was actually attached to (its tab) and
   falls back to the first mapped, non-minimized toplevel
   (`terminal_manager::pick_focus_back`, unit-tested). With several terminals
   open, focus no longer lands on an arbitrary one.
4. ~~**No concurrent Title condition**~~ **MITIGATED.** Marker titles remain
   unique per daemon, but two daemons can independently pick the same
   `🌐 host:port`. Association now anchors on the window we were last attached
   to (`resolve_host` → `Sticky`) instead of taking the first title match, and
   an unanchored collision is reported (`Ambiguous` → one `WARN` per *occurrence*:
   the latch re-arms once the marker resolves to a single window, so a collision
   that clears and returns is explained again instead of detaching the surface
   in silence) rather than silently grabbing a guess. Single-instance-per-user still
   prevents the common case. **Live-verified (P3.2):** with two
   duplicate-title windows planted, the daemon attached to nothing and logged
   one `WARN`; once the duplicates were gone it attached to its own window and
   reached `Visible`.

## Assumption hardening (Priority C)

Addressed (pure helpers, unit-tested, verified live for regressions):

- **Host association** (`terminal_manager::resolve_host`): returns
  `Unique`/`Sticky`/`Ambiguous`/`None`. Prefers the previously attached X11
  window; flags a genuine title collision once per occurrence instead of
  attaching to the wrong terminal.
- **Focus-back** (`terminal_manager::pick_focus_back`): focuses the terminal the
  surface came from, not just the first mapped window.

Also hardened:
- **Lingering placeholder tabs** (`marker_visible` + `marker_measured_since` +
  `LINGER_CONFIRM`): "tab closed with its shell" and "tab held open by the
  profile" are now distinguished, and only the latter — confirmed still on
  screen a grace period later — produces a warning. An earlier version trusted
  stale AT-SPI frames and emitted a false positive moments after a normal
  close; the split fixes that.
- **AT-SPI state decoding** is proven end-to-end (`scripts/verify-atspi-state.py`:
  constants vs. the installed header plus a live multi-role walk), not
  spot-checked. The constants half now also runs in CI
  (`--constants-only`), where a drifting libatspi index fails the build.
- **The decisions themselves**, not just their parts, are tested: normal close
  vs held tab (including the log, which is what a user sees), hide hysteresis,
  and one-warning-per-collision. What remains local-only is the *attach* step
  and the `Visible` transition they lead to, both of which are GUI events.

Still standing (documented, not fixed): the "no concurrent title" path in the
*unmanaged* direction — a user creating a window whose title collides is not
prevented, but the behavior degrades to `Ambiguous` + one `WARN` (live-verified
by planting duplicate-title windows; the daemon never attached to either).

## Known X11 limitations

- No resize propagation *into* the child: the rule re-places on configure
  events (measured working). A resize is re-glued as soon as the fresh AT-SPI
  measurement lands (≤250 ms coalescing floor); until then the surface uses the
  live geometric estimate rather than the pre-resize rect.
- Content rect needs AT-SPI; without it the geometric estimate returns
  (flagged `estimate` in logs).
- Focus is asserted (`XSetInputFocus`), not negotiated — correct per tests,
  inelegant by design.
- Undecorated child + reparenting is invisible to the WM: no taskbar entry
  (desired), but also no WM-managed minimize animations for the surface
  itself (it follows the parent).
- With a browser window open the GUI pump is still a bounded ~60 Hz loop
  (`ControlFlow::WaitUntil`, needed because GTK/WebKit input runs inside
  `main_iteration_do`); with **no** window it now blocks indefinitely
  (`ControlFlow::Wait`), so an idle daemon is a zero-wakeup sleeper.

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
  (whole-string validation, scheme allowlist, length cap, control-character
  rejection, no shell).
- The placeholder's shell program is a compile-time constant; the marker and
  sleep name enter as bash positional parameters, so URL-derived bytes are
  never shell syntax (the pre-fix version interpolated the marker into the
  script and allowed command injection from a crafted URL — see the security
  model for the history).
- Placeholder PID discovery reads `/proc/*/cmdline` and matches `argv[0]`
  against the unique `NEXTERM-SLEEP-<id>` token.
- `nexterm-sessions.json` is written 0600 (URLs carry query strings).
- IME/compose input is disabled in browser windows (`set_ime_allowed(false)`)
  AND winit 0.29's XIM focus/destroy paths are vendor-patched to
  log-and-continue (`vendor/winit`, see `vendor/winit/README.nexterm.md`):
  upstream `.expect()`s turn any `BadWindow` around reparented/raced windows
  into a dead daemon. IME stays off as defense-in-depth; revisit both if a
  newer winit makes these paths non-panicking (then drop the vendor dir).

## Idle cost (Priority F)

Measured on the reference host (GNOME Terminal 3.44, X11, WebKitGTK 2.50,
debug build). Idle is reported per **process set**, because a daemon is not one
process; reproduce any row with `python3 scripts/idle-cost.py [seconds]`.

| State (no sessions open unless stated) | Resident processes | RSS | CPU (one core) | Sample |
|---|---|---|---|---|
| daemon stopped | — | — | **0%** | — |
| daemon up, **no surface ever opened** | `nexterm-daemon` | 45 MB | **0.82%** | 60 s, 2 baseline walks |
| daemon up, **surface opened earlier, now closed** | `nexterm-daemon` + `WebKitNetworkProcess` | 147 MB + 85 MB | **1.20%** | 30 s |
| one session open | daemon + one WebKit web process + `WebKitNetworkProcess` + the marker tab's `sleep infinity` | — | **~4.0%** | prior run |

Per-thread, before/after the fixes:

| Thread | Before | After |
|---|---|---|
| `nexterm-x11` (tracking) | 24% of a core | **0.3-0.5%** |
| whole daemon, no session | 33% of a core | **0.82-1.20%** (see above) |
| whole daemon, one session open | n/a (not separated) | **~4.0%** |

Where the cost actually was (measured, not assumed): discovery probed
`WM_CLASS` on every root child — 38 children × ~5 X round-trips per pass — so
one pass cost ~24 ms; it is now one `_NET_CLIENT_LIST` read plus a handful of
calls per *terminal*, and one full pass measures **0.46 ms**. Two bugs were
found on the way: a zero-timeout wait that spun the tracking thread at ~90% of
a core, and a loop that re-armed itself immediately after every pass. The GUI
pump (the original suspect) turned out to be ~3% with a window and 0% without.
The remaining idle cost was the AT-SPI/zbus walk (~4-5% at a 2 s poll, ~3-4% at
5 s), which is why the baseline is now **adaptive**: 30 s while no session is
live and 5 s while one is (`ATSPI_IDLE_POLL` / `ATSPI_BASELINE_POLL`), with
invalidation still driven on demand by the X11 thread. That took the no-session
idle from ~7% to **0.82%** (fresh daemon) or **1.20%** (WebKit already warm,
below) without giving up a fresh measurement when a session appears. Verified on
the reference host by the counters `nexterm status` prints: 2 walks/65 s idle vs
13 walks/65 s with a session open, and `AT-SPI: baseline poll 30 s` / `5 s`
respectively.

**No sessions does not mean no processes.** WebKitGTK is initialized *inside the
daemon process* the first time a surface is created, and it does not fully
unwind when the last surface is destroyed: a shared `WebKitNetworkProcess`
stays alive as a **child of the daemon** until `nexterm stop`, and the daemon's
own RSS stays at the WebKit-initialized ~147 MB instead of dropping back to
~45 MB. Measured while idle: the network process contributes **0.00%** of a core
(~85 MB), so it costs memory, not CPU — but a daemon that has been *used* has a
floor of two processes, not one. `nexterm stop` is clean: checked immediately
after, no `nexterm-daemon`, `WebKitNetworkProcess` or `WebKitWebProcess`
remains. That floor is a deliberate trade, not an unimplemented cleanup:
keeping the engine warm costs ~85 MB and 0.00% of a core, while releasing it
would add roughly 2 s to the next `open` — see **ADR-008**.

The first ~60 s after `nexterm start` measure higher than the steady state
(2.33% over 30 s was observed) because startup walks and session restore run
inside the sample window; let it settle before comparing, which is how the
0.82% row above was taken.

## Latency instrumentation (Priority 8)

"It feels slow" is now a number, measured end to end:

- Per-session milestones (`core::SessionTiming`, surfaced by `nexterm list` as a
  `LATENCY` column and by `status`): `since_open_ms`, `attach_ms` (creation →
  first placement event sent — tab spawn + WebKit window creation), and
  `visible_ms` (creation → first time the GUI reported the surface visible).
- Reaction latency: the X11 tracking thread stamps `TermSnapshot.updated_at`
  on every real change (tab switch / move / resize); the serve loop logs
  `[LATENCY] terminal change → dispatch: N ms` (throttled to 1/s) — the part of
  the lag that is ours, separately from the terminal and compositor.
- The first placement/visible pair is logged once per session
  (`first attach sent at N ms`, `ready: attach=…, visible=…`).

Measured on the reference host (GNOME Terminal 3.44, X11, WebKitGTK 2.50,
live `nexterm open`): **attach ≈ 1.0–3.6 s, visible ≈ 2.0–5.7 s** (cold WebKit
is the slow end; a warm daemon attaching a second session measured
`attach=1010 ms, visible=2020 ms`), and reaction (terminal change → dispatch)
**0–16 ms** after the responsiveness work below — see
`docs/production-architecture.md` §8a. Before it, reaction was 6–670 ms with a
~50 ms poll floor, and a resize could place the surface at the previous rect
until the next 2 s AT-SPI sample landed.

- ~~The one reaction outlier left was structural: while the IPC thread was
  busy inside a client request (e.g. `nexterm open` spawning and
  PID-discovering a placeholder tab, ~600 ms), no tick could run, so a terminal
  change during that window was dispatched late.~~ **RESOLVED.** Connections are
  handled on their own threads and the slow half of `open` (the tab spawn) now
  runs with the session lock released, so neither the tick nor another command
  waits on a client. Measured during a cold `open` (tab spawn + WebKit surface):
  198 concurrent `status` round-trips over 4 s, max 17.5 ms — no outlier, and
  no `6024 ms`-class `terminal change → dispatch` line. See
  `docs/production-architecture.md` §8c.

## Remaining bugs / rough edges

- First-attach latency is real and now **measured** (above): the bulk is WebKit
  surface creation, not our bookkeeping. No fake instant open; the session
  reports `Attaching` meanwhile.
- ~~`focus` on a non-active tab errors honestly.~~ **RESOLVED.** It now reports
  a precise, actionable error and exits non-zero instead of a vague
  parenthetical: `session N is not focusable right now (state: hidden)`, why
  ("NexTerm cannot switch terminal tabs"), and what to do (switch to the marker
  tab, re-run). When the tab *is* active it prints `Session N focused` and
  exits 0. Tab switching itself remains non-automatable — platform fact.
- ~~Reused-session `open` returns `focused` optimistically.~~ **RESOLVED.** The
  daemon returns the session's real state and whether its tab is active; the
  CLI says `(already visible)` only when true, else `(its tab is not active; it
  appears when you switch to it)`. The re-attach is still queued for when the
  tab becomes active.
- AT-SPI snapshot is now bounded (was: no per-call timeout, could stall the
  AT-SPI thread indefinitely on a wedged third-party app): every D-Bus call
  carries a 2 s `method_timeout` (`ATSPI_CALL_TIMEOUT`) and each walk has a
  5 s wall-clock deadline (`ATSPI_SNAPSHOT_BUDGET`, via
  `walk_deadline_passed`), so a wedged app fails fast and the walk unwinds
  with partial data. X11 tracking is on a separate thread regardless;
  association degrades to last-good data.
- ~~A wedged AT-SPI registry could hang any AT-SPI client indefinitely.~~
  **RESOLVED by supervision** (`AtspiWalker`): walks run on a worker thread and
  the caller is released at `ATSPI_WALK_BOUND` (7 s = budget + one in-flight
  call), reporting `degraded — AT-SPI walk did not finish in time (a peer on the
  bus is wedged)`; repeated timeouts back off exponentially (60 s → 10 min) so a
  broken bus cannot be hammered. Verified live: `atspi_dump` returned at exactly
  7000 ms (exit 2) while the registry was wedged, and recovered on its own
  afterwards. Tracking keeps running throughout (estimates, then measured again
  once the bus answers).
- ~~A silent or slow client delayed every other command.~~ **FIXED, twice.**
  First the accepted stream was bounded (previously a client that connected and
  sent nothing parked `read_frame` forever, seen live as `nexterm doctor`
  hanging with no output), then the accept loop stopped *being* the dispatch
  loop: each connection gets its own bounded handler thread (cap
  `MAX_IPC_CONNECTIONS`), and the ~0.5 s tab spawn inside `open` runs with the
  session lock released. `nexterm status` also distinguishes "stopped" from
  "running but not answering IPC (busy or wedged)", and `status` reports the
  live AT-SPI cadence (`AT-SPI: baseline poll 30 s, N walk(s) since start`).
  The integration test stalls a raw socket mid-frame and asserts that another
  client is answered in < 2 s while the stalled peer is still open.
- The snapshot budget now gates the desktop-app scan too (`spend()` before each
  app-name D-Bus call): previously a few wedged apps could blow straight
  through the 5 s budget because only the tree walk was gated.
- Debug builds enable WebKit devtools; release builds do not.
- ~~Closing a session could leave its placeholder `sleep` alive.~~ **FIXED.**
  Placeholder PID discovery used `pgrep -f NEXTERM-SLEEP-<id>`, which also
  matched the `bash -c` wrapper and the transient `gnome-terminal` client (both
  merely *contain* the name in their command line), so the recorded PID could
  be the wrong process and `close` never signalled the real `sleep` — leaving a
  stuck tab and a CPU-idle `sleep infinity` behind. Discovery and stale
  cleanup now match **`argv[0]` exactly** via `/proc/<pid>/cmdline`
  (`placeholder_pids`/`find_placeholder_pid`); verified live: `close --all`
  now leaves no placeholder process.

## Exact test commands

```bash
export PKG_CONFIG_PATH="$HOME/.local/share/nexterm-sysroot/usr/lib/x86_64-linux-gnu/pkgconfig:/usr/lib/x86_64-linux-gnu/pkgconfig:/usr/share/pkgconfig"
cargo test --workspace            # 129 tests (128 unit + 1 daemon integration), no display needed
python3 scripts/verify-atspi-state.py   # AT-SPI constants vs. installed header + live decode (exit 0 = pass)
python3 scripts/verify-atspi-state.py --constants-only   # the half CI runs (no desktop needed)
python3 scripts/idle-cost.py 30      # idle CPU + exactly which processes are resident (any state)
cargo run -p nexterm-terminal-manager --example atspi_dump   # live AT-SPI proof
./target/debug/nexterm start && ./target/debug/nexterm doctor
./target/debug/nexterm open http://localhost:5173/ && ./target/debug/nexterm list
./target/debug/nexterm handler status   # opt-in only; changes nothing
./target/debug/nexterm adapters         # read-only pane-host survey (Kitty/…)
./target/debug/nexterm list             # LATENCY column: vis/att seconds per session
cargo test -p nexterm-daemon --test ipc_smoke   # real daemon + real socket, headless (CI-safe)
```

## Recommended next milestone

1. ~~Click-to-open.~~ **DONE (opt-in), with a documented limitation.** GNOME
   Terminal/VTE offers no per-terminal link hook: a *clicked* URL always goes
   to the xdg default handler. So the only honest mechanism is registration,
   which does change the system http/https handler while enabled — hence
   `nexterm handler enable|disable|status`: explicit, backed up
   (`handler-restore.json`, written once and never clobbered by a re-enable),
   and reversible (`disable` restores the exact previous handlers and removes
   the entry). The entry runs `nexterm open --ensure %u`, and `--ensure`
   auto-starts the daemon because a link click has no terminal to print a
   "run nexterm start" hint in. Nothing is registered unless asked.
2. ~~Session restore across daemon restarts.~~ **DONE.** The restorable set
   (`{url, marker}`) is persisted atomically to `nexterm-sessions.json` on
   every open/close; on start, when `preserve_sessions` is set (default), the
   IPC thread waits for the GUI loop to be pumping then re-opens each URL.
   `close --all` empties the file; a clean `stop` deliberately leaves it so a
   restart restores the user's tabs.
3. ~~Release engineering.~~ **DONE.** `README.md` is now the full release
   README (install with/without root, quickstart, command table, support
   matrix summary, security summary, known limitations, docs index);
   `docs/support-matrix.md` is the release-facing platform/terminal/feature
   matrix; `docs/security-model.md` consolidates the trust boundaries and —
   importantly — states that `nexterm handler enable` changes the system
   http/https handler while active (explicit, backed up once, reversible),
   that X11 has no isolation, and that Wayland is unsupported by design.
   A **Debian package** is built by `packaging/build-deb.sh` (release binaries,
   stripped, `Depends` derived from them); installing it puts `nexterm` on
   `PATH` at `/usr/bin/nexterm`, which also fixes the trap where a from-source
   `handler enable` records an absolute path inside `target/`. Verified here by
   extraction + running from the extracted tree on the reference host. The
   real `dpkg -i` needs root, which that host does not have, so **CI** does it:
   it builds the package, installs it with `dpkg -i`, asserts it is registered,
   runs the installed `nexterm version|doctor|help`, and uploads the `.deb`.
   That is also the only place the packaged CLI is shown to find
   `nexterm-daemon` beside itself in `/usr/bin`.
4. Kitty-protocol host adapter (the portable story) behind the same
   `SessionInfo` model — **adapter core DONE, session wiring pending.**
   `nexterm-terminal-adapters` now has a real `PaneHost` trait (`pane.rs`) and
   a Kitty adapter (`kitty.rs`): shell-free `kitten @` command builders,
   tolerant `kitten @ ls` JSON parsing, and association by the OS window's
   `platform_window_id` (the real X11 id). It is injectable
   (`CommandRunner`), unit-tested (12 tests), and stub-verified end-to-end
   through `nexterm adapters` (a fake `kitten` on `PATH` exercised detection →
   spawn → parse → XID). `nexterm adapters` is read-only and never
   creates/places/closes a pane. **Not done:** the daemon still drives sessions
   through the GNOME companion path; routing panes through the adapter, and
   live verification against a real Kitty (absent on this host — needs a
   downloaded standalone binary), remain. See ADR-007 for why Kitty/WezTerm
   adapters were chosen over a VS Code companion.
5. Revisit IME once winit's XIM handling is safe with reparented windows.
6. ~~Instrument latency/tearing.~~ **DONE** (see "Latency instrumentation"
   above): per-session `attach_ms`/`visible_ms`, a `nexterm list` LATENCY
   column, and throttled `[LATENCY] change → dispatch` reaction logs.
7. ~~UX correctness.~~ **DONE.** `focus` reports real focusability (and exits
   non-zero when it cannot focus, with the reason and the next step); a reused
   `open` reports the session's actual state instead of an optimistic
   `focused`. Verified live on both the active-tab and inactive-tab paths.
8. Release hygiene: **DONE** — `LICENSE-MIT` + `LICENSE-APACHE`, `CHANGELOG.md`,
   a CI workflow (`.github/workflows/ci.yml`), and `repository`/`description`
   metadata on every crate. A daemon/IPC **end-to-end integration test** is
   also in place (`crates/daemon/tests/ipc_smoke.rs`).
9. ~~Tracking responsiveness.~~ **DONE.** The X11 thread blocks on the X11
   socket (instant wake on tab switch / move / resize, ≤10 snapshot passes/s
   idle), the AT-SPI thread re-measures on demand with 250 ms coalescing and
   clears its measurements when the bus goes away, and a stale measurement is
   no longer glued over a resized window (`classify_change` /
   `measurement_is_stale`, unit-tested). Live: reaction **0–16 ms** (was
   6–670 ms); the estimate→measured correction after a size change was observed
   end-to-end. The one remaining outlier was the IPC thread being busy inside a
   client request — resolved in item 11.
10. ~~Idle cost / GUI pump.~~ **DONE.** The pump blocks when no window exists,
   the X11 tracker is event-driven with `_NET_CLIENT_LIST` discovery and cached
   atoms, and the zero-timeout spin that made it 90% of a core is fixed
   (33% → ~7% total idle, tracker 24% → 0.3%; now **0.82%** with the adaptive
   AT-SPI baseline, item 13, or **1.20%** once WebKit has been initialized —
   see the per-process-set table above). The zombie `gnome-terminal` client per
   open is
   reaped too. Remaining: supervising the AT-SPI walk against a wedged registry
   (see the bug above).
11. ~~Supervise the AT-SPI walk.~~ **DONE.** `AtspiWalker` bounds every walk
   (7 s), backs off after repeated timeouts, and reports degradation instead of
   hanging — live-verified at the bound with a wedged registry. Accepted
   connections are bounded too, and `status` reports "not answering" instead of
   "stopped" when the daemon is busy or wedged.
12. ~~Serve IPC without stalling the tick.~~ **DONE.** Each connection is
   handled on its own bounded thread (cap `MAX_IPC_CONNECTIONS`; `busy` beyond
   it), the session mutex is the only shared state, and the ~0.5 s tab spawn in
   `open` now runs with that lock released (`plan_open`/`install_open`).
   Live-verified with a stalled raw socket parked *and* a cold `open` in
   flight: 198 concurrent `status` round-trips, max 17.5 ms (the 20 ms accept
   nap is the floor), and no reaction outlier in the log. `kill_placeholder`
   also verifies the pid is still ours before signalling (PID reuse), and a
   placeholder whose surface cannot be opened is killed instead of leaked.
13. ~~Adaptive AT-SPI baseline.~~ **DONE.** `baseline_poll` keeps the responsive
   5 s sweep while a session is live and 30 s with none, publishing
   `atspi_poll_ms`/`atspi_walks` for `nexterm status`. `wait_for_wake` checks the
   demand flag before parking so an on-demand request cannot be lost across a
   long idle interval. Live: no-session idle **0.82%** of a core fresh and
   **1.20%** with WebKit warm (was ~7%; the extra process is
   `WebKitNetworkProcess`, see above), a session open ~4.0%; walk rate 2/65 s
   idle vs 13/65 s with a session, and
   the first `open` out of the idle state was still placed `[measured]` within
   ~1 s.
