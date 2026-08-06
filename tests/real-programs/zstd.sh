#!/bin/sh
# zstd under gex, checked byte for byte against QEMU.
#
# A compressor is a good thing to check a translator with: the output is a
# single artifact that every executed instruction contributed to, so one wrong
# flag anywhere changes the file. Three compression levels because they take
# very different paths through the library -- level 1 is a fast hash chain,
# level 19 is a full binary tree with optimal parsing -- plus a round trip and
# the multi-threaded mode, which runs four worker threads inside a real
# program rather than a test written to exercise threads.
#
#   usage: tests/real-programs/zstd.sh [work-dir]
set -eu

WORK="${1:-${TMPDIR:-/tmp}/gex-zstd}"
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
GEXC="$ROOT/target/release/gexc"
SHIM="$ROOT/target/wasm32-unknown-unknown/release/gex_browser.wasm"
VERSION=1.5.6
URL="https://github.com/facebook/zstd/releases/download/v$VERSION/zstd-$VERSION.tar.gz"

TOOLCHAIN="${GEX_TOOLCHAIN:-$HOME/toolchains/aarch64-linux-musl-cross}"
QEMU="${GEX_QEMU:-$HOME/toolchains/qemu-aarch64}"
CC="$TOOLCHAIN/bin/aarch64-linux-musl-gcc"

for tool in "$GEXC" "$CC" "$QEMU"; do
    if [ ! -x "$tool" ]; then
        echo "skip: $tool is not there"
        exit 0
    fi
done
if ! command -v node >/dev/null 2>&1 || [ ! -f "$SHIM" ]; then
    echo "skip: the browser shim is not built"
    exit 0
fi

mkdir -p "$WORK"
cd "$WORK"
if [ ! -d "zstd-$VERSION" ]; then
    echo "== fetching zstd $VERSION"
    curl -fsSL -o zstd.tar.gz "$URL"
    tar xzf zstd.tar.gz
fi
Z="$WORK/zstd-$VERSION"
if [ ! -x "$WORK/zstd" ]; then
    echo "== building zstd for aarch64"
    make -j"$(nproc)" -C "$Z/programs" zstd \
        CC="$CC" LDFLAGS=-static HAVE_ZLIB=0 HAVE_LZMA=0 HAVE_LZ4=0 MOREFLAGS="-O2" >/dev/null
    cp "$Z/programs/zstd" "$WORK/zstd"
fi

# Something with structure: an executable's worth of machine code followed by
# a few hundred kilobytes of C, so the compressor meets both.
CORPUS="$WORK/corpus.bin"
if [ ! -f "$CORPUS" ]; then
    cat "$Z"/lib/compress/*.c "$Z"/lib/decompress/*.c "$Z"/lib/common/*.c >"$WORK/text"
    cat "$WORK/zstd" "$WORK/text" "$WORK/text" >"$CORPUS"
fi

echo "== building the module"
"$GEXC" build --no-shared-memory "$WORK/zstd" -o "$WORK/zstd.wasm" >/dev/null

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

browser() {
    node "$ROOT/shims/browser/js/run-node.mjs" --argv0 "$WORK/zstd" "$@"
}

echo "== compressing, gex against QEMU"
for level in 1 9 19; do
    "$QEMU" "$WORK/zstd" -q -f "-$level" "$CORPUS" -o "$WORK/q-$level.zst"
    browser --file "$CORPUS=$CORPUS" --extract "/out.zst=$WORK/b-$level.zst" \
        "$WORK/zstd.wasm" "$WORK/zstd" -q -f "-$level" "$CORPUS" -o /out.zst
    if cmp -s "$WORK/q-$level.zst" "$WORK/b-$level.zst"; then
        check ok "level $level ($(wc -c <"$WORK/b-$level.zst") bytes, identical)"
    else
        check bad "level $level: the compressed files differ"
    fi
done

echo "== four worker threads"
"$QEMU" "$WORK/zstd" -q -f -T4 -9 "$CORPUS" -o "$WORK/q-t4.zst"
browser --file "$CORPUS=$CORPUS" --extract "/out.zst=$WORK/b-t4.zst" \
    "$WORK/zstd.wasm" "$WORK/zstd" -q -f -T4 -9 "$CORPUS" -o /out.zst
if cmp -s "$WORK/q-t4.zst" "$WORK/b-t4.zst"; then
    check ok "-T4 (identical)"
else
    check bad "-T4: the compressed files differ"
fi

echo "== round trip"
browser --file "$WORK/q-19.zst=$WORK/q-19.zst" --extract "/out=$WORK/b-dec" \
    "$WORK/zstd.wasm" "$WORK/zstd" -q -f -d "$WORK/q-19.zst" -o /out
if cmp -s "$CORPUS" "$WORK/b-dec"; then
    check ok "decompressed back to the original"
else
    check bad "the round trip does not recover the input"
fi

echo
echo "$pass passed, $fail failed"
[ "$fail" -eq 0 ]
