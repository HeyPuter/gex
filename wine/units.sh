#!/bin/bash
# Function-tier units for the images a run named as unitless.
#
#   wine32/units.sh LOG UNITS GEXC DIST GAMEDIR [PREFIX]
#
# Every image LOG names without a unit ("unitless: PATH BASE" from
# wine/page/page-run.mjs, "image PATH at base BASE: no
# function-tier unit" from the page, "PATH at BASE: no function-tier unit" from
# gexc) gets `gexc cgen-image PATH --pe-base BASE` into the unit cache UNITS
# (GEX_CGEN_CACHE), and so does the NT trampoline page when LOG names it
# missing. Page paths are mapped: /wine32 is DIST (/w3 in recordings made
# before the rename), /wine a 64-bit Wine's DIST, /pfx's C: drive PREFIX's
# (a 64-bit Wine's page run), /g GAMEDIR; host paths (a native run's) are used as they
# are. The provider modules are built too (`cgen-image --providers`), once per
# gexc and flavour. Unit keys include gexc's gex-cgen sources: units from another gexc are
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
# In each flavour: a process's providers follow its images' (the 32-bit images of a WoW64
# process fold KUSER_SHARED_DATA, so their providers are --wine32-kuser's).
for f in "" --wine32-kuser --mem64; do
	nice -n 19 "$G" cgen-image $f --providers --jobs ${WINE32_JOBS:-12} 2>&1 | tail -1
done
file() { # PATH: the host file of a run's path, or nothing
  local p=$1
  case $p in
    /wine32/*) echo "$D/${p#/wine32/}" ;;
    /w3/*) echo "$D/${p#/w3/}" ;;
    /wine/*) echo "$D/${p#/wine/}" ;;
    /g/*) echo "$A/${p#/g/}" ;;
    /pfx/dosdevices/c:/windows/system32/*|/pfx/dosdevices/c:/windows/syswow64/*)
      # a builtin's copy is a link to the dist's file in the page; a file of
      # its own there (a program's native DLL of a builtin's name) is the image
      local a=x86_64 n=${p##*/} q=; case $p in */syswow64/*) a=i386 ;; esac
      [ -n "$P" ] && q=$P/drive_c/${p#/pfx/dosdevices/c:/}
      if [ -n "$q" ] && [ -f "$q" ] && [ ! -L "$q" ]; then echo "$q"
      elif [ -f "$D/lib/wine/$a-windows/$n" ]; then echo "$D/lib/wine/$a-windows/$n"
      elif [ -n "$P" ]; then echo "$P/drive_c/${p#/pfx/dosdevices/c:/}"; fi ;;
    /pfx/dosdevices/c:/*)
      # a link to a page path (a program linked to a builtin's file, as
      # C:\winlogon.exe to the dist's) is that path's file
      local q t; [ -n "$P" ] && q=$P/drive_c/${p#/pfx/dosdevices/c:/} || return 0
      if [ -L "$q" ] && t=$(readlink "$q") && [ "${t#/}" != "$t" ]; then file "$t"; else echo "$q"; fi ;;
    # the page's root (Z:): a page path, the program's directory /g among them
    /pfx/dosdevices/z:/*) file "/${p#/pfx/dosdevices/z:/}" ;;
    /*) echo "$p" ;;
  esac
}
build() { # FILE BASE FLAG...: the last line of cgen-image's report
  local f=$1 b=$2; shift 2
  nice -n 19 "$G" cgen-image "$f" --pe-base "$b" "$@" --jobs $J 2>&1 | tail -1 | cut -c1-140
}
todo=$(mktemp); locks=$(mktemp -d); trap 'rm -f "$todo"; rm -rf "$locks"' EXIT
# PATH BASE INPUTS KEY (the page's inputs id of the process it ran in and the key it asked, "-" when
# the line names none)
{ sed -n 's/^unitless: \(\S*\) \(0x[0-9a-f]*\).*/\1 \2 -/p' "$log"
  sed -n 's/.*image \(\S*\) at base \(0x[0-9a-f]*\): no function-tier unit (key \([0-9a-f]*\(-p[0-9a-f]*\)\?\), inputs \([0-9a-f]*\).*/\1 \2 \5 \3/p' "$log"
  sed -n 's/.*image \(\S*\) at base \(0x[0-9a-f]*\): no function-tier unit (key [0-9a-f]*\(-p[0-9a-f]*\)\?)/\1 \2 -/p' "$log"
  sed -n 's/^gex-\(w3\|wine32\): \(\/\S*\) at \(0x[0-9a-f]*\): no function-tier unit.*/\2 \3 -/p' "$log"; } |
  sort -u | awk '{ k = $1 " " $2 } $3 != "-" { named[k] = 1; print; next } { bare[k] = $0 " -" } END { for (k in bare) if (!(k in named)) print bare[k] }' > "$todo"
