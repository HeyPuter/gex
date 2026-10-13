#!/bin/bash
# The window placement leg (gex_window_placement_v1): placetest.exe makes
# windows at known Win32 positions under W3 Wine on the native compositor
# (gexc --wayland-gpu), and this checks that the screen agrees -- placement
# (the compositor's window list and the pixels), stacking (a topmost window
# above later normal ones, a clicked window raised by Win32), and
# click-through (each scripted click reaches the window under it, by the
# test's own log). Correctness only: no timing is read from it.
#
# A watchdog (GEX_STALL_FRAME) dumps every guest thread's state and syscall
# when no frame arrives for STALL_MS, so a boot that stalls is caught in the act.
#
# usage: placement-leg.sh [TAG]
#   env: WDBG (WINEDEBUG), GEXC (gexc built with --features x86), DIST (a W3 dist with the
#        explorer fork's loader), PFX (a prefix to copy), OUT (runs dir),
#        SECS (deadline, default 400), STALL_MS (default 30000), PLACETEST (a prebuilt
#        placetest.exe); on edger: BENCH_CLASS=check scratch/bench/bench.sh -- ... (BENCH_CORES)
set -u
. ~/gex-sandbox/scratch/env.sh
here=$(cd "$(dirname "$0")" && pwd)
tag=${1:-place}
X=~/gex-sandbox/scratch/X
G=${GEXC:-$here/../../target/x86/release/gexc}
DIST=${DIST:-$X/dist-dev}
PFX=${PFX:-$X/pfx-mb}
OUT=${OUT:-$X/runs}
SECS=${SECS:-400}
out=$OUT/$tag; pfx=$OUT/pfx-$tag
rm -rf "$out" "$pfx"; mkdir -p "$out/frames"
cp -a "$PFX" "$pfx"
# placetest.exe: PLACETEST (prebuilt, for a box without MinGW), or built here
if [ -n "${PLACETEST:-}" ]; then cp "$PLACETEST" "$pfx/drive_c/placetest.exe" || exit 1
else TMPDIR=${TMPDIR:-$X/tmp} i686-w64-mingw32-gcc -O2 -o "$pfx/drive_c/placetest.exe" "$here/placetest.c" -mwindows || exit 1; fi
[ -e "$pfx/drive_c/winlogon.exe" ] || cp $X/tests/winlogon.exe "$pfx/drive_c/"
rm -f "$pfx/drive_c/placetest.log"
# Clicks, from the first frame: A alone (raises it over B), C where it is
# above B (topmost), B alone, the taskbar strip D.
cat > "$out/input" <<IN
frames 1
20 click 110 110
24 click 300 180
28 click 350 280
32 click 500 750
IN
cd "$pfx/drive_c"
GEX_WASM_BACKTRACE=1 GEX_PPM_EVERY=1 GEX_WL_INPUT=$out/input GEX_STALL_FRAME=${STALL_MS:-30000} \
  systemd-run --user --scope -q -p TasksMax=256 -p MemoryMax=32G ${BENCH_CORES:+taskset -c $BENCH_CORES} nice -n 10 "$G" run \
  --env WINEPREFIX="$pfx" --env WINEDEBUG=${WDBG:-err+all,fixme-all} \
  --gl --wayland-gpu --wayland "$out/wl" --wayland-size 1024x768 --wayland-out "$out/frames/f" \
  "$DIST/lib/wine/i386-unix/wine" 'C:\winlogon.exe' 'C:\placetest.exe' > "$out/log" 2>&1 &
pid=$!
start=$SECONDS
while kill -0 $pid 2>/dev/null; do
  if [ $((SECONDS - start)) -ge $SECS ]; then echo "placement-leg: deadline, stopping $pid"; break; fi
  if [ "$(grep -c '^click' "$pfx/drive_c/placetest.log" 2>/dev/null)" = 4 ]; then sleep 4; break; fi
  sleep 2
done
kill $pid 2>/dev/null; sleep 2; kill -9 $pid 2>/dev/null; wait $pid 2>/dev/null
cp "$pfx/drive_c/placetest.log" "$out/" 2>/dev/null
python3 - "$out" <<'PY'
import os, re, sys
out = sys.argv[1]
fails = []
def check(ok, what):
    print(("ok   " if ok else "FAIL ") + what)
    if not ok: fails.append(what)
log = open(f"{out}/log", errors="replace").read()
tlog = open(f"{out}/placetest.log").read() if os.path.exists(f"{out}/placetest.log") else ""
stalls = log.count("[gex-stall]")
if stalls: print(f"note: {stalls} stall dump(s) in {out}/log (grep gex-stall)")
# The window list: the last one with all four windows.
lists = [l for l in log.splitlines() if l.startswith("[wl-place]")]
full = [l for l in lists if len(re.findall(r" id=", l)) >= 4]
check(bool(full), "the compositor listed the four windows")
if full:
    wins = re.findall(r"id=(0x[0-9a-f]+) (\d+)x(\d+)\+(-?\d+)\+(-?\d+) layer=(\d)", full[0])
    geo = [(int(w), int(h), int(x), int(y), int(l)) for _, w, h, x, y, l in wins]
    want = {"A": (200, 150, 100, 100, 0), "B": (200, 150, 200, 150, 0),
            "C": (120, 80, 250, 120, 1), "D": (1024, 40, 0, 728, 1)}
    for n, g in want.items():
        check(g in geo, f"window {n} at {g[2]},{g[3]} {g[0]}x{g[1]} layer {g[4]}")
    order = [g for g in geo]
    if all(g in geo for g in want.values()):
        check(order.index(want["A"]) < order.index(want["B"]), "B above A as first listed (made later)")
        check(order.index(want["B"]) < order.index(want["C"]), "C (topmost, made first) above B")
clicks = re.findall(r"click (\w)", tlog)
check(clicks == ["A", "C", "B", "D"], f"clicks reached A, C, B, D (got {clicks})")
# Pixels, frame by frame.
def ppm(p):
    d = open(p, "rb").read()
    parts = d.split(b"\n", 3)
    w, h = map(int, parts[1].split())
    return w, h, parts[3]
def px(f, x, y):
    w, h, b = f
    o = (y * w + x) * 3
    return tuple(b[o:o + 3])
frames = sorted(f for f in os.listdir(f"{out}/frames") if f.endswith(".ppm"))
R, G, B, Y, W = (255, 0, 0), (0, 255, 0), (0, 0, 255), (255, 255, 0), (255, 255, 255)
seen_initial = seen_raised = False
last = None
for name in frames:
    f = ppm(f"{out}/frames/{name}")
    if f[0] != 1024: continue
    p = {k: px(f, *xy) for k, xy in {"a": (150, 120), "ab": (250, 200), "b": (350, 280), "c": (300, 180), "d": (500, 750)}.items()}
    if p == {"a": R, "ab": G, "b": G, "c": B, "d": Y}: seen_initial = True
    if p["a"] == W and p["b"] == G and p["ab"] == W: seen_raised = True
    last = (name, p)
check(seen_initial, "a frame shows A, B over A, C over B, D at the bottom")
check(seen_raised, "a frame shows A, clicked, raised over B")
if last:
    check(last[1] == {"a": W, "ab": W, "b": W, "c": W, "d": W}, f"the last frame ({last[0]}) has all four clicked: {last[1]}")
print("placement leg:", "PASS" if not fails else f"FAIL ({len(fails)})")
sys.exit(1 if fails else 0)
PY
