# Changelog

All notable changes to NexTerm are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **CLI + daemon + IPC lifecycle.** `nexterm start|stop|restart|status|doctor`,
  a Unix-socket JSON IPC layer (0600 socket, `SO_PEERCRED` UID check, no TCP),
  and single-instance gating.
- **URL detector** (pure, tested) and strict whole-string URL validation
  (`http`/`https` only, length-capped, never passed to a shell).
- **Real WebKitGTK browser surface** (wry `=0.45.0`) with session lifecycle:
  `open`, `list`, `focus`, `close`, `reload`, `back`, `forward`.
- **Browser-tab companion on GNOME Terminal + X11**: a marker shell tab plus a
  reparented WebKit surface that follows the active tab, using X11 window
  tracking and AT-SPI tab/frame data.
- **Session persistence and restore** across daemon restarts
  (`nexterm-sessions.json`, atomic writes; `browser.preserve_sessions`, default on).
- **Opt-in click-to-open** (`nexterm handler status|enable|disable`): registers
  NexTerm as the `http`/`https` handler, backing up the previous handlers once
  and restoring them on `disable`. Nothing is registered unless asked.
- **Crash containment**: a GUI-loop panic fails sessions and keeps the daemon
  and IPC alive.
- **Pane-host adapter layer** (Priority 7, in progress): a `PaneHost` trait and
  a Kitty adapter (`kitten @` command builders, tolerant `kitten @ ls` parsing,
  association by the OS window's real X11 `platform_window_id`), surfaced
  read-only by `nexterm adapters`. Not yet wired into live sessions.
- **Docs**: `README.md` (install, quickstart, support matrix, security,
  limitations), `docs/support-matrix.md`, `docs/security-model.md`, and the
  architecture/ADR/research set under `docs/`.
- **Latency instrumentation**: per-session `attach_ms`/`visible_ms` milestones
  (`nexterm list` gains a `LATENCY` column) and throttled
  `[LATENCY] terminal change → dispatch` reaction logs.
- **Daemon/IPC end-to-end integration test** (`crates/daemon/tests/ipc_smoke.rs`)
  that spawns the real daemon headless and exercises the real socket.
- **Tracking responsiveness**: change classification and a measurement
  freshness gate (`classify_change`, `measurement_is_stale`, `coalesce_delay`
  in `nexterm-terminal-manager`), on-demand AT-SPI re-measurement, and an
  event-driven X11 wait instead of a fixed 50 ms poll.
- **Supervised AT-SPI walks** (`AtspiWalker`): each walk runs on a worker
  thread with a hard caller-side bound of `budget + one in-flight call` (7 s),
  backing off exponentially (60 s → 10 min) after repeated timeouts. AT-SPI can
  therefore never hang the CLI or pin the tracking thread; callers get an
  explicit unavailable/degraded answer instead.
- **Adaptive AT-SPI baseline** (`baseline_poll`): the fallback sweep is 5 s
  while a session is live and 30 s when none is (`ATSPI_IDLE_POLL`), since
  every invalidation is still requested on demand by the X11 thread. The
  demand flag is checked before parking, so a request that arrives while a walk
  is still running is never lost across a long idle interval.
- **Lingering-tab detection** (`marker_visible`, `marker_measured_since`,
  `LINGER_CONFIRM = 3 s`): a placeholder tab whose shell exits but which stays
  on screen — a terminal profile with `exit-action=hold`, or `restart` — is now
  distinguished from a normal close. The session is closed either way, and only
  a tab still demonstrably present (a live X11 title, or an AT-SPI measurement
  taken *after* the exit) three seconds later gets one `WARN` naming it.
- **AT-SPI state verifier** (`scripts/verify-atspi-state.py`): parses the
  installed `atspi-constants.h` and asserts the Rust `SHOWING`/`VISIBLE`
  indices match it, then walks the live accessibility tree decoding every
  object with the production rule (exit 0 = pass, 2 = skipped, 1 = fail).
  Latest run: 548 objects, `SHOWING ⇒ VISIBLE` for all, `SHOWING` on 12 object
  classes, and `VISIBLE`-without-`SHOWING` observed.
- **Decision tests for the session lifecycle** (`nexterm-daemon`, +4): a normal
  close warns about nothing and is not persisted for restore; a tab that
  outlives its shell warns exactly once, naming the tab and the
  `exit-action=hold` cause; hides need three consecutive misses (two keep the
  surface attached, which is what stops ms-flashing); a duplicate-marker
  collision warns once per session and never attaches to a guess. They drive
  the real `tick`/`confirm_lingering` paths and read the log back to assert it —
  each in a temp dir of its own, because `cargo test` runs them as parallel
  threads of one process and the log is the only record that a warning was (or
  was not) emitted.
- **URL-handler save/restore as a tested round-trip** (`nexterm-cli`, +2):
  `handler_backup_value`, `parse_handler_backup` and `write_backup_once` are
  split out of `handler enable|disable` and covered — a corrupt or partial
  restore file degrades to "no previous handler" instead of aborting `disable`
  with NexTerm's entry still installed, and a second `enable` provably cannot
  overwrite the backup with NexTerm's own entry (which would make `disable`
  restore the hijack). The system handler itself is never touched by the tests.
