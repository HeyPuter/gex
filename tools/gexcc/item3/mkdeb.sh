#!/bin/bash
# The item-3 providers as a package of the native Debian image (docs/gexcc.md
# §7.8, §7.13): gex-providers-arm64 installs Debian's zlib, zstd and sqlite3
# built as LP64 wasm libraries (on the LP64 libc) at
# /usr/lib/gex/aarch64/<soname>.wasm, where the page's libprov finds them by
# soname and replaces the arm64 libraries an arm64 program (apt) maps.
#
#   mkdeb.sh POOL [WORK]   (GEXCC_PREFIX, A64_SYSROOT; GEX_CRT_SRC if the
#                           sysroot is to be built: WORK/a64libc otherwise)
# Prints `gex-providers-arm64 VERSION PATH`.
set -eu
H=$(cd "$(dirname "$0")" && pwd)
POOL=$(realpath -m "$1"); W=$(realpath -m "${2:-$HOME/gex-sandbox/scratch/CC/item3-deb}")
mkdir -p "$POOL" "$W"
if [ -z "${A64_SYSROOT:-}" ]; then
  "$H/sysroot.sh" "$W/a64libc" >/dev/null
  export A64_SYSROOT=$W/a64libc/sysroot
fi
"$H/build.sh" "$W/out" >/dev/null 2>&1 || { "$H/build.sh" "$W/out"; exit 1; }
"$H/build-zstd.sh" "$W/out" >/dev/null 2>&1 || { "$H/build-zstd.sh" "$W/out"; exit 1; }
"$H/build-sqlite.sh" "$W/out" >/dev/null 2>&1 || { "$H/build-sqlite.sh" "$W/out"; exit 1; }
V=1.0+gex1
P=$W/pkg; rm -rf "$P"; mkdir -p "$P/DEBIAN" "$P/usr/lib/gex/aarch64"
for f in libz.so.1 libzstd.so.1 libsqlite3.so.0; do
  install -m 644 "$W/out/$f.wasm" "$P/usr/lib/gex/aarch64/$f.wasm"
done
(cd "$P" && find usr -type f | sort | xargs md5sum > DEBIAN/md5sums)
size=$(du -sk "$P/usr" | cut -f1)
cat > "$P/DEBIAN/control" <<CTL
Package: gex-providers-arm64
Version: $V
Architecture: arm64
Multi-Arch: same
Section: libs
Priority: optional
Maintainer: gex <gex@localhost>
Installed-Size: $size
Enhances: zlib1g, libzstd1, libsqlite3-0
Description: wasm providers for arm64 programs' libraries (gex)
 Debian's zlib 1.3.1, zstd 1.5.7 and sqlite 3.46.1 built as LP64 wasm
 libraries. gex replaces libz.so.1, libzstd.so.1 and libsqlite3.so.0 with
 them when an arm64 program maps those, so that the library's code runs as
 wasm instead of translated arm64.
CTL
out=$POOL/gex-providers-arm64_${V}_arm64.deb
dpkg-deb --root-owner-group -Zxz --build "$P" "$out" >/dev/null
echo "gex-providers-arm64 $V $out"
