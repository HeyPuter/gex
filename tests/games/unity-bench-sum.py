#!/usr/bin/env python3
"""Summarise a tests/games/unity-bench.sh run: start-up milestones and frame rate.

    unity-bench-sum.py RUN [--window SKIP:LEN]

Milestones are seconds from the run's start: the first PRESENT the game made
(GEX_GL_FPS), and the first player-log line matching each marker. The frame rate
is the game's own PRESENTs per second, averaged over a window that starts SKIP
seconds (default 20) after the last milestone reached and lasts LEN seconds
(default 60); the run's last quarter is given too.
"""
import re
import sys

MARKERS = [
    ("reload begin", r"Begin MonoManager ReloadAssembly"),
    ("reload done", r"Completed reload"),
    ("first scene unload", r"Unloading \d+ Unused Serialized files"),
    ("HK Menu_Title start", r"Performing automatic level start"),
    ("Muck packets", r"Initializing packets"),
]


def main():
    run = sys.argv[1]
    skip, length = 20.0, 60.0
    if "--window" in sys.argv:
        a, b = sys.argv[sys.argv.index("--window") + 1].split(":")
        skip, length = float(a), float(b)
    meta = open(f"{run}/meta.txt").read()
    t0 = int(re.search(r"start (\d+)", meta).group(1))
    out = open(f"{run}/out.txt", errors="replace").read().splitlines()
    first = None
    fps = []  # (seconds from start, presents/s, total)
    shares = []
    for line in out:
        m = re.search(r"\[gex-gl\] first present at (\d+) ms", line)
        if m:
            first = (int(m.group(1)) - t0) / 1000
        m = re.search(r"\[gex-gl\] presents/s ([\d.]+) total (\d+) at (\d+) ms", line)
        if m:
            fps.append(((int(m.group(3)) - t0) / 1000, float(m.group(1)), int(m.group(2))))
        if "[gex-prof] AOT" in line:
            shares.append(line.split("[gex-prof] ", 1)[1])
    marks = {}
    try:
        plog = open(f"{run}/plog-t.txt", errors="replace").read().splitlines()
    except FileNotFoundError:
        plog = []
    menu_total = None
    for line in plog:
        ms, _, text = line.partition(" ")
        if not ms.isdigit():
            continue
        t = int(ms) / 1000
        for name, rx in MARKERS:
            if name not in marks and re.search(rx, text):
                marks[name] = t
        # Hollow Knight: the unload that follows Menu_Title's load; the menu is up after it.
        if "HK Menu_Title start" in marks and menu_total is None and text.startswith("Total:"):
            menu_total = t
            marks["HK menu ready (post-unload)"] = t
            m = re.search(r"MarkObjects: ([\d.]+) ms", text)
            if m:
                marks[f"  (that unload's MarkObjects {float(m.group(1)) / 1000:.1f} s)"] = t
    print(f"run {run}")
    print(meta.strip())
    print(f"first present: {first:.1f} s" if first is not None else "first present: none")
    for name, t in sorted(marks.items(), key=lambda x: x[1]):
        print(f"{name}: {t:.1f} s")
    unloads = [line.partition(" ")[2] for line in plog if "MarkObjects" in line]
    for u in unloads:
        print("unload:", u.strip()[:160])
    if fps:
        anchor = max(marks.values()) if marks else fps[0][0]
        w = [f for f in fps if anchor + skip <= f[0] < anchor + skip + length]
        if w:
            frames = w[-1][2] - w[0][2]
            secs = w[-1][0] - w[0][0]
            print(f"fps window {anchor + skip:.0f}-{anchor + skip + length:.0f} s: "
                  f"{frames / secs if secs else 0:.2f} presents/s ({frames} presents in {secs:.1f} s; "
                  f"per-second min {min(x[1] for x in w):.1f} max {max(x[1] for x in w):.1f})")
        else:
            print(f"fps window {anchor + skip:.0f}-{anchor + skip + length:.0f} s: no samples (run too short)")
        q = fps[len(fps) * 3 // 4:]
        if len(q) > 1:
            frames, secs = q[-1][2] - q[0][2], q[-1][0] - q[0][0]
            print(f"last quarter {q[0][0]:.0f}-{q[-1][0]:.0f} s: {frames / secs:.2f} presents/s")
        print(f"presents total {fps[-1][2]} by {fps[-1][0]:.0f} s")
    for s in shares[-3:]:
        print("share:", s)


main()
