# Common settings of the Windows 7 shell recipe (docs/win7-shell.md); every
# step script sources this. Environment (defaults in brackets):
#   W7_OUT    the output directory, where every step writes  [./win7-out]
#   WINE_SRC  a checkout of the Wine fork, branch explorer/wine  (required for the Wine steps)
#   MUSL_SRC  a checkout of musl-wasm, branch gex-wasm32-slim     (required for the sysroot)
#   GEX_SRC   this gex checkout  [the tree this script is in]
#   GEX_CRT_SRC  compiler-rt's lib/builtins  [LLVM 21.1.8's, downloaded by 10-sysroot.sh]
#   JOBS      parallel jobs  [nproc]
set -euo pipefail
W7_TOOLS=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
GEX_SRC=$(cd "${GEX_SRC:-$W7_TOOLS/../..}" && pwd)
W7_OUT=$(realpath -m "${W7_OUT:-./win7-out}")
JOBS=${JOBS:-$(nproc)}
mkdir -p "$W7_OUT/logs" "$W7_OUT/downloads" "$W7_OUT/tmp"
export TMPDIR=$W7_OUT/tmp
# the wasm32 sysroot (sysroot.sh) and the compiler wrapper over it
export GEX_WASM_SYSROOT=$W7_OUT/sysroot/sysroot
WCC=$GEX_SRC/tools/wine-wasm/wcc
# Wine's configure switches every tree shares (as W3's builds were made)
WINE_OPTS="--without-x --disable-tests --enable-archs= --without-gstreamer --without-pulse --without-alsa --without-cups --without-sane --without-gphoto --without-v4l2 --without-vulkan --without-dbus --without-krb5 --without-gssapi --without-netapi --without-pcap --without-usb --without-unwind --without-sdl --without-oss --without-udev --without-capi --without-inotify"
need() { local v; for v in "$@"; do [ -n "${!v:-}" ] || { echo "${0##*/}: set $v (docs/win7-shell.md)" >&2; exit 2; }; done; }
step() { echo "[$(date +%H:%M:%S)] ${0##*/}: $*"; }
# fetch URL SHA256: a download, checked, into $W7_OUT/downloads; prints its path
fetch() {
  local f=$W7_OUT/downloads/${1##*/}
  [ -f "$f" ] || curl -fsSL -o "$f.part" "$1" && { [ ! -f "$f.part" ] || mv "$f.part" "$f"; }
  echo "$2  $f" | sha256sum -c --quiet - >&2 || { echo "${0##*/}: $f: checksum mismatch" >&2; exit 1; }
  echo "$f"
}
