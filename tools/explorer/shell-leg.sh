#!/bin/bash
# The shell leg: Windows 7's explorer.exe as the session's shell (stage.sh
# with SHELL_KIND=windows), started by our winlogon under W3 Wine on the
# native compositor (gexc --wayland-gpu, windows placed by Win32). It checks
# that the taskbar comes up, where Win32 put it (Basic's 40-pixel bar at the
# bottom; the classic one is 54 pixels), and that it is drawn with the Basic
# theme -- Windows 7 Basic's flat blue (167,192,220 under a dark top line),
# not the classic grey (240,240,240) -- in the first frame that shows its
# background drawn and in every frame after. (Before its first WM_PAINT a
# window's surface is Wine's fresh white, as Windows shows a window erased
# but not yet painted; those frames are counted, not failed.) On Windows the Themes service has the
# theme loaded before winlogon starts the shell, so the shell never draws
# unthemed; a classic frame here is a boot-order or theme-change bug. The
# last frame must show the Basic theme's wallpaper (img0.jpg, filled).
# Correctness only: no timing is read from it (run it on edger with
# BENCH_CLASS=check, not on gooner).
#
# usage: shell-leg.sh [TAG]
#   env: WDBG (WINEDEBUG), GEXC (gexc built with --features x86), DIST (a W3 dist with the
#        explorer fork's loader), PFX (a prefix staged with SHELL_KIND=windows), WINLOGON
#        (winlogon.exe to use), OUT (runs dir), SECS (deadline, default 400), SETTLE (s to
#        keep running once the taskbar shows, default 30), STALL_MS (default 60000),
#        INPUT (a GEX_WL_INPUT script, e.g. clicks on the Start button), TASKS (TasksMax),
#        START=1 (click the Start button 60 s after the first frame; the Start menu,
#        Basic's, 491 wide and standing on the taskbar at 0,728 (its height is its
#        right column's: 485 for a workstation's), must open; start.png shows it),
#        APPS=1 (two startup programs, taskone and tasktwo, from HKCU's Run key: both
#        must start, the taskbar must show a button for each after its two pinned
#        programs', and a click on Task One's button 150 s after the first frame must
#        activate it, and each button must show its program's icon (the program's
#        process made it); buttons.png shows the taskbar's buttons; with USERINIT=1, as the
#        shell runs startup programs only in a session userinit set up)
set -u
. ~/gex-sandbox/scratch/env.sh
here=$(cd "$(dirname "$0")" && pwd)
tag=${1:-shell}
X=~/gex-sandbox/scratch/X
G=${GEXC:-$here/../../target/x86/release/gexc}
DIST=${DIST:-$X/dist-sh6}
PFX=${PFX:-$X/pfx-sh6}
OUT=${OUT:-$X/runs}
# (APPS=1 runs until its click, 150 s after the first frame, has been answered)
if [ "${APPS:-0}" = 1 ]; then SECS=${SECS:-560} SETTLE=${SETTLE:-200}; fi
SECS=${SECS:-400}
SETTLE=${SETTLE:-30}
out=$OUT/$tag; pfx=$OUT/pfx-$tag
chmod -R u+w "$pfx" 2>/dev/null; rm -rf "$out" "$pfx"; mkdir -p "$out/frames"
cp -a "$PFX" "$pfx"
cp "${WINLOGON:-$X/tests/winlogon.exe}" "$pfx/drive_c/winlogon.exe" || exit 1
if [ "${APPS:-0}" = 1 ]; then
  # two programs the shell starts from HKCU's Run key, as a user's startup
  # programs: taskapp.c built as taskone.exe and tasktwo.exe (TASKAPP: a
  # prebuilt taskapp.exe, for a box without MinGW)
  if [ -n "${TASKAPP:-}" ]; then cp "$TASKAPP" "$pfx/drive_c/taskone.exe" || exit 1
  else TMPDIR=${TMPDIR:-$X/tmp} i686-w64-mingw32-gcc -O2 -o "$pfx/drive_c/taskone.exe" "$here/taskapp.c" -mwindows || exit 1; fi
  cp "$pfx/drive_c/taskone.exe" "$pfx/drive_c/tasktwo.exe"
  python3 -I "$here/user-reg.py" "$pfx/user.reg" 'Software\Microsoft\Windows\CurrentVersion\Run' \
    'TaskOne=C:\\taskone.exe' 'TaskTwo=C:\\tasktwo.exe' || exit 1
fi
if [ "${START:-0}" = 1 ] && [ -z "${INPUT:-}" ]; then
  INPUT=$out/input; printf 'frames 1\n60 click 27 748\n' > "$INPUT"
