# Tab Detection Research (Mission E, experimental, 2026-09-23)

Prototype: `research/tab-detection-poc/` (`detect.py` + `run.sh`).
No windows opened by the detector itself; the driver opens/closes one
ordinary shell tab and removes it.

## Two independent sensors (both proven live)

### 1. AT-SPI selection — robust active-tab INDEX

- `org.a11y.Bus` on session bus; `python3-pyatspi` present; desktop has 16 apps.
- `gnome-terminal-server` (role `application`) → `frame` (window title) →
  `page tab list` → N `page tab` children.
- `page tab list.querySelection()`: `nSelectedChildren` + `getSelectedChild`
  give the active tab. Verified: `nChildren=1 nSelected=1` at baseline.
- Per-frame: multiple terminal windows → multiple frames, each with its own
  tab list. Multi-window safe.
- Tab names/descriptions are EMPTY strings — identity must come from index +
  title correlation, not from the tab objects themselves.

### 2. `_NET_WM_NAME` title tracking — WHAT the active tab is

- The terminal toplevel's title always mirrors the active tab's title
  (observed via X `PropertyNotify`, also visible in AT-SPI frame names).
- A placeholder tab with a **stable marker title** (`--title=` + `bash
  --norc --noprofile` + OSC-0 title set, so the shell prompt never rewrites
  it) makes "our tab active" observable as `title == marker`.
- Opening the placeholder autofocuses it; killing its unique sleep
  (`exec -a NEXTERM-… sleep 300`, closed via `pkill -f`) auto-closes the tab
  under the default profile — open AND close are fully scriptable.

### Measured transitions (`run.sh` output, no user interaction)

```text
frame='OC | NexTerm…' tabs=1 selected=0
→ open placeholder
frame='NEXTERM-PROBE-TAB-…' tabs=2 selected=1   # count, index AND title all move
→ kill placeholder sleep
frame='OC | NexTerm…' tabs=1 selected=0
```

The companion PoC then used title tracking as its live association sensor:
every manual and scripted tab switch in 4 runs produced a logged title
transition and a correct show/hide decision (see `x11-integration-research.md`).

## Limits (do not hide these)

1. **Duplicate titles**: two tabs with byte-identical titles are one
   observation. Marker titles are unique per browser instance, so OUR tab is
   unambiguous; other tabs may still collide with each other (irrelevant to
   the rule, which only tests equality with our marker).
2. **Prompt rewrites**: ordinary shell tabs rewrite titles constantly
   (`user@host: dir`). Harmless — only inequality with the marker matters.
3. **Polling vs events**: AT-SPI here is polled (0.4s); X property events are
   push. A product should use push (PropertyNotify) with AT-SPI as the
   disambiguator, exactly as the companion does.
4. **No programmatic tab switching exists** (that is the point): detection is
   observation-only. Association is one-directional (tab state → browser
   visibility), which is all the companion architecture needs.

## Verdict: PROVEN (prototype viable)

Active-tab state — count, index, and identity-via-marker-title — is
observable by an unprivileged user-space process through public, stable
interfaces. No modification of GNOME Terminal involved.
