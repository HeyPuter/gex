#!/bin/bash
# Run each real-application candidate once under gex_wgpu and say which of
# three things happened: the driver refused something, the app needs an
# environment we did not give it, or it drew.  Separate from the app sweep
# because these need arguments, a compositor or a file, which is exactly why
# the sweep skips them.
set -u
OUT=${1:-/tmp/newapps}
mkdir -p "$OUT"
INST=${GEX_INST:-/tmp/gex-mesa-native/inst}
export DISPLAY=${GEX_DISPLAY:-:78}
export LD_LIBRARY_PATH=$INST/lib:$HOME/.cache/gex-dawn-native/pkg/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}
export LIBGL_DRIVERS_PATH=$INST/lib/dri
export LIBGL_ALWAYS_SOFTWARE=1
export GALLIUM_DRIVER=${GALLIUM_DRIVER:-gex_wgpu}
export vblank_mode=0
VID=$(ls ~/Videos/*.mp4 2>/dev/null | head -1)
run() {  # run <name> <seconds> <cmd...>
  local name=$1 secs=$2; shift 2
  local log="$OUT/$name.log"
  timeout -s KILL "$secs" "$@" >"$log" 2>&1; local rc=$?
  # Not the startup format probe. Every context asks the driver which formats
  # are renderable and prints a "render target refused" line for each one that
  # is not -- fifteen of them, every run, for every application. Counting
  # those made four apps here report refused=15 and look broken when nothing
  # had been refused of anything they asked for.
  local refused=$(grep -E "gex-wgpu: .*(rejected|refused|unsupported|TODO)" "$log" 2>/dev/null |
                  grep -vc "render target refused")
  local draws=$(grep -cE "gex-wgpu" "$log" 2>/dev/null)
  # Whether it *rendered*, which the line count cannot say.
  #
  # The count includes the startup format queries -- fourteen of them for
  # every context -- so an application that creates a context and draws
  # nothing scores driverlines=21 and reads as healthy beside one that drew a
  # frame. Measured: cemu, sunshine, gimp and inkscape all reported 21 and
  # compiled *no shaders at all*, while alacritty, mpv, krita, amdgpu_top and
  # MaterialXGraphEditor compiled ten to seventeen each. Those are different
  # outcomes and the table said the same thing about both.
  #
  # A compiled shader is the cheapest honest evidence of rendering: nothing
  # reaches gex_wgpu_compile_nir except a program the application linked and
  # meant to draw with. This is the same correction the refusal count above
  # already carries for format probes -- see [[a-measurement-can-measure-nothing]].
  #
  # Labelled NO-RENDER rather than anything about contexts, deliberately.
  # Twenty-one lines is the *screen* being created -- the adapter limits, the
  # optional features, "native screen on Dawn" -- and says nothing about
  # whether a context followed. Checked: gimp's twenty-one are all screen
  # setup, and gimp's own output is failing to load plug-ins (libcfitsio,
  # gjs), so it never tried to render. Claiming "context" would have been a
  # guess dressed as a measurement.
  local shaders=$(grep -c "shader compiled" "$log" 2>/dev/null)
  local note=""
  case $rc in 137|124) note="ran-to-timeout" ;; 0) note="exited-clean" ;; *) note="exit $rc" ;; esac
  # An application that printed *no* driver line never reached this driver at
  # all, and "ran-to-timeout" then reads as a success it did not earn: the run
  # says nothing about the driver either way. Every context that does reach it
  # prints at least the startup format queries, so zero is unambiguous.
  [ "$draws" -eq 0 ] && note="$note NO-DRIVER"
  [ "$draws" -gt 0 ] && [ "$shaders" -eq 0 ] && note="$note NO-RENDER"
  [ "$refused" -gt 0 ] && note="$note refused=$refused"
  printf '%-24s %-30s shaders=%-4s driverlines=%s\n' \
         "$name" "$note" "$shaders" "$draws" | tee -a "$OUT/results.txt"
}
: > "$OUT/results.txt"
run alacritty            12 alacritty -e sleep 10
run amdgpu_top           12 amdgpu_top --gui
run MaterialXGraphEditor 12 MaterialXGraphEditor
run aseprite             12 aseprite
run mpv                  15 mpv --gpu-api=opengl --vo=gpu --length=5 --no-audio --really-quiet "$VID"
run dolphin-emu          15 dolphin-emu
run cemu                 15 cemu
run sunshine             10 sunshine
run hello_xr             10 hello_xr -g OpenGL
# Four real desktop applications that were installed here and tested by
# nothing: not in the app sweep's list, which is built from binaries that link
# libGL directly, and Krita and OBS reach GL through Qt and so do not. They are
# the shape that matters most -- a toolkit creating a context, a canvas, and
# blend modes -- and they need longer than the demos above to get a context up.
#
# Krita is a named target: KHR_blend_equation_advanced, which this driver
# serves through the colour-attachment snapshot, *is* its layer blend modes.
run krita                25 krita --nosplash
run gimp                 25 gimp -s -f --no-shm
run inkscape             20 inkscape
run obs                  20 obs --minimize-to-tray
echo DONE >> "$OUT/results.txt"