elif [ "${APPS:-0}" = 1 ] && [ -z "${INPUT:-}" ]; then
  # Task One's button: the third, after Internet Explorer's and Windows Explorer's pins
  INPUT=$out/input; printf 'frames 1\n150 click 210 748\n' > "$INPUT"
fi
# USERINIT=1: winlogon starts Windows 7's userinit.exe (staged in the
# prefix), which starts the shell Winlogon's Shell names, as on Windows
logon='C:\Windows\explorer.exe'; [ "${USERINIT:-0}" = 1 ] && logon='C:\Windows\system32\userinit.exe'
cd "$pfx/drive_c"
env GEX_PPM_EVERY=1 GEX_STALL_FRAME=${STALL_MS:-60000} ${INPUT:+GEX_WL_INPUT=$INPUT} \
  systemd-run --user --scope -q -p TasksMax=${TASKS:-256} -p MemoryMax=32G \
  ${BENCH_CORES:+taskset -c $BENCH_CORES} nice -n 10 "$G" run \
  --env WINEPREFIX="$pfx" --env WINEDEBUG=${WDBG:-err+all,fixme-all} --env WINLOGON_VERBOSE=1 \
  --gl --wayland-gpu --wayland "$out/wl" --wayland-size 1024x768 --wayland-out "$out/frames/f" \
  "$DIST/lib/wine/i386-unix/wine" 'C:\winlogon.exe' "$logon" > "$out/log" 2>&1 &
pid=$!
start=$SECONDS seen=
while kill -0 $pid 2>/dev/null; do
  if [ $((SECONDS - start)) -ge $SECS ]; then echo "shell-leg: deadline, stopping $pid"; break; fi
  if [ -z "$seen" ] && grep -aq '^\[wl-place\].* 1024x40+0+728 ' "$out/log"; then seen=$SECONDS; fi
  if [ -n "$seen" ] && [ $((SECONDS - seen)) -ge $SETTLE ]; then break; fi
  sleep 2
done
kill $pid 2>/dev/null; sleep 2; kill -9 $pid 2>/dev/null; wait $pid 2>/dev/null
START=${START:-0} APPS=${APPS:-0} python3 -I - "$out" "$X/bin/ppm2png.py" "$pfx/drive_c/taskapp.log" <<'PY'
import os, re, subprocess, sys
out, topng = sys.argv[1], sys.argv[2]
fails = []
def check(ok, what):
    print(("ok   " if ok else "FAIL ") + what)
    if not ok: fails.append(what)
log = open(f"{out}/log", errors="replace").read()
stalls = log.count("[gex-stall]")
if stalls: print(f"note: {stalls} stall dump(s) in {out}/log (grep gex-stall)")
order = [l for l in log.splitlines() if l.startswith("winlogon: ")]
print("winlogon:", " | ".join(l[10:] for l in order))
logon = next((i for i, l in enumerate(order) if "ThemesOnLogon(1)" in l), None)
shell = next((i for i, l in enumerate(order) if "starting the shell" in l), None)
check(logon is not None and shell is not None and logon < shell,
      "winlogon loads the theme (ThemesOnLogon) before the shell runs")
lists = [l for l in log.splitlines() if l.startswith("[wl-place]")]
bars = {m for l in lists for m in re.findall(r"id=0x[0-9a-f]+ (\d+x\d+\+-?\d+\+-?\d+) layer=1", l)}
check("1024x40+0+728" in bars, f"the taskbar is placed at 0,728 1024x40, Basic's (topmost windows seen: {sorted(bars)})")
check("1028x54+-2+716" not in bars, "no classic-sized taskbar (1028x54 at -2,716)")
def ppm(p):
    d = open(p, "rb").read()
    m = re.match(rb"P6\s+(\d+)\s+(\d+)\s+255\s", d)
    return int(m[1]), int(m[2]), d[m.end():]
def px(f, x, y):
    w, h, b = f
    o = (y * w + x) * 3
    return tuple(b[o:o + 3])
# The taskbar's empty middle, away from the Start button, the buttons and the tray.
xs, y = (500, 650, 800), 740
def basic(p): return p[2] >= 180 and p[2] - p[0] >= 30
def unpainted(p): return p in ((0, 0, 0), (255, 255, 255))
frames = sorted(f for f in os.listdir(f"{out}/frames") if f.endswith(".ppm"))
first = None; classic = []; shown = 0; fresh = 0
for name in frames:
    f = ppm(f"{out}/frames/{name}")
    if f[0] != 1024 or f[1] != 768: continue
    p = [px(f, x, y) for x in xs]
    if all(unpainted(q) for q in p):
        if first is None and p[0] == (255, 255, 255): fresh += 1
        continue
    shown += 1
    if first is None: first = (name, p)
    if not all(basic(q) for q in p): classic.append((name, p))
