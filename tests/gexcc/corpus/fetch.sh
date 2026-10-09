#!/bin/sh
# Fetch the corpus sources into SRC (default $GEXCC_SRC) and check their
# pinned sha256 (sources.txt). A "-" pin is filled in on first fetch.
#   tests/gexcc/corpus/fetch.sh [NAME...]
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
SRC=${GEXCC_SRC:?set GEXCC_SRC (where tarballs go)}
mkdir -p "$SRC"
grep -v '^#' "$HERE/sources.txt" | while read -r name ver url sum; do
  [ -n "$name" ] || continue
  if [ $# -gt 0 ]; then case " $* " in *" $name "*) ;; *) continue ;; esac; fi
  f=$SRC/$(basename "$url")
  if [ ! -s "$f" ]; then
    if timeout 600 curl -fsSL --connect-timeout 30 -o "$f.part" "$url"; then mv "$f.part" "$f"
    else rm -f "$f.part"; echo "FAIL fetch $name"; continue; fi
  fi
  got=$(sha256sum "$f" | cut -d' ' -f1)
  if [ "$sum" = - ]; then
    sed -i "s|^$name $ver $url -\$|$name $ver $url $got|" "$HERE/sources.txt"
    echo "pinned $name $got"
  elif [ "$sum" != "$got" ]; then
    echo "FAIL sha256 $name: $got, want $sum"; exit 1
  fi
done
