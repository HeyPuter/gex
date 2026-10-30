#!/bin/bash
# Function-tier units for the images a run named as unitless.
#
#   wine32/units.sh LOG UNITS GEXC DIST GAMEDIR [PREFIX]
#
# Every image LOG names without a unit ("unitless: PATH BASE" from
# tools/wine-wasm/wine32/page-run.mjs, "image PATH at base BASE: no
# function-tier unit" from the page, "PATH at BASE: no function-tier unit" from
# gexc) gets `gexc cgen-image PATH --pe-base BASE` into the unit cache UNITS
# (GEX_CGEN_CACHE), and so does the NT trampoline page when LOG names it
# missing. Page paths are mapped: /wine32 is DIST (/w3 in recordings made
# before the rename), /wine a 64-bit Wine's DIST, /pfx's C: drive PREFIX's
# (a 64-bit Wine's page run), /g GAMEDIR; host paths (a native run's) are used as they
# are. The provider modules are built too (`cgen-image --providers`), once per
# gexc. Unit keys include gexc's gex-cgen sources: units from another gexc are
# simply not found, never wrong.
#
# A Wine that folds KUSER_SHARED_DATA wants its units built with
# --wine32-kuser, and a 64-bit Wine's take --wine64-compact or --mem64, which
# change their keys. For each inputs id the page names, the first image whose
# key LOG names is built plain and, if that is not the key the run asked for,
# with each of those in turn; the rest of that id's images are built the way
# that matched.
# Env: WINE32_JOBS (default 12; four images at once, each with WINE32_JOBS/4 jobs).
set -u
log=${1:?LOG} U=$(realpath -m "${2:?UNITS}") G=$(realpath "${3:?GEXC}") D=$(realpath "${4:?DIST}") A=$(realpath "${5:?GAMEDIR}") P=${6:+$(realpath "$6")}
J=$(( ${WINE32_JOBS:-12} / 4 )); [ $J -ge 1 ] || J=1
mkdir -p "$U"
export GEX_CGEN_CACHE=$U
nice -n 19 "$G" cgen-image --providers --jobs ${WINE32_JOBS:-12} 2>&1 | tail -1
file() { # PATH: the host file of a run's path, or nothing
  local p=$1
  case $p in
    /wine32/*) echo "$D/${p#/wine32/}" ;;
    /w3/*) echo "$D/${p#/w3/}" ;;
    /wine/*) echo "$D/${p#/wine/}" ;;
    /g/*) echo "$A/${p#/g/}" ;;
    /pfx/dosdevices/c:/windows/system32/*|/pfx/dosdevices/c:/windows/syswow64/*)
      # a builtin's copy is a link to the dist's file in the page
      local a=x86_64 n=${p##*/}; case $p in */syswow64/*) a=i386 ;; esac
      if [ -f "$D/lib/wine/$a-windows/$n" ]; then echo "$D/lib/wine/$a-windows/$n"
      elif [ -n "$P" ]; then echo "$P/drive_c/${p#/pfx/dosdevices/c:/}"; fi ;;
    /pfx/dosdevices/c:/*) [ -n "$P" ] && echo "$P/drive_c/${p#/pfx/dosdevices/c:/}" ;;
    /pfx/dosdevices/z:/*) echo "/${p#/pfx/dosdevices/z:/}" ;;
    /*) echo "$p" ;;
  esac
}
build() { # FILE BASE FLAG...: the last line of cgen-image's report
  local f=$1 b=$2; shift 2
  nice -n 19 "$G" cgen-image "$f" --pe-base "$b" "$@" --jobs $J 2>&1 | tail -1 | cut -c1-140
}
todo=$(mktemp); trap 'rm -f "$todo"' EXIT
# PATH BASE INPUTS (the page's inputs id of the process it ran in, "-" when
# the line names none)
{ sed -n 's/^unitless: \(\S*\) \(0x[0-9a-f]*\).*/\1 \2 -/p' "$log"
  sed -n 's/.*image \(\S*\) at base \(0x[0-9a-f]*\): no function-tier unit (key [0-9a-f]*, inputs \([0-9a-f]*\).*/\1 \2 \3/p' "$log"
  sed -n 's/.*image \(\S*\) at base \(0x[0-9a-f]*\): no function-tier unit (key [0-9a-f]*)/\1 \2 -/p' "$log"
  sed -n 's/^gex-\(w3\|wine32\): \(\/\S*\) at \(0x[0-9a-f]*\): no function-tier unit.*/\2 \3 -/p' "$log"; } |
  sort -u | awk '{ k = $1 " " $2 } $3 != "-" { named[k] = 1; print; next } { bare[k] = $0 } END { for (k in bare) if (!(k in named)) print bare[k] }' > "$todo"
# The flavour, per inputs id (a 64-bit Wine's WoW64 processes and its other
# ones differ), from the first image of that id whose key the log names.
declare -A flag
while read -r p b in; do
  [ -z "${flag[$in]+x}" ] || continue
  # (that id's line: an image both kinds load, at one base, has a key per id)
  if [ "$in" = - ]; then
    key=$(grep -aom1 "$p at \(base \)\?$b: no function-tier unit (key [0-9a-f]*" "$log" | sed 's/.*(key //')
  else
    key=$(grep -aom1 "$p at base $b: no function-tier unit (key [0-9a-f]*, inputs $in" "$log" | sed 's/.*(key //; s/,.*//')
  fi
  f=$(file "$p")
  [ -n "$key" ] && [ -n "$f" ] && [ -f "$f" ] || continue
  for fl in plain --wine32-kuser --wine64-compact --mem64; do
    build "$f" "$b" ${fl#plain} > /dev/null
    [ -d "$U/$key" ] && { flag[$in]=$fl; break; }
  done
done < "$todo"
for in in "${!flag[@]}"; do echo "wine32/units: inputs $in: ${flag[$in]}"; done
# an image whose run named no inputs id: the one flavour found, or plain
any=plain; [ ${#flag[@]} = 1 ] && any=${flag[*]}
one() {
  local p=$1 b=$2 in=$3 f fl
  fl=${flag[$in]:-$any}
  f=$(file "$p")
  [ -n "$f" ] || { echo "wine32/units: unmapped $p"; return; }
  [ -f "$f" ] || { echo "wine32/units: missing $f ($p)"; return; }
  echo "$p $b: $(build "$f" "$b" ${fl#plain})"
}
{ while read -r p b in; do
    one "$p" "$b" "$in" &
    while [ "$(jobs -r | wc -l)" -ge 4 ]; do wait -n; done
  done < "$todo"; wait; }
# The NT trampoline page's unit, which the page places with the first image:
# in each flavour found (a 32-bit Wine's take no --wine32-kuser)
if grep -aq 'missing unit: \[the trampolines\]' "$log"; then
  t=--nt-trampolines32; grep -aq 'missing unit: \[the trampolines\].*--nt-trampolines64' "$log" && t=--nt-trampolines64
  for fl in $(printf '%s\n' "${flag[@]:-plain}" | sort -u); do
    [ $fl = --wine32-kuser ] && fl=plain
    echo "wine32/units: the trampolines ($fl): $(nice -n 19 "$G" cgen-image ${fl#plain} $t 2>&1 | tail -1 | cut -c1-140)"
  done
fi
echo "wine32/units: $U"
