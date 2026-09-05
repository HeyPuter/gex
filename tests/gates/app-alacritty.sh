#!/bin/sh
# Alacritty, photographed, against llvmpipe on the same display.
#
# It earns a gate of its own because it is the only app here whose vertex
# layout makes u_vbuf reserve a translate slot per step mode: its glyph quads
# read five per-instance attributes, one of them three bytes wide, and WebGPU
# has no unorm8x3. u_vbuf moves that one into a buffer of its own and leaves
# the slot it reserves for per-vertex translation unreferenced, so the buffer
# indices arrive at the driver non-contiguous. The driver refused exactly that
# shape once, and because a refused vertex-element state is simply never bound,
# every glyph quad collapsed to the origin -- a window with no text in it, and
# no error anywhere. Neither the CTS nor any probe here reproduces it: st
# compacts the binding numbers it is given, so only u_vbuf's own reservation
# produces the hole, and only in an app that triggers a translation.
#
#   alacritty.sh <out.png> [settle seconds]
set -u
out=$1; settle=${2:-10}
INST=${GEX_INST:-/tmp/gex-mesa-native/inst}
export DISPLAY=${DISPLAY:-:78}
cmd='while :; do echo GEX_GATE_TEXT; sleep 0.4; done'
shoot() {
  w=$(xwininfo -root -children 2>/dev/null |
      awk 'match($0,/^ *(0x[0-9a-f]+) /,id){if(match($0,/([0-9]+)x([0-9]+)\+/,wh)){a=wh[1]*wh[2]; if(a>b){b=a;p=id[1]}}} END{print p}')
  [ -n "$w" ] && import -window "$w" "$1" 2>/dev/null
}
pkill -KILL -x alacritty 2>/dev/null; sleep 1
( LD_LIBRARY_PATH=$INST/lib:$HOME/.cache/gex-dawn-native/pkg/lib \
  LIBGL_DRIVERS_PATH=$INST/lib/dri LIBGL_ALWAYS_SOFTWARE=1 \
  GALLIUM_DRIVER=${GALLIUM_DRIVER:-gex_wgpu} \
  timeout -s KILL $((settle + 8)) alacritty -e sh -c "$cmd" >"$out.log" 2>&1 & )
sleep "$settle"; shoot "$out"
pkill -KILL -x alacritty 2>/dev/null; sleep 1

ref=${out%.png}-ref.png
( LIBGL_ALWAYS_SOFTWARE=1 GALLIUM_DRIVER=llvmpipe \
  timeout -s KILL $((settle + 8)) alacritty -e sh -c "$cmd" >/dev/null 2>&1 & )
sleep "$settle"; shoot "$ref"
pkill -KILL -x alacritty 2>/dev/null; sleep 1

[ -s "$out" ] || { echo "no shot"; exit 1; }
# Glyphs are thin: the count of distinct colours separates antialiased text
# from a flat background far more sharply than the mean does. A blank window
# holds two or three; the reference is the number to match.
got=$(magick "$out" -depth 8 -format '%k' info:)
if [ -s "$ref" ]; then exp=$(magick "$ref" -depth 8 -format '%k' info:); else exp=?; fi
echo "alacritty colours=$got llvmpipe=$exp  $(magick identify -format '%wx%h' "$out")"
case "$exp" in ?) exit 0;; esac
# Well under the reference means glyphs are missing, which is the regression.
awk -v g="$got" -v e="$exp" 'BEGIN{exit !(g >= e * 0.5)}' &&
  echo "alacritty OK" || { echo "alacritty FAILED: text is missing"; exit 1; }
