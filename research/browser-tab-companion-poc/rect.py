#!/usr/bin/env python3
"""rect.py — DISPOSABLE helper (Mission 2, not part of NexTerm).

Prints one line per terminal frame that has a SHOWING terminal object:
    <frame title>\\t<x>\\t<y>\\t<w>\\t<h>
Coords are desktop (screen) pixels from AT-SPI Component extents — the EXACT
VTE content rectangle (position and size), including tab-bar shifts. Hidden
tabs report zombie coords and are excluded via STATE_SHOWING.
"""
import pyatspi


def find(o, role, out):
    try:
        if o.getRoleName() == role:
            out.append(o)
        for i in range(o.childCount):
            find(o.getChildAtIndex(i), role, out)
    except Exception:
        pass
    return out


def main():
    d = pyatspi.Registry.getDesktop(0)
    apps = find(d, "application", [])
    for app in apps:
        try:
            if app.name != "gnome-terminal-server":
                continue
        except Exception:
            continue
        for frame in find(app, "frame", []):
            try:
                title = frame.name or ""
            except Exception:
                continue
            for term in find(frame, "terminal", []):
                try:
                    st = term.getState()
                    if not st.contains(pyatspi.STATE_SHOWING):
                        continue
                    x, y, w, h = term.queryComponent().getExtents(pyatspi.DESKTOP_COORDS)
                except Exception:
                    continue
                print(f"{title}\t{x}\t{y}\t{w}\t{h}", flush=True)


if __name__ == "__main__":
    main()
