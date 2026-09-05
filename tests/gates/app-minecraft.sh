#!/bin/bash
# Launch the Minecraft 26.2 instance under a chosen driver and photograph it.
#   app-minecraft.sh <driver> <out.png> [budget-seconds]
#
# 26.2 requires an OpenGL 4.4 context, which is why this exists: it produced
# nothing at all until the driver reached that version.
#
# Three things here are not incidental, each having produced a convincing but
# entirely false "the driver renders the title screen blank":
#
#  1. **The display needs its window manager.** Without one the game's window
#     is never managed, and a client that dies holding a grab wedges the
#     server: xwininfo then blocks forever and every later run photographs
#     nothing. ensure-display.sh restores both.
#  2. **The title screen has a loading frame** -- logo and icons drawn, no
#     panorama and no text -- that looks exactly like a texture-atlas defect.
#     It is not one. Poll until the colour count settles instead of
#     photographing after a fixed wait.
#  3. **A failed launch leaves a zenity dialog titled "Minecraft"**, which
#     matches a window search by name and photographs as a small grey box.
#     Match the game window by *size*, and read the dialog's text instead --
#     it says what went wrong, e.g. "Driver does not support OpenGL 3.3".
#
# Note that a driver argument other than gex_wgpu is not a reference run:
# run-native.sh points LIBGL_DRIVERS_PATH at this build, whose megadriver has
# only this driver in it, so `llvmpipe` here fails the GL 3.3 check above.
DRV=${1:-gex_wgpu}; OUT=${2:-/tmp/mc262.png}; BUDGET=${3:-420}
S=$(cd -- "$(dirname "$0")" && pwd)
export DISPLAY=${GEX_DISPLAY:-:77}
bash "$S/../ensure-display.sh" || { echo "display not usable"; exit 2; }
python3 "$S/killmatch.py" prismlauncher EntryPoint net.minecraft zenity >/dev/null 2>&1
sleep 3
( GALLIUM_DRIVER=$DRV timeout $((BUDGET + 120)) "$S/run-native.sh" \
    prismlauncher --launch "26.2" --profile "velzie_" >"$OUT.log" 2>&1 & )
prev=""; stable=0; t=0
while [ "$t" -lt "$BUDGET" ]; do
  sleep 15; t=$((t + 15))
  for z in $(pgrep -x zenity); do
    echo "launch dialog: $(ps -o args= -p "$z" | grep -oE -- '--text=.*' | cut -c1-300)"
    kill -9 "$z" 2>/dev/null
  done
  W=$(xwininfo -root -tree 2>/dev/null |
      awk '{ if (match($0,/0x[0-9a-f]+/)) { id=substr($0,RSTART,RLENGTH)
             if (match($0,/[0-9]+x[0-9]+\+/)) { split(substr($0,RSTART,RLENGTH),d,/[x+]/)
               if (d[1] >= 640 && d[2] >= 360) print id } } }' | head -1)
  [ -z "$W" ] && continue
  import -window "$W" "$OUT" 2>/dev/null || continue
  [ -s "$OUT" ] || continue
  k=$(magick "$OUT" -depth 4 -format '%k' info: 2>/dev/null)
  echo "t=${t}s win=$W colours=$k"
  # The loading frame has ~67 colours; a drawn title screen has thousands.
  if [ -n "$prev" ] && [ "$k" = "$prev" ] && [ "${k:-0}" -gt 200 ]; then
    stable=$((stable + 1)); [ "$stable" -ge 2 ] && break
  else stable=0; fi
  prev=$k
done
python3 "$S/killmatch.py" prismlauncher EntryPoint net.minecraft zenity >/dev/null 2>&1
[ -s "$OUT" ] && magick identify -format 'final %wx%h mean=%[fx:int(mean*255)] colours=' "$OUT" \
  && magick "$OUT" -depth 4 -format '%k\n' info:
