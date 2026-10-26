#!/bin/bash
# A pinned download, kept once and checked against its sha256.
#
#   wine32/dl.sh URL SHA256 DIR        -> DIR/<basename of URL>
#
# WINE32_DL_SEED="DIR..." are searched for a file of that name first (an earlier
# hand build's tarballs), and a seeded or fetched file is used only when its
# sha256 matches.
set -eu
url=$1 sum=$2 dir=$3
f=$dir/$(basename "$url")
ok() { echo "$sum  $1" | sha256sum -c --status - 2>/dev/null; }
[ -f "$f" ] && ok "$f" && exit 0
mkdir -p "$dir"
for s in ${WINE32_DL_SEED:-}; do
  if [ -f "$s/$(basename "$url")" ] && ok "$s/$(basename "$url")"; then
    cp "$s/$(basename "$url")" "$f.tmp" && mv "$f.tmp" "$f" && exit 0
  fi
done
echo "wine32/dl: $url" >&2
curl -sSfL --retry 3 -o "$f.tmp" "$url"
ok "$f.tmp" || { echo "wine32/dl: $url: sha256 mismatch (want $sum)" >&2; rm -f "$f.tmp"; exit 1; }
mv "$f.tmp" "$f"
