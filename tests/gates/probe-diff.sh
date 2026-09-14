#!/bin/bash
# One probe, both drivers, compared case by case.
#
#   tests/gates/probe-diff.sh <name> [name...]
#
# where <name> is a probe without its `gl44-` prefix and `-probe.c` suffix,
# the same spelling tests/gates/probe-suite.sh prints.
#
# probe-suite compares **SUMMARY lines**: it says `vertex-streams DIFF
# ref=[SUMMARY cases=6 passed=6 failed=0] gex=[... passed=1 ...]` and stops
# there. Which five cases, and what they actually returned, then costs two
# more runs by hand with the two environments typed out from memory -- and
# the gex environment is four variables long, so typing it from memory is how
# you end up measuring the system driver (see: most of an afternoon, twice).
#
# This prints the per-case table instead, and the probe's own INFO line for
# every case that differs, which is where probes put the measured value:
#
#   case                 reference    gex_wgpu
#   streams_advertised   PASS         FAIL
#     ref: INFO streams_advertised GL_MAX_VERTEX_STREAMS=4 (GL 4.0 wants >= 4)
#     gex: INFO streams_advertised GL_MAX_VERTEX_STREAMS=1 (GL 4.0 wants >= 4)
#
# A case the reference fails too is marked `(both)` and is not the driver's
# problem -- that distinction is the whole reason the reference is run at all,
# and reading a probe's output without it is how a wrong reference gets
# mistaken for a defect.
set -u
ROOT=/home/velzie/src/gex2
W=${GEX_PROBE_WORK:-/tmp/gex-probe-suite}
INST=${GEX_INST:-/tmp/gex-mesa-native/inst}
mkdir -p "$W"

[ $# -ge 1 ] || { sed -n '2,30p' "$0" | sed 's/^# \?//'; exit 2; }

status=0
for name in "$@"; do
    src="$ROOT/tests/integration/programs/gl44-$name-probe.c"
    [ -f "$src" ] || { echo "no such probe: $src" >&2; status=2; continue; }
    if ! cc -O2 -w -o "$W/$name" "$src" -lEGL -lGL -lX11 2>"$W/$name.cc"; then
        echo "$name: BUILDFAIL"; sed 's/^/  /' "$W/$name.cc"; status=2; continue
    fi

    DISPLAY=${GEX_DISPLAY:-:78} EGL_PLATFORM=surfaceless \
        LIBGL_ALWAYS_SOFTWARE=1 GEX_NATIVE_REFERENCE=1 \
        timeout -s KILL 120 "$W/$name" >"$W/$name.ref.out" 2>"$W/$name.ref.err"
    DISPLAY=${GEX_DISPLAY:-:78} \
        LD_LIBRARY_PATH=$INST/lib:$HOME/.cache/gex-dawn-native/pkg/lib \
        LIBGL_DRIVERS_PATH=$INST/lib/dri LIBGL_ALWAYS_SOFTWARE=1 \
        GALLIUM_DRIVER=gex_wgpu EGL_PLATFORM=surfaceless \
        timeout -s KILL 120 "$W/$name" >"$W/$name.gex.out" 2>"$W/$name.gex.err"

    echo "== $name"
    # The probe's own name for each case is the second field of a CASE line.
    # Reading the union of both sides rather than one: a probe that dies part
    # way through emits fewer CASE lines, and the cases it never reached are
    # exactly the interesting ones.
    awk -v n="$name" -v w="$W" '
      function detail(file, case_,   line, hit) {
        hit = ""
        while ((getline line < file) > 0)
          if (line ~ ("INFO[ \t]+" case_ "([ \t]|$)")) { hit = line; break }
        close(file)
        return hit
      }
      FNR==NR { if ($1=="CASE") { ref[$2]=$3; if (!($2 in seen)) { seen[$2]=1; order[++k]=$2 } } ; next }
      { if ($1=="CASE") { gex[$2]=$3; if (!($2 in seen)) { seen[$2]=1; order[++k]=$2 } } }
      END {
        printf "%-28s %-12s %s\n", "case", "reference", "gex_wgpu"
        differ = 0
        for (i = 1; i <= k; ++i) {
          c = order[i]
          r = (c in ref) ? ref[c] : "(absent)"
          g = (c in gex) ? gex[c] : "(absent)"
          mark = ""
          if (r != g) { mark = "   <-- differs"; differ++ }
          else if (r != "PASS") mark = "   (both)"
          printf "%-28s %-12s %s%s\n", c, r, g, mark
          if (r != g) {
            rd = detail(w "/" n ".ref.err", c); gd = detail(w "/" n ".gex.err", c)
            if (rd != "") printf "  ref: %s\n", rd
            if (gd != "") printf "  gex: %s\n", gd
          }
        }
        printf "%d of %d cases differ\n", differ, k
        exit differ > 0
      }' "$W/$name.ref.out" "$W/$name.gex.out" || status=1

    # A probe that refused something says so on stderr and nowhere else.
    if grep -q "gex_wgpu\|declined\|refus" "$W/$name.gex.err" 2>/dev/null; then
        echo "  -- driver said:"
        grep -h "gex_wgpu\|declined\|refus" "$W/$name.gex.err" | sort -u |
            head -8 | sed 's/^/     /'
    fi
    echo
done
exit $status
