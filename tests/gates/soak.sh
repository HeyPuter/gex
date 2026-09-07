#!/bin/sh
# Does a long run stay flat? A soak gate for resource growth.
#
#   soak.sh [seconds] [stk|gears|browser]
#
# This gate exists because nothing else here measured *growth over time*. The
# whole suite -- probes, CTS families, app screenshots, FPS -- runs for tens of
# seconds and asks whether the answer is right. A leak is right at every
# instant and wrong by the minute, so all of it passed while the driver leaked
# ~35 MiB of VRAM a second (see docs/gl-webgpu-handoff.md, "the leak:
# u_vbuf kept one release slot for three writers"). That capped every
# application at under two minutes on a 4 GB card and nothing said so.
#
# Verdict is on the *slope*, not the peak: a working set is allowed to be
# large, and is not allowed to keep growing. Samples the second half against
# the first, so start-up allocation does not count as growth.
set -u
secs=${1:-120}
app=${2:-stk}
INST=${GEX_INST:-/tmp/gex-mesa-native/inst}
export DISPLAY=${DISPLAY:-:78}
export LD_LIBRARY_PATH=$INST/lib:$HOME/.cache/gex-dawn-native/pkg/lib
export LIBGL_DRIVERS_PATH=$INST/lib/dri
export LIBGL_ALWAYS_SOFTWARE=1
export GALLIUM_DRIVER=${GALLIUM_DRIVER:-gex_wgpu}
export vblank_mode=0
VRAM_SYSFS=${GEX_VRAM_SYSFS:-/sys/class/drm/card1/device/mem_info_vram_used}

# Growth budget over the run's second half, in MiB. Small but not zero: a
# genuinely growing scene may still be filling caches.
BUDGET=${GEX_SOAK_GROWTH_MIB:-64}

# The browser is its own shape: there is no one process to measure, and
# chromium's RSS grows for harness reasons that have nothing to do with the
# driver (it keeps one browser alive across runs, so a day of gates never
# shrinks it -- see docs/gl-webgpu-handoff.md). VRAM is the honest signal
# there, and it is the one the leak moved.
if [ "$app" = browser ]; then
  start_vram=$(( $(cat "$VRAM_SYSFS") / 1048576 ))
  if [ "$start_vram" -gt "${GEX_SOAK_MAX_START_MIB:-600}" ]; then
    echo "soak: GPU already at ${start_vram} MiB; run this on an idle card"
    exit 2
  fi
  log=$(mktemp); trap 'rm -f "$log"' EXIT
  # Long enough that a leak of even a few MiB a second is unmistakable, and
  # the harness's own VRAM guard still aborts if one runs away.
  ( GEX_STK_WALL=$((secs + 60)) GEX_STK_IDLE=$((secs + 60)) \
      timeout -s KILL $((secs + 180)) \
      "$(dirname "$0")/../stk-webgpu-fps.sh" >"$log" 2>&1 & )
  sleep 30    # page load, guest boot and STK's own start-up
  i=0; first_v=0; last_v=0; peak_v=0; half=$((secs / 20)); [ "$half" -lt 2 ] && half=2
  while [ "$i" -lt "$((half * 2))" ]; do
    sleep 10
    i=$((i + 1))
    v=$(( $(cat "$VRAM_SYSFS") / 1048576 ))
    [ "$v" -gt "$peak_v" ] && peak_v=$v
    printf 't=%3ds vram=%4sMiB\n' "$((i * 10 + 30))" "$v"
    [ "$i" -eq "$half" ] && first_v=$v
    last_v=$v
  done
  # Let the harness finish and print its verdict; it outlives the sampling by
  # however long the page took to come up.
  waited=0
  while [ "$waited" -lt 180 ] && ! grep -q 'STK real-present gate' "$log"; do
    sleep 5
    waited=$((waited + 5))
  done
  gv=$((last_v - first_v))
  printf 'soak browser %ss: vram %+dMiB over the second half (peak %dMiB, budget %dMiB)\n' \
    "$secs" "$gv" "$peak_v" "$BUDGET"
  grep -q 'STK real-present gate' "$log" && grep 'STK real-present gate' "$log"
  if [ "$gv" -gt "$BUDGET" ]; then
    echo "soak FAIL -- still growing at the end of the run"
    exit 1
  fi
  echo "soak PASS -- flat"
  exit 0
fi

case $app in
  stk)
    bin=supertuxkart
    set -- --race-now --track=snowtuxpeak --kart=tux --numkarts=1 --laps=99 \
      --screensize=640x480 --windowed --disable-hd-textures --disable-glow \
      --disable-bloom --disable-light-shaft --disable-dof --disable-particles \
      --disable-animated-characters --disable-motion-blur --disable-mlaa \
      --disable-ssao --disable-ibl --shadows=0 --anisotropic=0 ;;
  gears) bin=glxgears; set -- ;;
  *) echo "soak: unknown app '$app' (stk, gears, browser)" >&2; exit 2 ;;
esac
command -v "$bin" >/dev/null 2>&1 || { echo "soak: $bin not installed"; exit 2; }

# Refuse to start on a loaded card, for the reason drive.mjs does: the
# measurement would be of the desktop, not of us.
start_vram=$(( $(cat "$VRAM_SYSFS") / 1048576 ))
if [ "$start_vram" -gt "${GEX_SOAK_MAX_START_MIB:-600}" ]; then
  echo "soak: GPU already at ${start_vram} MiB; run this on an idle card"
  exit 2
fi

pkill -KILL -x "$bin" 2>/dev/null; sleep 1
log=$(mktemp); trap 'rm -f "$log"' EXIT
( timeout -s KILL $((secs + 15)) "$bin" "$@" >"$log" 2>&1 & )
sleep 8   # let it reach a steady state before the first sample

n=0; half=$((secs / 20)); [ "$half" -lt 2 ] && half=2
first_v=0; first_r=0; last_v=0; last_r=0; peak_v=0
i=0
while [ "$i" -lt "$((half * 2))" ]; do
  sleep 10
  i=$((i + 1))
  pid=$(pgrep -x "$bin" | head -1)
  [ -n "$pid" ] || { echo "soak: $bin exited after $((i * 10))s -- see the app log"; exit 1; }
  v=$(( $(cat "$VRAM_SYSFS") / 1048576 ))
  r=$(awk '/VmRSS/{printf "%d", $2/1024}' /proc/"$pid"/status 2>/dev/null || echo 0)
  # An exit between pgrep and the read leaves this empty, and an empty string
  # in the arithmetic below is not an error in every shell -- it is a zero,
  # which would read as a huge drop rather than as a missing sample.
  [ -n "$r" ] || r=0
  [ "$v" -gt "$peak_v" ] && peak_v=$v
  printf 't=%3ds vram=%4sMiB rss=%4sMiB\n' "$((i * 10 + 8))" "$v" "$r"
  if [ "$i" -eq "$half" ]; then first_v=$v; first_r=$r; fi
  last_v=$v; last_r=$r
  n=$i
done
pkill -KILL -x "$bin" 2>/dev/null; sleep 1

gv=$((last_v - first_v)); gr=$((last_r - first_r))
printf 'soak %s %ss: vram %+dMiB rss %+dMiB over the second half (peak vram %dMiB, budget %dMiB)\n' \
  "$app" "$secs" "$gv" "$gr" "$peak_v" "$BUDGET"
if [ "$gv" -gt "$BUDGET" ] || [ "$gr" -gt "$BUDGET" ]; then
  echo "soak FAIL -- still growing at the end of the run"
  exit 1
fi
echo "soak PASS -- flat"
