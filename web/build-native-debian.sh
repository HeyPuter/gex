#!/bin/bash
# The native Debian rootfs for the page's `native-debian` demo (docs/gexcc.md
# §6): Debian trixie's tools built from Debian's source packages with gexcc
# into wasm32 programs (dpkg architecture musl-linux-wasm32), on Debian's own
# arm64 apt, dpkg, perl and libraries.
#
#   web/build-native-debian.sh [TIER...]       (default: tier1)
#
# Steps (each cached; rerunning rebuilds what changed):
#   1. tools/gexcc/debian/build.sh TIER   sources fetched and verified
#      (debsrc.py), Debian's patches applied, built with gexcc
#   2. mkdebs.py per built package        .debs into $GEXCC_DEB/pool,
#      and item3/mkdeb.sh                  gex-providers-arm64 (zlib, zstd,
#                                          sqlite3 as wasm for arm64 programs)
#   3. mkrootfs.py                        Debian's arm64 closure (verified,
#      debfetch.py) + ours, dpkg database, apt sources and pins
#   4. zstd -19                           web/public/rootfs/native-debian.tar.zst
#
# Needs: GEXCC_PREFIX (an installed gexcc toolchain, tools/gexcc/install.sh),
# dpkg-deb, dpkg-source, gpgv and debian-archive-keyring on the host, zstd.
# Run inside a resource scope (docs/gexcc.md §2):
#   systemd-run --user --scope -p TasksMax=4096 -p MemoryMax=32G nice -n 19 web/build-native-debian.sh
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
TOP=$(cd "$HERE/.." && pwd)
D=$TOP/tools/gexcc/debian
: "${GEXCC_PREFIX:?set GEXCC_PREFIX to an installed gexcc toolchain}"
export GEXCC_DEB=${GEXCC_DEB:-$HOME/gex-sandbox/scratch/CC/debian}
OUT=${OUT:-$HERE/public/rootfs}
WORK=${WORK:-$GEXCC_DEB/rootfs}
tiers=("${@:-tier1}")

"$D/build.sh" "${tiers[@]}"
if grep -q 'build=FAIL' "$GEXCC_DEB/built.txt"; then
  echo "some packages failed to build:"; grep 'build=FAIL' "$GEXCC_DEB/built.txt"; exit 1
fi
rm -f "$GEXCC_DEB"/pool/*.deb
while read -r n v r; do
  [ "$r" = build=ok ] || continue
  u=${v#*:}
  python3 -I "$D/mkdebs.py" --src "$GEXCC_DEB/src/$n-$u" --stage "$GEXCC_DEB/stage/$n" --out "$GEXCC_DEB/pool" "$n" "$v"
done < "$GEXCC_DEB/built.txt"
"$TOP/tools/gexcc/item3/mkdeb.sh" "$GEXCC_DEB/pool"
mkdir -p "$OUT" "$WORK"
python3 -I "$D/mkrootfs.py" --pool "$GEXCC_DEB/pool" --out "$WORK/tree" --tar "$WORK/native-debian.tar"
zstd -19 -T0 -q -f "$WORK/native-debian.tar" -o "$OUT/native-debian.tar.zst"
ls -l "$OUT/native-debian.tar.zst"
