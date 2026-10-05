#!/bin/sh
# P2's test of stateful providers in an x86-64 guest with a 64-bit guest
# memory (docs/unity-dotnet.md): libgt.so is replaced by gt.wasm (musl
# wasm64), bound when the guest dlopens it. Output must match the native run.
#
#   run.sh SYSROOT [gexc]     SYSROOT: tools/unity-dotnet/sysroot/build.sh OUT's OUT/sysroot
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
SR=$(realpath "$1")
GEXC=$(realpath "${2:-$HERE/../../../target/x86/release/gexc}")
W=${WORK:-$(mktemp -d)}
mkdir -p "$W"
. "$HERE/../../../tools/unity-dotnet/sysroot/flags.sh"

cc -O1 -shared -fPIC -o "$W/libgt.so" "$HERE/libgt.c"
cc -O1 -pthread -o "$W/main" "$HERE/main.c" -ldl
clang $WASM64_CFLAGS -nostdinc -isystem "$SR/include" -isystem "$(clang -print-resource-dir)/include" \
	-I"$HERE/../../../tools/unity-dotnet/provider" -c "$HERE/gt.c" -o "$W/gt.o"
P=$HERE/../../../tools/unity-dotnet/provider
clang $WASM64_CFLAGS -nostdinc -isystem "$SR/include" -isystem "$(clang -print-resource-dir)/include" -I"$P" -c "$P/gexprov64.c" -o "$W/gexprov64.o"
clang $WASM64_CFLAGS -Wno-unused-command-line-argument -c "$P/gexprov64-table.s" -o "$W/gexprov64-table.o"
wasm-ld -mwasm64 --experimental-pic -pie --no-entry --shared-memory --import-memory --max-memory=17179869184 \
	-o "$W/gt.wasm" "$W/gt.o" "$W/gexprov64.o" "$W/gexprov64-table.o" -L"$SR/lib" -lc "$SR/lib/libclang_rt.builtins-wasm64.a"

cd "$W"
# Thread lines come in any order; the implementation line must differ.
native=$(GT_VALUE=forty-two ./main ./libgt.so | sort | sed 's/^impl x86-64$/impl ok/')
gex=$(GEX_WASM_DLLS=libgt.so="$W/gt.wasm" "$GEXC" run --mem64 --env GT_VALUE=forty-two ./main ./libgt.so | sort | sed 's/^impl wasm64$/impl ok/')
if [ "$native" = "$gex" ]; then
	echo "$gex"
	echo PASS
else
	printf '%s\n' "$native" > native.txt
	printf '%s\n' "$gex" > gex.txt
	diff native.txt gex.txt || true
	echo FAIL
	exit 1
fi
