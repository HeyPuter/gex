#!/bin/bash
# Function-tier units for the images a run named as unitless.
#
#   w3/units.sh LOG UNITS GEXC DIST GAMEDIR
#
# Every "unitless: PATH BASE" line of LOG (tools/wine-wasm/w3/page-run.mjs's,
# or "image PATH at base BASE: no function-tier unit" from gexc) gets
# `gexc cgen-image PATH --pe-base BASE` into the unit cache UNITS
# (GEX_CGEN_CACHE). Page paths are mapped: /w3 is DIST, /g GAMEDIR; host paths
# (a native run's) are used as they are. The provider modules are built too
# (`cgen-image --providers`), once per gexc. Unit keys include gexc's gex-cgen
# sources: units from another gexc are simply not found, never wrong.
# Env: W3_JOBS (default 12; four images at once, each with W3_JOBS/4 jobs).
set -u
log=${1:?LOG} U=$(realpath -m "${2:?UNITS}") G=$(realpath "${3:?GEXC}") D=$(realpath "${4:?DIST}") A=$(realpath "${5:?GAMEDIR}")
J=$(( ${W3_JOBS:-12} / 4 )); [ $J -ge 1 ] || J=1
mkdir -p "$U"
export GEX_CGEN_CACHE=$U
GEX_CGEN_CACHE=$U nice -n 19 "$G" cgen-image --providers --jobs ${W3_JOBS:-12} 2>&1 | tail -1
one() {
  local p=$1 b=$2 f
  case $p in
    /w3/*) f=$D/${p#/w3/} ;;
    /g/*) f=$A/${p#/g/} ;;
    /pfx/dosdevices/z:/*) f=/${p#/pfx/dosdevices/z:/} ;;
    /*) f=$p ;;
    *) echo "w3/units: unmapped $p"; return ;;
  esac
  [ -f "$f" ] || { echo "w3/units: missing $f ($p)"; return; }
  echo "$p $b: $(nice -n 19 "$G" cgen-image "$f" --pe-base "$b" --jobs $J 2>&1 | tail -1 | cut -c1-140)"
}
{ sed -n 's/^unitless: \(\S*\) \(0x[0-9a-f]*\).*/\1 \2/p' "$log"
  sed -n 's/.*image \(\S*\) at base \(0x[0-9a-f]*\): no function-tier unit.*/\1 \2/p' "$log"; } | sort -u |
{ while read -r p b; do
    one "$p" "$b" &
    while [ "$(jobs -r | wc -l)" -ge 4 ]; do wait -n; done
  done; wait; }
echo "w3/units: $U"
