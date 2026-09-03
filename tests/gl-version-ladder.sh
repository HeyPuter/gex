#!/bin/bash
# What each GL rung above 3.3 still wants, measured from Mesa's own ladder.
#
# _mesa_compute_version in src/mesa/main/version.c states each rung's
# requirements exactly, as a run of `extensions->ARB_foo` tests. Diffing those
# against the driver's own extension list is the only honest way to say how
# far away a version is -- impression consistently said fewer.
#
# Run it against the native Dawn build:  tests/gl-version-ladder.sh
# or against any driver:                 GEX_LADDER_GLXINFO="glxinfo" ...
set -u
ROOT=$(cd "$(dirname "$0")/.." && pwd)
VERSION_C=$ROOT/third_party/mesa/src/mesa/main/version.c
INST=${GEX_INST:-/tmp/gex-mesa-native/inst}
LIST=$(mktemp)
if [ -n "${GEX_LADDER_GLXINFO:-}" ]; then
  $GEX_LADDER_GLXINFO -s 2>/dev/null | tr ', ' '\n' > "$LIST"
else
  DISPLAY=${GEX_DISPLAY:-:78} \
  LD_LIBRARY_PATH=$INST/lib:$HOME/.cache/gex-dawn-native/pkg/lib \
  LIBGL_DRIVERS_PATH=$INST/lib/dri LIBGL_ALWAYS_SOFTWARE=1 \
  GALLIUM_DRIVER=gex_wgpu \
  timeout -s KILL 90 glxinfo -s 2>/dev/null | tr ', ' '\n' > "$LIST"
fi
if ! grep -q GL_ARB "$LIST"; then
  echo "no extension list: the driver did not load" >&2; rm -f "$LIST"; exit 1
fi
total=0
for rung in 4_0 4_1 4_2 4_3 4_4 4_5 4_6; do
  # The ver_N block is everything between this label and the next one.
  # The rung is a single `const bool ver_4_N = (ver_4_M && ...);` expression,
  # so the block runs from its declaration to the next one -- there are no
  # labels to match, which is what an earlier version of this assumed and why
  # it reported every rung complete from an empty requirement list.
  want=$(awk -v tag="const bool ver_$rung = " '
    index($0, tag) {on=1}
    on && /^   const bool ver_[0-9]_[0-9] = / && !index($0, tag) {exit}
    on {print}' "$VERSION_C" |
    grep -oE 'extensions->[A-Za-z0-9_]+' | sed 's/extensions->//' | sort -u)
  if [ -z "$want" ]; then
    echo "GL ${rung//_/.}: could not read the requirement list from $VERSION_C" >&2
    rm -f "$LIST"; exit 1
  fi
  missing=
  for e in $want; do
    grep -qx "GL_$e" "$LIST" || missing="$missing $e"
  done
  # Each rung also demands a GLSL level (`consts->GLSLVersion >= N`), read
  # from the driver's `glsl_feature_level` cap *before* Mesa clamps the level
  # down to the granted GL version -- so it is a real requirement that no
  # extension list shows. Without it a rung with every extension present is
  # still refused.
  glsl=$(awk -v tag="const bool ver_$rung = " '
    index($0, tag) {on=1}
    on && /^   const bool ver_[0-9]_[0-9] = / && !index($0, tag) {exit}
    on {print}' "$VERSION_C" |
    grep -oE 'GLSLVersion >= [0-9]+' | grep -oE '[0-9]+$' | head -1)
  if [ -n "$glsl" ] && [ "$glsl" -gt "${GEX_GLSL_LEVEL:-400}" ]; then
    missing="$missing GLSL$glsl"
  fi
  n=$(echo $missing | wc -w)
  total=$((total + n))
  if [ "$n" = 0 ]; then printf 'GL %s: ** COMPLETE **\n' "${rung//_/.}"
  else printf 'GL %s:%s\n' "${rung//_/.}" "$missing"; fi
  [ "$rung" = 4_4 ] && printf -- '--- total to 4.4: %d\n' "$total"
done
# The driver's own GLSL cap, which the rungs above are measured against.
# GEX_GLSL_LEVEL overrides it for a what-if.
printf -- '--- driver glsl_feature_level: %s (Mesa then clamps the usable\n' \
  "${GEX_GLSL_LEVEL:-400}"
printf -- '    #version to the granted GL version, so a 400 cap in a 3.3\n'
printf -- '    context still compiles nothing above 3.30)\n'
rm -f "$LIST"
