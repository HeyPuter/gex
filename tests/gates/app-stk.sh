#!/bin/sh
# One short SuperTuxKart race on the native Dawn path, photographed.
#   stk.sh <out.png> [settle seconds]
set -u
out=$1; settle=${2:-25}
INST=${GEX_INST:-/tmp/gex-mesa-native/inst}
export DISPLAY=${DISPLAY:-:78}
export LD_LIBRARY_PATH=$INST/lib:$HOME/.cache/gex-dawn-native/pkg/lib
export LIBGL_DRIVERS_PATH=$INST/lib/dri
export LIBGL_ALWAYS_SOFTWARE=1
export GALLIUM_DRIVER=${GALLIUM_DRIVER:-gex_wgpu}
export vblank_mode=0
pkill -KILL -x supertuxkart 2>/dev/null; sleep 1
( timeout -s KILL $((settle + 12)) supertuxkart --race-now --track=snowtuxpeak \
    --kart=tux --numkarts=1 --laps=99 --screensize=640x480 --windowed \
    --disable-hd-textures --disable-glow --disable-bloom --disable-light-shaft \
    --disable-dof --disable-particles --disable-animated-characters \
    --disable-motion-blur --disable-mlaa --disable-ssao --disable-ibl \
    --shadows=0 --anisotropic=0 >"$out.log" 2>&1 & )
sleep "$settle"
w=$(xwininfo -root -children 2>/dev/null | awk 'match($0,/^ *(0x[0-9a-f]+) /,id){if(match($0,/([0-9]+)x([0-9]+)\+/,wh)){a=wh[1]*wh[2]; if(a>b){b=a;p=id[1]}}} END{print p}')
[ -n "$w" ] && import -window "$w" "$out" 2>/dev/null
vram=$(cat /sys/class/drm/card1/device/mem_info_vram_used 2>/dev/null)
pkill -KILL -x supertuxkart 2>/dev/null; sleep 1
if [ -s "$out" ]; then
  echo "shot $(magick identify -format '%wx%h' "$out") vram=$((vram/1048576))MiB"
  magick "$out" -colorspace gray -format 'dark=%[fx:mean<0.08?1:0] meangray=%[fx:int(mean*100)]/100 stdev=%[fx:int(standard_deviation*100)]/100\n' info:
  magick "$out" -format 'distinct=%[fx:0] colors: ' info: ; magick "$out" -depth 4 -format '%k\n' info:
else echo "no shot"; fi
