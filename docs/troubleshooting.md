# NexTerm Troubleshooting

## `nexterm open` says "browser unavailable: daemon runs headless"

The daemon could not initialize GTK/wry. Causes, in order:

1. No X display (`DISPLAY` unset, SSH without `-X`). Fix: run on the desktop session.
2. Wayland session. Fix: use an Xorg/X11 session (Wayland is unsupported).
3. Missing WebKitGTK runtime. Check: `ldconfig -p | grep webkit2gtk-4.1`.

`nexterm doctor` distinguishes all three.

## A separate browser window flashes (or stays)

- A brief flash at open is the window being created before the first
  attach; it should vanish within seconds as the tab rule applies. If it
  persists, check `nexterm list`: state `Attaching` with no transition
  means the placeholder tab never appeared (was GNOME Terminal reachable?
  see logs). State `Failed` + log line explains why.
- A window lingering after `stop`/crash: kill it normally (it is an ordinary
  window). For orphan placeholder tabs prefer `nexterm close --all`; if the
  daemon is already gone, **inspect by `argv[0]` before signalling** — `pkill
  -f`/`pgrep -f` also match the `bash -c` wrapper and the transient
  `gnome-terminal` client, and will match the shell you run them from:
  ```bash
  for p in /proc/[0-9]*; do
    printf '%s %s\n' "${p#/proc/}" "$(tr '\0' ' ' < $p/cmdline 2>/dev/null | cut -c1-40)"
  done | grep NEXTERM-SLEEP   # inspect, then `kill <pid>` only those pids
  ```

## Session stuck in `Attaching`

The marker tab is not visible to tracking: either the tab didn't open
(`gnome-terminal --tab` failed — see daemon log) or titles aren't updating
(non-standard profile overriding tab titles). `nexterm doctor` shows live
tab counts; compare with what you see.

## A placeholder tab stays open after its shell exits

With the default GNOME Terminal profile the tab disappears along with its
shell. A profile that holds tabs open (`exit-action=hold`; `restart` behaves
similarly) leaves the tab behind showing its dead child. The daemon detects
this instead of misreporting it — the session is closed either way, and only
the leftover tab is flagged:

```text
[INFO] Session 1 placeholder shell exited; session closed
[INFO] Closed session 1
[WARN] placeholder tab "🌐 host:port" is still open 3s after its shell exited
      (a terminal profile that holds tabs open, e.g. exit-action=hold);
      close that tab to remove it
```

The warning is deliberately late (3 s grace) and requires evidence from *after*
the exit — a window title still on screen, or a fresh AT-SPI measurement — so a
normal close never produces it. Fix: close the stuck tab, and check the profile
that opened it:

```bash
P="org.gnome.Terminal.Legacy.Profile:/org/gnome/terminal/legacy/profiles:/:$(gsettings get org.gnome.Terminal.ProfilesList default | tr -d \"'\")/"
gsettings get "$P" exit-action     # want: 'close'
gsettings set "$P" exit-action 'close'
```

## AT-SPI reports "degraded — walk did not finish in time"

The desktop's AT-SPI **registry** is wedged: a `GetChildren` call to
`/org/a11y/atspi/accessible/root` is sent and never answered, and a single
D-Bus call can block past the per-call timeout. It is not NexTerm-specific (any
AT-SPI client is affected) and it usually clears by itself when the
unresponsive app deregisters — observed live, going from "wedged" to "68 ms"
with no code change.

NexTerm no longer hangs on it: walks are supervised, so `nexterm doctor` and
`cargo run -p nexterm-terminal-manager --example atspi_dump` return within
`ATSPI_WALK_BOUND` (7 s) and say so. The daemon backs off (60 s, doubling to
10 min) and keeps tracking with X11 titles + geometric estimates, so sessions
keep working. Nothing to fix locally; if it persists, find the wedged app.

Note the baseline sweep is **adaptive**: `nexterm status` reports
`AT-SPI: baseline poll 30 s` when no session is open, which is expected — with
nothing to measure, the fallback sweep is rare and changes are still requested
on demand, so the first `open` is measured within about a second. With a
session live the same line reads `5 s`.

## `nexterm status` says "NOT answering IPC (busy or wedged)"

The daemon process is alive but did not answer within the command's timeout.
Connections are handled on their own threads and every stream is bounded, so a
stalled client cannot cause this — the daemon itself is busy or wedged. Check
the log and restart it (`nexterm stop` / `nexterm start`); your shell and tabs
are unaffected.

## Tab switches feel slow (~1s)

Fixed. Terminal tracking is event-driven: the X11 thread blocks on the X11
socket and wakes on the tab switch itself (`StructureNotify`/
`PropertyChange`), and the AT-SPI thread re-measures on demand within a 250 ms
coalescing floor. Measured reaction (terminal change → dispatch): **0-16 ms**,
previously 6-670 ms with a ~50 ms poll floor. The other outlier — the IPC
thread being busy inside a client request like `nexterm open` — is gone too:
connections have their own threads and the tab spawn runs without the session
lock, so a cold `open` no longer delays the tick (measured max 17.5 ms
round-trip during a cold open).

## The browser covers the tab bar / is misaligned

The placement should always be `[measured]` (see daemon log). If you see
`[estimate]`, AT-SPI is unreachable — check `doctor`'s AT-SPI line. The
estimate cannot know the tab-bar height and will be approximate.

## Typing goes to the terminal instead of the page

Focus follows the association rule: click the page once to focus it. If keys
never arrive, check the log for `attach` lines — a hidden (unmapped) window
cannot take focus by design.

## No CJK/compose input in pages

Known limitation (see `production-readiness.md`): IME is disabled because
winit 0.29 panics on XIM focus of reparented windows.

## `doctor` says AT-SPI unavailable

Tab data falls back to X titles (open/hide still work; sizing is estimated).
Cause is usually a missing/broken `at-spi-bus-launcher` session service;
re-login usually restores it. Verify with the `atspi_dump` example.

## Two terminals show the same marker title (log says `NOT attaching`)

Markers (`🌐 host:port`) are unique per daemon, but two daemons — two users, or
a second instance — can independently pick the same one, and a renamed tab can
collide too. The daemon refuses to guess rather than grabbing the wrong
terminal:

```text
[WARN] Session 1 marker "🌐 host:port" is shown by more than one window and
      none is the one this session was attached to; NOT attaching
      (rename or close the duplicate tab)
```

The surface stays detached (state `Creating`) until exactly one window shows
the marker, then the daemon attaches by itself and the session becomes
`Visible` — measured live: it recovered automatically the moment the duplicate
was killed. Rename or close the impostor to clear it faster.

## A `WebKitNetworkProcess` is running but I have no sessions

Expected, not a leak. WebKitGTK initializes *inside the daemon* the first time a
surface is created, and a shared `WebKitNetworkProcess` stays alive as a child
of `nexterm-daemon` after every session is closed — it is the engine kept warm
for the next `open`. Measured while idle: **0.00%** of a core, ~85 MB RSS. The
daemon's own memory likewise stays WebKit-initialized (~147 MB) rather than
returning to its fresh ~45 MB.

It goes away with the daemon, and only then:

```bash
nexterm stop                        # no nexterm-daemon / WebKit* process remains
python3 scripts/idle-cost.py 30     # show exactly what is resident, and its CPU
```

`nexterm stop` closes NexTerm's sessions, not your shell — and a later `open`
recreates the network process, so stopping is the only way to release it.

## Logs

`nexterm logs [-n N]` tails `~/.local/share/nexterm/nexterm.log`. Every
attach/hide/focus/close decision is logged with session id and reason.