if fresh: print(f"note: {fresh} frame(s) before the taskbar's first paint show its fresh (white) surface")
check(first is not None, f"a frame shows the taskbar drawn ({shown} of {len(frames)} frames)")
if first:
    check(all(basic(q) for q in first[1]), f"the first taskbar frame ({first[0]}) is Basic blue: {first[1]}")
    check(not classic, f"every taskbar frame is Basic blue" + (f" ({len(classic)} not, first {classic[0]})" if classic else ""))
    # Windows 7's img0.jpg, filled: blue sky at the desktop's left and right
    lastf = ppm(f"{out}/frames/{frames[-1]}")
    # (with START=1 the Start menu is still open over the left side)
    wall = [px(lastf, x, y) for x, y in (((700, 150) if os.environ.get("START") == "1" else (60, 380)), (960, 380))]
    check(all(q[2] >= 150 and q[2] > q[0] + 40 for q in wall), f"the last frame shows the wallpaper: {wall}")
    for tag, name in (("first", first[0]), ("last", [n for n in frames][-1])):
        subprocess.run([sys.executable, "-I", topng, f"{out}/frames/{name}", f"{out}/{tag}.png"])
if os.environ.get("START") == "1":
    # The Start menu: a topmost window at Basic's width, on the taskbar, once the button is
    # clicked. Its height is its right column's (485 with a workstation's items).
    after = log.split("[gex] input at 60", 1)[1] if "[gex] input at 60" in log else ""
    menu = [m for m in (re.search(r" 491x(\d+)\+0\+(\d+) layer=1", l) for l in after.splitlines() if l.startswith("[wl-place]"))
            if m and int(m[1]) + int(m[2]) == 728 and int(m[1]) > 400]
    check(bool(menu), f"the Start button opens the Start menu (491 wide on the taskbar: {', '.join(sorted({m[1] + ' high' for m in menu})) or 'none'})")
    # Its first frame: the menu's white program list at 100,400.
    shown = [n for n in frames if (lambda f: f[0] == 1024 and px(f, 100, 400) == (255, 255, 255) and px(f, 300, 600)[2] < 200)(ppm(f"{out}/frames/{n}"))]
    check(bool(shown), f"a frame shows the Start menu ({len(shown)} frames)")
    if shown: subprocess.run([sys.executable, "-I", topng, f"{out}/frames/{shown[-1]}", f"{out}/start.png"])
if os.environ.get("APPS") == "1":
    # taskapp.log: "NAME shown|activated TICK" lines, in order
    try: notes = [l.split()[:2] for l in open(sys.argv[3]) if l.strip()]
    except OSError: notes = []
    check(["taskone", "shown"] in notes and ["tasktwo", "shown"] in notes,
          f"both startup programs ran and showed their windows ({notes})")
    # A running program's button: Basic's light face (its top edge at y 731 is
    # near white, where the bar is (167,192,220)), in the third and fourth slots,
    # with the drawn bar's blue past them.
    def lit(f, x): return px(f, x, 731)[0] >= 200
    def shown(f): return f[0] == 1024 and lit(f, 192) and lit(f, 256) and basic(px(f, 350, 731)) and basic(px(f, 650, 740))
    both = [n for n in frames if shown(ppm(f"{out}/frames/{n}"))]
    check(bool(both), f"the taskbar shows a button for each program ({len(both)} frames)")
    if both: subprocess.run([sys.executable, "-I", topng, f"{out}/frames/{both[-1]}", f"{out}/buttons.png"])
    # Each button shows its program's icon, which the shell (another process)
    # reads from the program's window or class: Task One's is IDI_WARNING (a
    # yellow triangle), Task Two's IDI_INFORMATION (a blue disc), not the
    # blank document the shell draws for an icon it cannot read.
    # (the blank document has a few such pixels: 3 and 24; the icons 391 and 533)
    def has(f, x0, x1, want):
        return sum(want(px(f, x, y)) for x in range(x0, x1) for y in range(732, 766)) >= 150
    def yellow(p): return p[0] >= 200 and p[1] >= 120 and p[2] <= 90
    def blue(p): return p[2] >= 150 and p[0] <= 110 and p[2] - p[1] >= 40
    if both:
        last = ppm(f"{out}/frames/{both[-1]}")
        check(has(last, 186, 242, yellow) and has(last, 248, 304, blue),
              "the buttons show their programs' icons (Task One's warning, Task Two's information)")
    clicked = "[gex] input at 150" in log
    act = [n for n, what in notes if what == "activated"]
    check(clicked and act[-1:] == ["taskone"] and act.count("taskone") >= 2,
          f"a click on Task One's button activates it (activations: {act}; clicked: {clicked})")
print("shell leg:", "PASS" if not fails else f"FAIL ({len(fails)})")
sys.exit(1 if fails else 0)
PY