- **CI checks the AT-SPI state constants**: the verifier gained
  `--constants-only`, and the workflow installs `libatspi2.0-dev` + `python3-gi`
  and runs it, so a distro whose libatspi indices drift from our
  `SHOWING`/`VISIBLE` (25/30) fails the build instead of silently misreporting
  visibility. The live-tree half stays a local check — a runner has no
  accessibility bus.
- **Release hygiene**: `LICENSE-MIT` + `LICENSE-APACHE`, this changelog, a CI
  workflow, and `repository`/`description` metadata on every crate.
- **Concurrent IPC** (`MAX_IPC_CONNECTIONS = 32`): each accepted connection is
  served on its own `nexterm-ipc-conn` thread, so no client can delay the
  session tick or another command. Beyond the cap a client gets an honest
  `busy` reply. `nexterm status` now also reports the live AT-SPI cadence
  (`AT-SPI: baseline poll 30 s, N walk(s) since start`).

### Changed

- Idle cost: the daemon went from ~33% of a core at rest to **~1.2%** with no
  session (tracking thread 24% → 0.3%; ~4.0% with one session open). The X11
  tracker discovers terminals through the WM's `_NET_CLIENT_LIST` (one
  round-trip) instead of probing `WM_CLASS` on every root child, interns its
  atoms once per connection, and passes on events with a 25 ms throttle and a
  1 s safety net (one pass measures 0.46 ms). The GUI event loop blocks
  indefinitely when no browser window exists and keeps a bounded 16 ms
  GTK/WebKit pump only while one does. The AT-SPI baseline walk moved from 2 s
  to 5 s, then became adaptive: 30 s with no session to measure and 5 s with
  one (it exists as a fallback; changes are re-measured on demand within
  250 ms). Measured live via the new `status` counters: 2 walks/65 s idle vs
  13 walks/65 s with a session, and the first `open` out of the idle state was
  still placed `[measured]` within ~1 s. What "idle" includes is now stated per
  process set: a daemon that has opened a surface at least once keeps a shared
  `WebKitNetworkProcess` resident as its child until `nexterm stop` (measured
  0.00% of a core, ~85 MB), and its own RSS stays WebKit-initialized, so the
  no-session floor is two processes, not one. `scripts/idle-cost.py` re-measures
  any state on any host.
- Terminal tracking reacted on a timer; it now reacts to events. The X11 thread
  blocks on the X11 socket and wakes on tab switches, moves, resizes and
  minimizes (`libc::poll`, 100 ms cap), so reaction measured **0–16 ms** live
  (was 6–670 ms with a ~50 ms poll floor) while idle snapshot passes drop from
  20/s to ≤10/s. The AT-SPI thread is woken on demand and coalesces bursts to
  one walk per 250 ms, with the adaptive baseline described above.
- Placement no longer uses a measurement that predates a resize: the tracking
  thread stamps size changes separately from moves (`TermSnapshot.size_at`),
  and a stale measurement falls back to the live geometric estimate until the
  fresh one lands (bounded by the 250 ms coalescing floor). A window *move* no
  longer invalidates a measurement at all — the content rect travels with its
  window, so the previous behaviour was triggering pointless re-glues.
- The `[LATENCY] terminal change → dispatch` log now only reports X11-stamped
  changes, so a tick caused by a fresh AT-SPI sample is not misreported as
  reaction latency; the tracking thread's tick floor is 20 ms instead of 50 ms.
- Session association now anchors on the X11 window the surface was last
  attached to (`resolve_host`), so a duplicate marker title can no longer make
  the browser jump to another terminal; a genuine collision is logged once
  instead of silently grabbing the first match. Focus-back likewise targets the
  terminal the surface came from, not the first mapped window. An unanchored
  collision now **does not attach at all** (`HostMatch::Ambiguous` runs the hide
  path plus one `WARN`) instead of attaching to a best guess; verified live by
  planting two duplicate-title windows — the surface stayed detached, and it
  re-attached to its own window by itself once the duplicates were gone.
- AT-SPI D-Bus calls now carry a 2 s `method_timeout` and each snapshot walk a
  5 s deadline, so a wedged third-party app fails fast instead of stalling the
  tracking thread.
