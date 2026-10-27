#!/bin/bash
# Function-tier units for the images a run named as unitless.
#
#   wine32/units.sh LOG UNITS GEXC DIST GAMEDIR
#
# Every image LOG names without a unit ("unitless: PATH BASE" from
# tools/wine-wasm/wine32/page-run.mjs, "image PATH at base BASE: no
# function-tier unit" from the page, "PATH at BASE: no function-tier unit" from
# gexc) gets `gexc cgen-image PATH --pe-base BASE` into the unit cache UNITS
# (GEX_CGEN_CACHE), and so does the NT trampoline page when LOG names it
# missing. Page paths are mapped: /wine32 is DIST (/w3 in recordings made
# before the rename), /g GAMEDIR; host paths (a native run's) are used as they
# are. The provider modules are built too (`cgen-image --providers`), once per
# gexc. Unit keys include gexc's gex-cgen sources: units from another gexc are
# simply not found, never wrong.
#
# A Wine that folds KUSER_SHARED_DATA wants its units built with
# --wine32-kuser, which changes their keys. The first image whose key LOG
# names is built plain and, if that is not the key the run asked for, again
# with --wine32-kuser; the rest are built the way that matched.
# Env: WINE32_JOBS (default 12; four images at once, each with WINE32_JOBS/4 jobs).
set -u
log=${1:?LOG} U=$(realpath -m "${2:?UNITS}") G=$(realpath "${3:?GEXC}") D=$(realpath "${4:?DIST}") A=$(realpath "${5:?GAMEDIR}")
J=$(( ${WINE32_JOBS:-12} / 4 )); [ $J -ge 1 ] || J=1
mkdir -p "$U"
export GEX_CGEN_CACHE=$U
nice -n 19 "$G" cgen-image --providers --jobs ${WINE32_JOBS:-12} 2>&1 | tail -1
file() { # PATH: the host file of a run's path, or nothing
  local p=$1
  case $p in
    /wine32/*) echo "$D/${p#/wine32/}" ;;
    /w3/*) echo "$D/${p#/w3/}" ;;
    /g/*) echo "$A/${p#/g/}" ;;
    /pfx/dosdevices/z:/*) echo "/${p#/pfx/dosdevices/z:/}" ;;
    /*) echo "$p" ;;
  esac
}
build() { # FILE BASE FLAG...: the last line of cgen-image's report
  local f=$1 b=$2; shift 2
  nice -n 19 "$G" cgen-image "$f" --pe-base "$b" "$@" --jobs $J 2>&1 | tail -1 | cut -c1-140
}
todo=$(mktemp); trap 'rm -f "$todo"' EXIT
{ sed -n 's/^unitless: \(\S*\) \(0x[0-9a-f]*\).*/\1 \2/p' "$log"
  sed -n 's/.*image \(\S*\) at base \(0x[0-9a-f]*\): no function-tier unit.*/\1 \2/p' "$log"
  sed -n 's/^gex-\(w3\|wine32\): \(\/\S*\) at \(0x[0-9a-f]*\): no function-tier unit.*/\2 \3/p' "$log"; } | sort -u > "$todo"
# The flavour, from the first image whose key the log names.
flag=
while read -r p b; do
  key=$(grep -aom1 "$p at \(base \)\?$b: no function-tier unit (key [0-9a-f]*" "$log" | sed 's/.*(key //')
  f=$(file "$p")
  [ -n "$key" ] && [ -n "$f" ] && [ -f "$f" ] || continue
  build "$f" "$b" > /dev/null
  [ -d "$U/$key" ] && break
  build "$f" "$b" --wine32-kuser > /dev/null
  [ -d "$U/$key" ] && { flag=--wine32-kuser; break; }
done < "$todo"
echo "wine32/units: units built ${flag:-plain}"
one() {
  local p=$1 b=$2 f
  f=$(file "$p")
  [ -n "$f" ] || { echo "wine32/units: unmapped $p"; return; }
  [ -f "$f" ] || { echo "wine32/units: missing $f ($p)"; return; }
  echo "$p $b: $(build "$f" "$b" $flag)"
}
{ while read -r p b; do
    one "$p" "$b" &
    while [ "$(jobs -r | wc -l)" -ge 4 ]; do wait -n; done
  done < "$todo"; wait; }
# The NT trampoline page's unit, which the page places with the first image.
if grep -aq 'missing unit: \[the trampolines\]' "$log"; then
  echo "wine32/units: the trampolines: $(nice -n 19 "$G" cgen-image $flag --nt-trampolines32 2>&1 | tail -1 | cut -c1-140)"
fi
echo "wine32/units: $U"
