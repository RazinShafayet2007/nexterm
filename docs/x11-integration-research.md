# X11 Integration Research (Mission 2, experimental, 2026-09-23)

Session: X11 (`XDG_SESSION_TYPE=x11`, `:0`, Xorg + Mutter/GNOME Shell 42.9).
Terminal: GNOME Terminal 3.44 / VTE 0.68 / GTK 3.24, single-X-window toplevel
(Mission 1 tree walk). Prototypes: `research/x11-browser-controller/`,
`research/browser-tab-companion-poc/`, `research/gnome-embed-poc/`.

## What X11 permits against an unmodified foreign window (all verified live)

| Operation | Mechanism | Evidence |
|---|---|---|
| Locate + inspect | `XQueryTree`, `XGetGeometry`, `WM_CLASS`, `_NET_WM_NAME`, `_NET_WM_STATE`, `_NET_FRAME_EXTENTS` | `locate`: toplevel `0x320000a`, title, `MAXH+MAXV`, extents `(0,0,0,0)` (maximized) |
| Track move/resize | `XSelectInput(STRUCTURE_NOTIFY)` → `ConfigureNotify` | `track`/`follow`: re-glue logged on every configure |
| Track title (tab switches!) | `XSelectInput(PROPERTY_CHANGE)` → `PropertyNotify(_NET_WM_NAME)` | companion: every tab switch logged with old→new title |
| Track minimize/restore | `PropertyNotify(_NET_WM_STATE)` → `HIDDEN` atom | controller + companion: hide/show browser in sync |
| Detect new windows | `SubstructureNotify` on root + `CreateNotify` | companion adopted new terminals mid-run (`terms=2`) |
| Move/resize foreign window | `ConfigureWindow` on terminal toplevel | terminal moved+shrunk and restored; browser re-glued |
| Minimize/restore foreign window | `ClientMessage(_NET_WM_STATE)` to root | terminal minimized 3–4s, restored; verified in logs |
| Focus any window | `XSetInputFocus` + `GetInputFocus` verify | focus moved poc↔terminal, round-trip logged |
| Show/hide own surface | `Map/UnmapWindow` | browser shown/hidden per association rule |
| Stacking | `ConfigureWindow(stack_mode ABOVE/BELOW)` | raise/lower cycled without errors |
| Embed into toplevel | `XReparentWindow` into terminal top-level | Mission 1 + companion: `VIEWABLE`, input works |
| Embed into inner child | `XReparentWindow` into 1x1 helper | **BadMatch** (InputOnly) — no container target exists |

None of the above required any cooperation from GNOME Terminal. All foreign
ops were transient and restored (geometry, minimized state, focus).

## Overlay (Mission C) vs reparenting (Mission D) — not the same thing

- **Overlay** (controller `follow`): independent top-level positioned over the
  content area, re-glued on `ConfigureNotify`, hidden on `HIDDEN`. Proven to
  track. Remaining separateness: taskbar/switcher entry, own decorations
  (we run undecorated), no clipping to the tab strip, visible during tab
  switches unless hidden by the rule.
- **Reparenting** (companion rule): `XReparentWindow(ours → terminal top,
  content offset)` on show. Then: no taskbar entry, minimizes/restores/dies
  with the terminal automatically, cannot be "behind" the terminal. Proven
  across 4 runs. Still not a tab (Mission 1 §D): visible across all tabs
  unless hidden — hence the companion's show/hide rule, which is what makes
  it *behave* like tab content.

## Known approximations (do not hide these)

1. **Content rectangle: SOLVED by measurement.** The 43px chrome constant is
   retired: `research/browser-tab-companion-poc/rect.py` reads the exact VTE
   content rect per frame from AT-SPI (`terminal` object extents, desktop
   coords). Load-bearing, not cosmetic — with 2 tabs the tab bar pushes
   content to y=88 (logged `offset (0,88 1284x653) [measured]`), with 1 tab
   it sits at y=74; any constant is wrong in one of the two states. The
   companion logs `[measured]` vs `[estimate]` per placement, falling back
   to the geometric estimate only if the helper yields nothing.
2. **Title tracking has an ambiguity**: two tabs with identical titles are
   indistinguishable via `_NET_WM_NAME` (AT-SPI index, see
   `tab-detection-research.md`, disambiguates).
3. **Focus is asserted, not negotiated**: `XSetInputFocus` works but fights
   the WM's model; correct behavior was verified, elegance was not.
4. **Latency/artifacts not instrumented**: re-glue happens on the next event
   poll (Poll loop, ~ms); no tearing/flicker measurement was made. Visual
   judgment was the user's ("inside the terminal, not a tab").

## Verdict matrix (all Mission 2 approaches)

| Approach | Works? | Evidence | Stability | UX quality |
|---|---|---|---|---|
| GTK child embedding | NOT POSSIBLE | Mission 1: no API at D-Bus/VTE/GTK layers (live introspection) | — | — |
| X11 overlay | PROVEN | controller `follow`/`cycle`: track, glue, hide/show, focus, minimize-sync | PROTOTYPE VIABLE | separate window, glued |
| X11 reparenting (toplevel) | PROVEN | Mission 1 input proofs + companion 4 runs (show/hide/focus/follow) | FRAGILE | inside window, not a tab |
| XEmbed | NOT POSSIBLE | needs host-created `GtkSocket`; tree shows no socket, terminal creates none | — | — |
| X11 top-level integration | PROVEN | controller: locate/track/move/hide/show/raise/focus/minimize-foreign | PROTOTYPE VIABLE | full external control |
| Active-tab detection | PROVEN | `tab-detection-research.md`: AT-SPI selection + title transitions 1→2→1 | PROTOTYPE VIABLE | index + title |
| Browser-tab companion | PROTOTYPE VIABLE | 4 runs: association rule, stress phases, input events in-log, dispatch chain | FRAGILE | tab-like, with listed approximations |
| Wayland equivalent | NOT POSSIBLE | `wayland-integration-research.md` (design-level; no local session) | — | — |

Status meanings: PROVEN = demonstrated end-to-end live; PROTOTYPE VIABLE =
works with documented approximations; FRAGILE = works but not shippable
without solving listed issues; BLOCKED = path exists but needs something
absent; NOT POSSIBLE = ruled out by evidence.
