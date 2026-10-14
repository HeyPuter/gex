#!/bin/bash
# Bring a prefix's copies of Wine's builtin DLLs up to a dist, offline: what
# `wineboot --update` does for the files, without running anything.
#
#   update-builtins.sh PFX DIST [NEW.dll...]
#
# Wine finds a builtin through its file in system32: a DLL the dist gained
# (samcli.dll) cannot load in a prefix made before it, and the copy there
# should be the dist's. Each builtin copy in PFX's system32 that differs from
# DIST/lib/wine/i386-windows is replaced, and each NEW builtin named is
# added (wineboot places some builtins elsewhere -- winsxs, the .NET
# directories -- so a missing one is added only when named). A native image
# in system32 -- a Windows 7 file staged by stage.sh -- is never replaced.
# Prints what it copied.
set -eu
pfx=$1 dist=$2; shift 2; new=" ${*,,} "
sys=$pfx/drive_c/windows/system32
src=$dist/lib/wine/i386-windows
[ -d "$sys" ] && [ -d "$src" ] || { sed -n 2,14p "$0" >&2; exit 2; }
builtin() { cmp -s <(dd if="$1" bs=1 skip=64 count=16 2>/dev/null) <(printf 'Wine builtin DLL'); }
n=0
for f in "$src"/*; do
  [ -f "$f" ] && builtin "$f" || continue
  name=$(basename "$f"); dst=$sys/$name
  # Windows names are case-blind: a staged native image may differ in case.
  hit=$(find "$sys" -maxdepth 1 -iname "$name" -print -quit)
  [ -n "$hit" ] && dst=$hit
  if [ -e "$dst" ]; then
    builtin "$dst" || continue
    cmp -s "$f" "$dst" && continue
  else
    case $new in *" ${name,,} "*) ;; *) continue;; esac
  fi
  cp "$f" "$dst"; echo "update-builtins: $(basename "$dst")"; n=$((n + 1))
done
echo "update-builtins: $n file(s) copied into $sys"
