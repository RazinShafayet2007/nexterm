# Wayland Integration Research (Mission J, analysis — no local Wayland session)

This host runs X11 only (`XDG_SESSION_TYPE=x11`, no `WAYLAND_DISPLAY`), so
nothing below was executed here. It is protocol/design analysis, stated as
such, to answer exactly what Wayland permits and prohibits for each Mission 2
mechanism.

## What Wayland fundamentally prohibits

- **No cross-process window embedding.** Surfaces belong to the client that
  created them. Subsurfaces (`wl_subsurface`) must be created by the SAME
  client as the parent — a foreign surface cannot attach to GNOME Terminal's
  window. There is no `XReparentWindow` equivalent by design (a deliberate
  security property: clients are isolated from each other's windows).
- **No foreign geometry/control.** No `ConfigureWindow`, no `SetInputFocus`,
  no property snooping (`_NET_WM_NAME`-style state is compositor-private).
  A client cannot move, resize, hide, raise, minimize, or focus another
  client's windows. The entire Mission B/C operation set is impossible.
- **`xdg-foreign` is not embedding.** It exports/imports *toplevel handles*
  for focus exchange and transient-parent relationships between COOPERATING
  apps — both sides must opt in, and it confers no content hosting.
- **No title/state observation.** No `PropertyNotify` on foreign windows, so
  the title-tracking tab sensor has no equivalent. AT-SPI still exists under
  Wayland (AT-SPI is D-Bus, display-protocol independent) — tab *detection*
  likely still works there, but no surface mechanism can act on it.

## GNOME/Mutter specifics

- Mutter offers no privileged embedding API to ordinary clients. The paths
  that CAN overlay foreign content (GNOME Shell extensions, which run inside
  the compositor process; screencast portals, which are one-way pixel pipes
  with user consent) are not available to a CLI/daemon and do not provide
  interactive input into the overlaid surface.
- An XWayland reparenting hack is not native Wayland integration and inherits
  all of Mission 1's verdicts plus XWayland's own constraints. Rejected
  explicitly per the mission brief.

## What COULD work on Wayland (different product shape, not proven here)

1. **GNOME Shell extension + daemon**: an extension (compositor-privileged)
   could position/track surfaces, but interactive input into a foreign
   surface remains unsupported — at best an overlay viewer, not a browser tab.
2. **Kitty-protocol host terminal**: Kitty/Ghostty/WezTerm run natively on
   Wayland AND implement the graphics protocol — the screencast-browser-tab
   architecture (outcome B of the overall investigation) is Wayland-viable
   because rendering goes through the terminal's own surfaces and input
   through terminal protocols. This is the portable path.
3. **VS Code**: Electron/Chromium host with official webview API, Wayland Indie; same companion-extension shape as on X11.

## Verdict: NOT POSSIBLE (Wayland-native external-surface integration)

On Wayland, every mechanism Mission 2 relies on (foreign control, foreign
observation via X properties, reparenting) is absent by protocol design.
Tab *detection* via AT-SPI is the one sensor expected to survive; everything
else must move to a cooperating host (Kitty-protocol terminal, VS Code) —
which is also the cross-platform story, since none of the X11 mechanisms
exist on macOS/Windows either.
