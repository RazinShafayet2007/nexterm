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
  window), then `pkill -f 'NEXTERM-SLEEP-[0-9]'` (bracket pattern avoids
  matching your own shell) to drop orphan placeholder tabs.

## Session stuck in `Attaching`

The marker tab is not visible to tracking: either the tab didn't open
(`gnome-terminal --tab` failed — see daemon log) or titles aren't updating
(non-standard profile overriding tab titles). `nexterm doctor` shows live
tab counts; compare with what you see.

## Tab switches feel slow (~1s)

The association tick runs every second by design (event push for titles +
poll for geometry). Sub-second tracking would need X event selection on all
terminals plus AT-SPI event subscriptions — possible, not yet implemented.

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

## Two terminals, browser attached to the wrong one

Association keys on marker titles, which are unique per session. If two
windows show the same title (e.g. a manually renamed tab colliding with a
marker), the first match wins. Rename the impostor tab.

## Logs

`nexterm logs [-n N]` tails `~/.local/share/nexterm/nexterm.log`. Every
attach/hide/focus/close decision is logged with session id and reason.