# The flavour, per inputs id (a 64-bit Wine's WoW64 processes and its other
# ones differ), from the first image of that id whose key the log names.
declare -A flag=()
while read -r p b in _; do
  [ -z "${flag[$in]+x}" ] || continue
  # (that id's line: an image both kinds load, at one base, has a key per id)
  if [ "$in" = - ]; then
    key=$(grep -aom1 "$p at \(base \)\?$b: no function-tier unit (key [0-9a-f]*\(-p[0-9a-f]*\)\?" "$log" | sed 's/.*(key //')
  else
    key=$(grep -aom1 "$p at base $b: no function-tier unit (key [0-9a-f]*\(-p[0-9a-f]*\)\?, inputs $in" "$log" | sed 's/.*(key //; s/,.*//')
  fi
  f=$(file "$p")
  [ -n "$key" ] && [ -n "$f" ] && [ -f "$f" ] || continue
  # (a unit an earlier run made tells nothing: the first flavour tried would match)
  [ -f "$U/$key/c.wasm" ] && continue
  for fl in plain --wine32-kuser --wine64-compact --mem64; do
    build "$f" "$b" ${fl#plain} > /dev/null
    [ -f "$U/$key/c.wasm" ] && { flag[$in]=$fl; break; }
  done
done < "$todo"
for in in "${!flag[@]}"; do echo "wine32/units: inputs $in: ${flag[$in]}"; done
# an image whose run named no inputs id: the one flavour found, or plain
any=plain; [ ${#flag[@]} = 1 ] && any=${flag[*]}
# A named key the id's flavour does not make is tried in the others: one
# inputs id can cover processes of both kinds (a 64-bit Wine's session
# programs in the compact space and its other 64-bit processes), so an image
# both load at one base has a key per kind under one id.
one() {
  local p=$1 b=$2 in=$3 key=$4 f fl r l
  # one build of an image at a base at a time: its lines per inputs id run
  # in parallel and would write the same unit together
  exec {l}> "$locks/${p//\//_}-$b"; flock "$l"
  fl=${flag[$in]:-$any}
  f=$(file "$p")
  [ -n "$f" ] || { echo "wine32/units: FAIL: unmapped $p" >&2; touch "$locks/failed"; return; }
  [ -f "$f" ] || { echo "wine32/units: FAIL: missing $f ($p)" >&2; touch "$locks/failed"; return; }
  r=$(build "$f" "$b" ${fl#plain})
  if [ "$key" != - ] && [ ! -f "$U/$key/c.wasm" ]; then
    for o in plain --wine32-kuser --wine64-compact --mem64; do
      [ $o = "$fl" ] && continue
      r=$(build "$f" "$b" ${o#plain}); [ -f "$U/$key/c.wasm" ] && break
    done
  fi
  echo "$p $b: $r"
  [ "$key" = - ] || [ -f "$U/$key/c.wasm" ] || { echo "wine32/units: FAIL: $p $b: no flavour makes the key the run asked for ($key)" >&2; touch "$locks/failed"; }
  exec {l}>&-
}
{ while read -r p b in key; do
    one "$p" "$b" "$in" "$key" &
    while [ "$(jobs -r | wc -l)" -ge 4 ]; do wait -n; done
  done < "$todo"; wait; }
# The NT trampoline page's unit, which the page places with the first image:
# each set of options a miss line asks for (a flavour found from an image
# misses when the images' units were all there before this run)
grep -ao 'missing unit: \[the trampolines\].*gexc cgen-image --nt-trampolines[0-9a-z -]*' "$log" |
  sed 's/.*gexc cgen-image //; s/ *$//' | sort -u | while read -r o; do
  echo "wine32/units: the trampolines ($o): $(nice -n 19 "$G" cgen-image $o 2>&1 | tail -1 | cut -c1-140)"
done
echo "wine32/units: $U"
[ ! -e "$locks/failed" ]
