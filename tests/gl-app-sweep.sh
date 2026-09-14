#!/bin/bash
# Run every installed GL program briefly against gex_wgpu *and* llvmpipe and
# report where the two differ.
#
#   tests/gl-app-sweep.sh [outdir] [list]
#
# Build the list with, from the repo root:
#
#   for f in /usr/bin/*; do [ -x "$f" ] && [ -f "$f" ] &&
#     objdump -p "$f" 2>/dev/null |
#     grep -qE 'NEEDED.*(libGL\.so|libEGL\.so|libGLESv2)' && echo "$f"; done \
#     > /tmp/glapps.txt
#
# This machine has 336 of them: the mesa-demos and piglit-style suites plus a
# few real applications. It is the widest statement available about whether
# ordinary GL works here, and it is what found the vertex input location
# collision that was behind 17 of 27 crashes -- see the sweep section of
# docs/gl-webgpu-handoff.md, including why every crash was a consequence of an
# earlier refusal rather than a bug of its own.
#
# Do not run anything else on the GPU while this is going: a concurrent
# workload does not merely add noise, it changes how far a program gets inside
# its eight seconds and so changes its verdict.
#
# 341 of them are installed (mesa-demos and piglit-style tests). Individually
# they are trivial; together they are the broadest statement available about
# whether ordinary GL works here, and they cost seconds each. What matters is
# not the pass rate -- many need input, or a feature no one has claimed -- but
# which ones the *driver* refuses or crashes in.
set -u
OUT=${1:-/tmp/glsweep}
LIST=${2:-/tmp/glapps.txt}
mkdir -p "$OUT"
: > "$OUT/results.tsv"
printf 'app\tverdict\tref\tdiffers\texit\tnote\n' >> "$OUT/results.tsv"

# Self-contained, deliberately. An earlier version shelled out to a run.sh
# beside itself, which was true where it was written and false once it lived
# here -- every program then exited 127 and, because "exit non-zero" is not one
# of the driver verdicts below, the whole run read as though the failures had
# been fixed. A harness that cannot run the thing it is measuring has to say
# so, which is what the HARNESS verdict is for.
# The reference run is not decoration. Without it this sweep cannot tell a
# driver problem from an environment one: it once reported 193 of 337 programs
# "exited" after the window manager quietly died, every one of them failing
# identically on llvmpipe with an X BadWindow error that looks nothing like
# GL. A verdict is evidence only when the reference disagrees with it, so
# `differs` is the column to read and the rest is context.
INST=${GEX_INST:-/tmp/gex-mesa-native/inst}
export DISPLAY=${GEX_DISPLAY:-:77}
export LD_LIBRARY_PATH=$INST/lib:$HOME/.cache/gex-dawn-native/pkg/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}
export LIBGL_DRIVERS_PATH=$INST/lib/dri
export LIBGL_ALWAYS_SOFTWARE=1
export GALLIUM_DRIVER=${GALLIUM_DRIVER:-gex_wgpu}
[ -d "$INST/lib/dri" ] || { echo "no driver at $INST/lib/dri" >&2; exit 2; }
# Brings up the display *and* the window manager if either is missing. Both
# are needed and they die separately: Xvfb outlives openbox whenever an
# application's process group is killed, and a GLUT program that reads window
# properties then dies with BadWindow -- an X error that looks nothing like
# GL and which read, across a whole sweep, as though the driver had collapsed.
# "Nothing else may touch the GPU while it runs" is in this file's header and
# was never checked. A leftover headless chromium slowed the sister harness,
# gl-app-compare.sh, by 30x -- 13 seconds a program to 6.5 minutes -- with no
# symptom but the clock.
. "$(dirname "$0")/gates/gpu-exclusive.sh"
gex_require_idle_gpu "gl-app-sweep" || exit 2
bash "$(dirname "$0")/ensure-display.sh" >/dev/null 2>&1 || true
xdpyinfo >/dev/null 2>&1 || { echo "no X display at $DISPLAY" >&2; exit 2; }
xprop -root _NET_SUPPORTING_WM_CHECK 2>/dev/null | grep -q "window id" || \
  echo "WARNING: no window manager on $DISPLAY; GLUT programs will exit 1" >&2
