#!/bin/sh
# Uncapped per-frame overhead, against llvmpipe on the same display.
#
#   perf-gears.sh [seconds] [geometry]
#
# Every app gate here is vsync-capped at 60, so a 6 ms frame and a 1 ms frame
# score identically and no regression in fixed per-frame cost can be seen at
# all. glxgears with vblank_mode=0 is the opposite measurement: its geometry is
# trivial -- 21 draws a frame -- so what it reports is almost entirely the
# driver's own overhead, which is the thing a translation layer has to keep
# small.
#
# It is a *relative* number. gex_wgpu renders on the GPU and copies the result
# back for X11 to put on screen, while llvmpipe renders straight into CPU
# memory, so the two are not doing the same work and the ratio is not a
# verdict. What it is good for is watching one number over time, and for the
# breakdown GEX_WGPU_TIMING prints beside it.
set -u
# Job-control notices for the killed timeout would otherwise land in the middle
# of the numbers.
set +m 2>/dev/null || true
secs=${1:-14}; geom=${2:-300x300}
INST=${GEX_INST:-/tmp/gex-mesa-native/inst}
export DISPLAY=${DISPLAY:-:78}
run() {  # run <label> <env...>
  label=$1; shift
  log=/tmp/perf-gears-$label.txt
  # Through an inner shell that execs, so the "Killed" notice for the timeout
  # lands in the log rather than in the middle of the numbers.
  env "$@" vblank_mode=0 sh -c \
      "timeout -s KILL $secs glxgears -geometry $geom; exit 0" >"$log" 2>&1 || :
  fps=$(grep -oE '= [0-9.]+ FPS' "$log" | tail -1 | tr -dc '0-9.')
  printf '%-10s %8s FPS' "$label" "${fps:-?}"
  [ -n "${fps:-}" ] && awk -v f="$fps" 'BEGIN{printf "  %5.2f ms/frame", 1000/f}'
  echo
}
run gex_wgpu LD_LIBRARY_PATH=$INST/lib:$HOME/.cache/gex-dawn-native/pkg/lib \
    LIBGL_DRIVERS_PATH=$INST/lib/dri LIBGL_ALWAYS_SOFTWARE=1 \
    GALLIUM_DRIVER=${GALLIUM_DRIVER:-gex_wgpu} GEX_WGPU_TIMING=2
run llvmpipe LIBGL_ALWAYS_SOFTWARE=1 GALLIUM_DRIVER=llvmpipe
# The driver's own split, which is where a regression is actually diagnosable.
grep -oE '[0-9]+% of wall in draw_vbo, [0-9]+% in state calls, [0-9]+% in present' \
    /tmp/perf-gears-gex_wgpu.txt | tail -1
grep -oE 'cache hit rate.*' /tmp/perf-gears-gex_wgpu.txt | tail -1
grep -oE '[0-9]+ submits for [0-9]+ batched draws; broken by: .*' \
    /tmp/perf-gears-gex_wgpu.txt | tail -1