- Vendored `winit` 0.29.15 with a documented patch: XIM focus/destroy paths
  log-and-continue instead of panicking on `BadWindow` around reparented
  windows (`vendor/winit/README.nexterm.md`).
- The whole workspace is now `rustfmt`-clean (`cargo fmt --all`), and the CI
  `Check formatting` step is **blocking** instead of `continue-on-error`.

### Fixed

- An IPC client that connected and sent nothing (or half a frame) parked the
  daemon's single-threaded IPC thread in `read_frame` indefinitely, so every
  other command queued behind it — observed live as `nexterm doctor` hanging
  with no output. Accepted connections are now bounded by the same read/write
  timeout *and* handled on their own threads, so a stalled peer costs at most
  one timeout and delays nobody (the integration test asserts a later command
  is answered in < 2 s while the peer is still silent). `nexterm status`
  distinguishes "stopped" from "running but NOT answering IPC (busy or
  wedged)" instead of claiming the daemon is stopped.
- The session lock was held across the whole `open`, including the ~0.5 s
  `gnome-terminal` launch and placeholder PID discovery, so the tracking tick
  and every other command waited for it. `open` is now split into a cheap
  in-lock plan and an in-lock install, with the spawn in between and no lock
  held: two clients plus the tick now run concurrently (measured max 17.5 ms
  round-trip during a cold open, versus ~600 ms of queued latency before).
- A placeholder whose browser surface could never be opened was left behind as
  an untracked tab running `sleep infinity`; it is now killed and the session
  recorded as `Failed`. `kill_placeholder` also confirms the pid still has one
  of our `NEXTERM-SLEEP-` `argv[0]`s before signalling, so a recycled PID
  cannot have an unrelated process terminated.
- `nexterm doctor` / `atspi_dump` could hang forever when the desktop's AT-SPI
  registry wedged: a blocking D-Bus call can outlive its own timeout, so the
  walk's internal budget was not a bound at all. Walks are supervised now (see
  Added) and report the degradation.
- The X11 tracking thread could busy-spin (measured at ~90% of a core): the
  wait before the next pass could compute as zero, and the loop re-armed
  immediately after every pass. Waits are now computed from what is actually
  pending, with a 1 ms floor.
- The AT-SPI snapshot's wall-clock budget did not cover the desktop-app scan,
  which names every app on the bus with a D-Bus call — a few wedged apps could
  therefore blow past the 5 s budget. The scan is now gated by the same
  deadline, making `budget + one in-flight call` the hard bound.
- Every `nexterm open` left a zombie `gnome-terminal` client as a child of the
  daemon (visible in `ps` as `<defunct>`): the spawn is now reaped on a
  throwaway thread, since the tab itself belongs to gnome-terminal-server.
- Stale AT-SPI measurements were kept when the bus went away (only the
  `atspi_ok` flag flipped), so placement could keep gluing the surface against
  rects whose source no longer existed. The frames are now cleared and the
  snapshot generation bumped, making the documented geometric-estimate fallback
  actually take effect.
- `nexterm focus` no longer reports success when it cannot focus: it returns
  the session's real state, explains that terminal tabs cannot be switched, and
  exits non-zero (with the marker tab to switch to). A reused `nexterm open`
  likewise reports `(already visible)` only when the tab is active, instead of
  an optimistic `(focused)`.
- Browser window reverted to unmapped immediately after opening: visibility is
  now driven through winit (`set_visible`), with a server-side map check,
  instead of a raw `XMapWindow` that winit's visibility tracking undid.
- Closing a session could leave its placeholder tab and `sleep infinity`
  process behind: placeholder PID discovery used `pgrep -f`, which also matched
  the `bash -c` wrapper and the transient `gnome-terminal` client, so the wrong
  PID was recorded and the real sleep was never signalled. Discovery and stale
  cleanup now match `argv[0]` exactly via `/proc/<pid>/cmdline`.

### Known limitations

- A wedged peer on the desktop's accessibility bus still costs NexTerm its
  AT-SPI measurements until it recovers (walks are bounded and backed off, the
  tracking thread and sessions keep running on X11 titles + geometric
  estimates). It is not NexTerm-specific.
- Wayland is unsupported by design; `doctor` reports it and the daemon runs
  headless.
- No Linux terminal exposes true in-tab browser embedding; NexTerm uses an
  external controlled surface rather than faking it.
- IME/compose input is disabled in browser windows.
- First-attach latency is ~2–10 s; `focus` on an inactive tab errors honestly.

[Unreleased]: https://github.com/RazinShafayet2007/nexterm/commits/main