while read -r app; do
  name=$(basename "$app")
  case "$name" in
    aseprite|cemu|dolphin-emu|dolphin-emu-nogui|mpv) continue ;;
  esac
  log="$OUT/$name.log"
  # One classifier for both drivers, so a difference between them cannot be a
  # difference of rules.
  classify() {
    local l=$1 r=$2 refused_real wgpuerr latched
    refused_real=$(grep -E "gex-wgpu: .*(rejected|refused)" "$l" 2>/dev/null \
                   | grep -vc "render target refused")
    wgpuerr=$(grep -c "WebGPU error" "$l" 2>/dev/null)
    latched=$(grep -c "gex_wgpu context failed\|context latched" "$l" 2>/dev/null)
    refused_real=${refused_real:-0}; wgpuerr=${wgpuerr:-0}; latched=${latched:-0}
    # 127 first, and in *this* function rather than only beside the driver
    # run. The comment above promises one classifier for both drivers so that a
    # difference between them cannot be a difference of rules -- and this was
    # the one rule the reference did not have. A program that cannot start at
    # all (dotewm, wanting a libGLEW this machine does not have) exits 127
    # under both, was called HARNESS on one side and EXIT on the other, and so
    # appeared in the differs column, which is the column meant to be evidence.
    if [ "$r" -eq 127 ]; then echo HARNESS
    # A signal death is 128+signo and signo never exceeds 64, so the crash
    # range ends at 192. Without the upper bound an ordinary `exit 255` was
    # reported as "CRASH ... signal 127": glsync exits that way saying
    # "GLX_SGI_video_sync not supported", glxpixmap saying "glXCreateContext
    # failed", and both do it identically on llvmpipe. Crash counts from this
    # sweep have been the starting point for investigations before, so an
    # inflated one is expensive.
    elif [ "$r" -gt 128 ] && [ "$r" -le 192 ] && [ "$r" -ne 137 ]; then echo CRASH
    elif [ "$wgpuerr" -gt 0 ]; then echo WGPU-ERROR
    elif [ "$refused_real" -gt 0 ]; then echo REFUSED
    elif [ "$latched" -gt 0 ]; then echo LATCHED
    elif [ "$r" -eq 137 ]; then echo RAN
    elif [ "$r" -eq 0 ]; then echo OK
    else echo EXIT; fi
  }
  { timeout -s KILL 8 "$app" >"$log" 2>&1; rc=$?; } 2>/dev/null
  # The same program on the system's llvmpipe, which is what says whether the
  # verdict above is about this driver or about this machine. `env -u` because
  # the driver's paths are exported for the whole script.
  reflog="$OUT/$name.ref.log"
  { env -u LD_LIBRARY_PATH -u LIBGL_DRIVERS_PATH -u GALLIUM_DRIVER \
        LIBGL_ALWAYS_SOFTWARE=1 DISPLAY="$DISPLAY" \
        timeout -s KILL 8 "$app" >"$reflog" 2>&1; refrc=$?; } 2>/dev/null
  refverdict=$(classify "$reflog" "$refrc")
  # The driver's own complaints, which are the point of the sweep. Format
  # probing ("render target refused") is routine and not a complaint.
  refused=$(grep -cE "gex-wgpu: .*(rejected|refused)" "$log" 2>/dev/null)
  refused_real=$(grep -E "gex-wgpu: .*(rejected|refused)" "$log" 2>/dev/null \
                 | grep -vc "render target refused")
  wgpuerr=$(grep -c "WebGPU error" "$log" 2>/dev/null)
  latched=$(grep -c "gex_wgpu context failed\|context latched" "$log" 2>/dev/null)
  note=""
  if [ "$rc" -eq 127 ]; then
    verdict=HARNESS; note="could not execute"
  elif [ "$rc" -gt 128 ] && [ "$rc" -le 192 ] && [ "$rc" -ne 137 ]; then
    verdict=CRASH; note="signal $((rc-128))"
  elif [ "$wgpuerr" -gt 0 ]; then
    verdict=WGPU-ERROR
    note=$(grep -oE "WebGPU error.*" "$log" | head -1 | cut -c1-110)
  elif [ "$refused_real" -gt 0 ]; then
    verdict=REFUSED
    note=$(grep -E "gex-wgpu: .*(rejected|refused)" "$log" | grep -v "render target refused" | head -1 | cut -c1-110)
  elif [ "$latched" -gt 0 ]; then
    verdict=LATCHED
  elif [ "$rc" -eq 137 ]; then
    verdict=RAN            # killed by the timeout: it was still drawing
  elif [ "$rc" -eq 0 ]; then
    verdict=OK
  else
    verdict=EXIT; note="exit $rc"
  fi
  differs=no
  [ "$verdict" != "$refverdict" ] && differs=YES
  printf '%s\t%s\t%s\t%s\t%s\t%s\n' \
    "$name" "$verdict" "$refverdict" "$differs" "$rc" "$note" \
    >> "$OUT/results.tsv"
done < "$LIST"
echo "--- verdicts (gex_wgpu) ---"
tail -n +2 "$OUT/results.tsv" | cut -f2 | sort | uniq -c | sort -rn
echo "--- verdicts (llvmpipe) ---"
tail -n +2 "$OUT/results.tsv" | cut -f3 | sort | uniq -c | sort -rn
echo "--- where they differ, which is the only part that is evidence ---"
awk -F'\t' 'NR>1 && $4=="YES"{printf "  %-34s gex=%-11s ref=%-11s %s\n", \
                                      $1, $2, $3, $6}' "$OUT/results.tsv"
