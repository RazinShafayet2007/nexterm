#!/usr/bin/env python3
"""How the idle-cost numbers in `docs/production-readiness.md` were produced.

"Idle daemon" is not one process. A daemon that has never opened a surface is
just `nexterm-daemon`; once a browser surface has been opened *at least once*, a
shared WebKit network process stays resident as a child of the daemon until
`nexterm stop`, even with no sessions open. This script reports per-process CPU
and the resident process set, so both halves of that claim can be re-checked on
any host instead of taken on faith.

Usage:
    python3 scripts/idle-cost.py [seconds] [--session-note]

Sampling is from `/proc/<pid>/stat` (utime+stime deltas), so it needs no root
and does not depend on `perf` (which is often unusable: `perf_event_paranoid`).
"""
import os
import sys
import time

CLK = os.sysconf("SC_CLK_TCK")
# argv[0] basenames that make up (or belong to) a running NexTerm daemon.
WATCHED = ("nexterm-daemon", "WebKitNetworkProcess", "WebKitWebProcess")


def snapshots():
    """{pid: (name, cpu_ticks, rss_kb)} for every process we care about."""
    out = {}
    for entry in os.listdir("/proc"):
        if not entry.isdigit():
            continue
        try:
            with open(f"/proc/{entry}/cmdline", "rb") as fh:
                argv0 = fh.read().split(b"\0", 1)[0].decode("utf-8", "replace")
            with open(f"/proc/{entry}/stat") as fh:
                stat = fh.read()
        except OSError:
            continue
        name = os.path.basename(argv0)
        if name not in WATCHED:
            continue
        # Field 2 is "(comm)" and may contain spaces, so split after it.
        fields = stat.rsplit(")", 1)[1].split()
        ticks = int(fields[11]) + int(fields[12])  # utime + stime
        rss_pages = int(fields[21])                # rss
        page_kb = os.sysconf("SC_PAGE_SIZE") // 1024
        out[entry] = (name, ticks, rss_pages * page_kb)  # KiB
    return out


def main():
    argv = [a for a in sys.argv[1:] if not a.startswith("--")]
    seconds = float(argv[0]) if argv else 30.0

    before = snapshots()
    if not before:
        print("no nexterm-daemon / WebKit processes found -- is the daemon stopped?")
        print("(that is itself a data point: no daemon = nothing resident)")
        return 0
    print(f"resident before: {len(before)} process(es); sampling {seconds:g}s ...")
    time.sleep(seconds)
    after = snapshots()

    width = max((len(n) for n, _, _ in before.values()), default=8)
    print(f"\n  {'process':<{width}}  {'pid':>6}  {'cpu% of 1 core':>14}  rss     parent")
    total = 0.0
    for pid, (name, t0, _) in sorted(before.items(), key=lambda kv: kv[1][0]):
        if pid not in after:
            print(f"  {name:<{width}}  {pid:>6}  {'exited during sample':>14}")
            continue
        _, t1, rss = after[pid]
        pct = 100.0 * (t1 - t0) / (CLK * seconds)
        total += pct
        rss_mb = rss / 1024
        try:
            with open(f"/proc/{pid}/status") as fh:
                ppid = next(
                    l.split()[1] for l in fh if l.startswith("PPid:")
                )
        except OSError:
            ppid = "?"
        print(f"  {name:<{width}}  {pid:>6}  {pct:>13.2f}%  {rss_mb:>6.0f}MB  {ppid:>6}")
    print(f"\n  TOTAL idle: {total:.2f}% of one core over {seconds:g}s")
    if not any(n.startswith("WebKit") for n, _, _ in before.values()):
        print("  (no WebKit processes: this daemon has not opened a surface yet)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
