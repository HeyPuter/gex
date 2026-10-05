#!/bin/sh
# busybox as a native wasm32 program (docs/wine-native.md §5.1): sh (hush)
# and the common coreutils in one binary, for `gexc run --busybox`.
#
#   tools/wine-wasm/build-busybox.sh OUT_DIR [VERSION]   (GEX_WASM_SYSROOT set)
#
# Downloads busybox VERSION (default 1.37.0) into OUT_DIR, configures it
# with busybox.config (NOMMU: vfork, which the musl fork's wasm32 vfork
# emulates -- the child runs on the parent's stack until it execs or exits,
# which become a posix_spawn; LFS; hush; tail -N), builds with wcc. Prints
# the binary's path.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
out=$(mkdir -p "$1" && cd "$1" && pwd)
ver=${2:-1.37.0}
: "${GEX_WASM_SYSROOT:?set GEX_WASM_SYSROOT (tools/wine-wasm/build-sysroot.sh)}"
src=$out/busybox-$ver
if [ ! -d "$src" ]; then
    (cd "$out" && curl -sSLO "https://busybox.net/downloads/busybox-$ver.tar.bz2" && tar xjf "busybox-$ver.tar.bz2")
fi
cp "$here/busybox.config" "$src/.config"
cd "$src"
yes '' | make oldconfig >/dev/null 2>&1
make -j"${JOBS:-6}" CC="$here/wcc" HOSTCC=gcc AR=llvm-ar STRIP=true SKIP_STRIP=y >"$out/build.log" 2>&1 \
    || { tail -20 "$out/build.log" >&2; exit 1; }
echo "$src/busybox"
