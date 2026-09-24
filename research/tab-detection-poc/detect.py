#!/usr/bin/env python3
"""tab-detection-poc — DISPOSABLE (Mission E, not part of NexTerm).

Reads GNOME Terminal tab state as a normal user-space observer via AT-SPI:
per terminal frame -> tab count + selected tab index (+ frame title).

Usage:
  ./detect.py snapshot          # print current model once
  ./detect.py watch [secs]      # poll and report changes (tab switches show up)

Requires: python3-pyatspi (present on this host), running at-spi bus.
Pure observation: opens/closes nothing, modifies nothing.
"""
import sys
import time

import pyatspi


def find_by_role(root, role, out):
    try:
        if root.getRoleName() == role:
            out.append(root)
        for i in range(root.childCount):
            find_by_role(root.getChildAtIndex(i), role, out)
    except Exception:
        pass
    return out


def terminal_frames():
    d = pyatspi.Registry.getDesktop(0)
    apps = find_by_role(d, "application", [])
    terms = [a for a in apps if getattr(a, "name", "") == "gnome-terminal-server"]
    frames = []
    for t in terms:
        frames.extend(find_by_role(t, "frame", []))
    return frames


def model():
    """{frame_title: (tab_count, selected_index)}"""
    m = {}
    for f in terminal_frames():
        try:
            title = f.name
        except Exception:
            continue
        ptls = find_by_role(f, "page tab list", [])
        if not ptls:
            m[title] = (0, -1)
            continue
        # A frame normally has exactly one page tab list.
        ptl = ptls[0]
        try:
            sel = ptl.querySelection()
            n = ptl.childCount
            s = sel.nSelectedChildren
            idx = -1
            if s > 0:
                kid = sel.getSelectedChild(0)
                for i in range(n):
                    if ptl.getChildAtIndex(i) == kid:
                        idx = i
                        break
            m[title] = (n, idx)
        except Exception as e:
            m[title] = (f"err:{e}", -1)
    return m


def main():
    if len(sys.argv) < 2 or sys.argv[1] not in ("snapshot", "watch"):
        print("usage: detect.py <snapshot|watch [secs]>")
        return 2
    if sys.argv[1] == "snapshot":
        for title, (n, idx) in model().items():
            print(f"frame={title!r} tabs={n} selected={idx}")
        return 0
    secs = float(sys.argv[2]) if len(sys.argv) > 2 else 20
    print(f"[poc] watching tab state for {secs}s — switch terminal tabs now", flush=True)
    last = model()
    print(f"[poc] t=0 {last}", flush=True)
    t0 = time.time()
    while time.time() - t0 < secs:
        time.sleep(0.4)
        cur = model()
        if cur != last:
            print(f"[poc] t={time.time()-t0:.1f}s {cur}", flush=True)
            last = cur
    print("[poc] watch done", flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
