#!/bin/bash
# Build each probe natively and run it twice: once on llvmpipe (the reference)
# and once on gex_wgpu, comparing the SUMMARY lines.
ROOT=/home/velzie/src/gex2
# Build products and per-probe logs. Overridable, but the default is a
# fixed path so a rebuild of one probe does not rebuild all 175.
W=${GEX_PROBE_WORK:-/tmp/gex-probe-suite}
mkdir -p "$W"
INST=${GEX_INST:-/tmp/gex-mesa-native/inst}
nm -D --defined-only "$INST/lib/libGL.so.1" 2>/dev/null |
  awk '{print $NF}' | sort -u > "$W/libgl-syms.txt"
for src in "$ROOT"/tests/integration/programs/gl44-*-probe.c; do
  name=$(basename "$src" | sed 's/^gl44-//; s/-probe\.c$//')
  [ -n "${1:-}" ] && [ "$1" != "$name" ] && continue
  # -lX11 for the probes that go through GLX rather than EGL.
  cc -O2 -w -o "$W/$name" "$src" -lEGL -lGL -lX11 2>"$W/$name.cc" || { echo "$name BUILDFAIL"; continue; }
  # A probe that links a GL entry point Mesa's non-glvnd libGL does not export
  # dies at load with "undefined symbol", after printing part of its output --
  # which reads as a mid-run failure and hid three passing probes. Say so.
  miss=$(nm -u "$W/$name" 2>/dev/null | awk '{print $NF}' | grep -E '^gl[A-Z]' |
         sort -u | comm -23 - "$W/libgl-syms.txt" | tr '\n' ' ')
  [ -n "$miss" ] && echo "$name UNLINKABLE  needs via getProcAddress: $miss"
  # DISPLAY for the reference too: the GLX probes (glx-context,
  # window-present) need one, and without it they skip and every comparison
  # against them reads as a difference.
  ref=$(DISPLAY=${GEX_DISPLAY:-:78} \
        EGL_PLATFORM=surfaceless LIBGL_ALWAYS_SOFTWARE=1 GEX_NATIVE_REFERENCE=1 \
        timeout -s KILL 60 "$W/$name" 2>/dev/null | grep -oE 'SUMMARY.*|^[a-z0-9].* (ok|FAILED)( .*)?$' | tail -1)
  got=$(DISPLAY=${GEX_DISPLAY:-:78} \
        LD_LIBRARY_PATH=$INST/lib:$HOME/.cache/gex-dawn-native/pkg/lib \
        LIBGL_DRIVERS_PATH=$INST/lib/dri LIBGL_ALWAYS_SOFTWARE=1 \
        GALLIUM_DRIVER=gex_wgpu EGL_PLATFORM=surfaceless \
        timeout -s KILL 60 "$W/$name" 2>"$W/$name.err" | grep -oE 'SUMMARY.*|^[a-z0-9].* (ok|FAILED)( .*)?$' | tail -1)
  if [ -z "$got" ]; then echo "$name NORESULT   ref=[$ref]"
  elif [ "$ref" = "$got" ]; then echo "$name OK   $got"
  else echo "$name DIFF ref=[$ref] gex=[$got]"; fi
done
