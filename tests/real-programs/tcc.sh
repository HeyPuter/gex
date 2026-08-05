#!/bin/sh
# Build tcc for aarch64-musl and check gex against QEMU on it.
#
# The integration suite compiles its own programs and runs offline, so a
# third-party compiler does not belong in it. This lives beside it instead:
# fetch, build, and check the one thing a compiler is uniquely good at
# testing, which is that every bit of its output is the same.
#
# Three claims, in order of how much they exercise:
#
#   1. gex runs tcc, and the objects tcc writes are byte-identical to the ones
#      it writes under QEMU -- including tccgen.c, which is 290 KiB of object.
#      Where tcc fails, it fails the same way on both.
#   2. gex runs tcc to build libtcc1.a, which tcc's own build system cannot do
#      when cross-compiling: it would have to run the compiler it just built.
#      Then tcc links a program and gex runs that too.
#   3. The browser runtime does (1) as well, with the headers bundled and the
#      object read back out.
#
#   usage: tests/real-programs/tcc.sh [work-dir]
set -eu

WORK="${1:-${TMPDIR:-/tmp}/gex-tcc}"
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
GEXC="$ROOT/target/release/gexc"
SHIM="$ROOT/target/wasm32-unknown-unknown/release/gex_browser.wasm"
VERSION=0.9.27
URL="https://download.savannah.gnu.org/releases/tinycc/tcc-$VERSION.tar.bz2"

TOOLCHAIN="${GEX_TOOLCHAIN:-$HOME/toolchains/aarch64-linux-musl-cross}"
QEMU="${GEX_QEMU:-$HOME/toolchains/qemu-aarch64}"
CC="$TOOLCHAIN/bin/aarch64-linux-musl-gcc"
AR="$TOOLCHAIN/bin/aarch64-linux-musl-ar"
INC="$TOOLCHAIN/aarch64-linux-musl/include"

for tool in "$GEXC" "$CC" "$QEMU"; do
    if [ ! -x "$tool" ]; then
        echo "skip: $tool is not there"
        exit 0
    fi
done

mkdir -p "$WORK"
cd "$WORK"
if [ ! -d "tcc-$VERSION" ]; then
    echo "== fetching tcc $VERSION"
    curl -fsSL -o tcc.tar.bz2 "$URL"
    tar xjf tcc.tar.bz2
fi
T="$WORK/tcc-$VERSION"
cd "$T"
if [ ! -x tcc ]; then
    echo "== building tcc for aarch64"
    ./configure --cpu=aarch64 --cross-prefix="$TOOLCHAIN/bin/aarch64-linux-musl-" \
        --prefix=/usr/local --config-musl >/dev/null
    # Statically linked, so the guest's view of a path is the host's and the
    # paths tcc records in its output are the same under both.
    make tcc LDFLAGS=-static >/dev/null 2>&1 || make tcc LDFLAGS=-static
fi

pass=0
fail=0
check() {
    if [ "$1" = ok ]; then
        pass=$((pass + 1))
        echo "ok   $2"
    else
        fail=$((fail + 1))
        echo "FAIL $2"
    fi
}

echo "== tcc compiling its own sources, under gex against QEMU"
for f in libtcc tccpp tccgen tccelf tccrun tccasm arm64-gen arm64-link tcc; do
    [ -f "$f.c" ] || continue
    set +e
    "$QEMU" ./tcc -B"$T" -nostdinc -Iinclude -I"$INC" -I. -c "$T/$f.c" -o "$WORK/$f.q.o" 2>"$WORK/q.err"
    qrc=$?
    "$GEXC" run ./tcc -B"$T" -nostdinc -Iinclude -I"$INC" -I. -c "$T/$f.c" -o "$WORK/$f.g.o" 2>"$WORK/g.err"
    grc=$?
    set -e
    if [ "$qrc" != "$grc" ] || ! cmp -s "$WORK/q.err" "$WORK/g.err"; then
        check bad "$f: gex and QEMU disagree about whether it compiles"
    elif [ "$qrc" -ne 0 ]; then
        check ok "$f (both reject it identically)"
    elif cmp -s "$WORK/$f.q.o" "$WORK/$f.g.o"; then
        check ok "$f ($(wc -c <"$WORK/$f.g.o") bytes, identical)"
    else
        check bad "$f: the object files differ"
    fi
done

echo "== gex finishing tcc's build, then running what tcc links"
rm -f lib/lib-arm64.o libtcc1.a
"$GEXC" run ./tcc -B"$T" -nostdinc -I"$INC" -Ilib -I. -c lib/lib-arm64.c -o lib/lib-arm64.o
"$AR" rcs libtcc1.a lib/lib-arm64.o
LIB="$TOOLCHAIN/aarch64-linux-musl/lib"
cat >"$WORK/hello.c" <<'EOF'
#include <stdio.h>
static int fib(int n) { return n < 2 ? n : fib(n - 1) + fib(n - 2); }
int main(void) {
    double x = 1.5;
    for (int i = 0; i < 5; i++) x = x * 1.1 + i;
    printf("%d %.6f\n", fib(20), x);
    return 0;
}
EOF
"$GEXC" run ./tcc -B"$T" -nostdinc -I"$INC" -nostdlib \
    "$LIB/crt1.o" "$LIB/crti.o" "$WORK/hello.c" "$LIB/libc.so" libtcc1.a "$LIB/crtn.o" \
    -o "$WORK/hello"
mkdir -p "$WORK/root/lib"
cp "$LIB/libc.so" "$WORK/root/lib/ld-musl-aarch64.so.1"
cp "$LIB/libc.so" "$WORK/root/lib/libc.so"
cp "$WORK/hello" "$WORK/root/hello"
"$GEXC" run --sysroot "$WORK/root" --argv0 /hello "$WORK/root/hello" >"$WORK/hello.gex"
"$QEMU" -L "$WORK/root" "$WORK/hello" >"$WORK/hello.qemu"
if cmp -s "$WORK/hello.gex" "$WORK/hello.qemu"; then
    check ok "the program tcc linked runs the same under gex and QEMU"
else
    check bad "the program tcc linked disagrees"
fi

if command -v node >/dev/null 2>&1 && [ -f "$SHIM" ]; then
    echo "== the same compile, in the browser runtime"
    "$GEXC" build --no-shared-memory ./tcc -o "$WORK/tcc.wasm" >/dev/null
    node "$ROOT/shims/browser/js/run-node.mjs" \
        --dir "$INC=$INC" --dir "$T=$T" --argv0 "$T/tcc" \
        --extract "/out.o=$WORK/tccgen.br.o" \
        "$WORK/tcc.wasm" "$T/tcc" \
        -B"$T" -nostdinc -I"$T/include" -I"$INC" -I"$T" -c "$T/tccgen.c" -o /out.o
    if cmp -s "$WORK/tccgen.q.o" "$WORK/tccgen.br.o"; then
        check ok "tccgen.c compiled in the browser ($(wc -c <"$WORK/tccgen.br.o") bytes, identical)"
    else
        check bad "the browser's object differs"
    fi
fi

echo
echo "$pass passed, $fail failed"
[ "$fail" -eq 0 ]
