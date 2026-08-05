#!/bin/sh
# BYTEmark (nbench) under QEMU, gex and the browser runtime, kernel by kernel.
#
# Ten small benchmarks with very different shapes, which is the point: gex is
# an order of magnitude faster than QEMU on some of them and slower on others,
# and a single number hides that. Anything that claims to make gex faster
# should be checked here, not on one interpreter.
#
# The metric is iterations per second, which nbench arrives at by calibrating
# against the clock, so it compares across hosts directly.
#
#   usage: tests/real-programs/nbench.sh [work-dir]
set -eu

WORK="${1:-${TMPDIR:-/tmp}/gex-nbench}"
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
GEXC="$ROOT/target/release/gexc"
SHIM="$ROOT/target/wasm32-unknown-unknown/release/gex_browser.wasm"
VERSION=2.2.3
URL="https://www.math.utah.edu/~mayer/linux/nbench-byte-$VERSION.tar.gz"

TOOLCHAIN="${GEX_TOOLCHAIN:-$HOME/toolchains/aarch64-linux-musl-cross}"
QEMU="${GEX_QEMU:-$HOME/toolchains/qemu-aarch64}"
CC="$TOOLCHAIN/bin/aarch64-linux-musl-gcc"

for tool in "$GEXC" "$CC" "$QEMU"; do
    if [ ! -x "$tool" ]; then
        echo "skip: $tool is not there"
        exit 0
    fi
done

mkdir -p "$WORK"
cd "$WORK"
if [ ! -d "nbench-byte-$VERSION" ]; then
    echo "== fetching nbench $VERSION"
    curl -fsSL -o nbench.tar.gz "$URL"
    tar xzf nbench.tar.gz
fi
D="$WORK/nbench-byte-$VERSION"
cd "$D"
if [ ! -x nbench ]; then
    echo "== building nbench for aarch64"
    sed -i "s|^CC = .*|CC = $CC|; s|^CFLAGS = .*|CFLAGS = -static -O3 -DLINUX -Wall|" Makefile
    # `pointer` is a build-time probe nbench compiles and runs to find out how
    # wide a long is. Cross-compiling, it cannot be run; aarch64-linux is LP64.
    echo '#define LONG64' >pointer.h
    touch pointer
    make >/dev/null 2>&1 || make
fi
# Two seconds a kernel is enough for nbench to be confident and short enough
# that three hosts fit in a few minutes.
printf 'MINSECONDS=2\nALLSTATS=F\n' >NBCFG

# nbench prints the rate on the same line as the name, unless the run was not
# statistically certain, in which case warnings come between the two.
extract() {
    awk -F: '
        /^[A-Z][A-Z ]+ +:/ {
            gsub(/ +$/, "", $1); name = $1; v = $2; gsub(/ /, "", v)
            if (v != "") { print name "\t" v; name = "" }
            next
        }
        /^ +: *[0-9]/ && name != "" { v = $2; gsub(/ /, "", v); print name "\t" v; name = "" }
    ' | grep -Ev 'INDEX|^TEST'
}

echo "== QEMU"
"$QEMU" ./nbench -cNBCFG 2>/dev/null | extract | sort >"$WORK/qemu.txt"
echo "== gex, native"
"$GEXC" run ./nbench -cNBCFG 2>/dev/null | extract | sort >"$WORK/native.txt"

if command -v node >/dev/null 2>&1 && [ -f "$SHIM" ]; then
    echo "== gex, browser runtime"
    "$GEXC" build --exhaustive --no-shared-memory ./nbench -o "$WORK/nbench.wasm" >/dev/null
    node "$ROOT/shims/browser/js/run-node.mjs" \
        --dir "$D=$D" --cwd "$D" --argv0 "$D/nbench" \
        "$WORK/nbench.wasm" "$D/nbench" -cNBCFG 2>/dev/null | extract | sort >"$WORK/browser.txt"
else
    : >"$WORK/browser.txt"
fi

echo
printf '%-20s %-13s %-22s %s\n' KERNEL QEMU 'GEX (native)' 'GEX (browser)'
join -t"$(printf '\t')" "$WORK/qemu.txt" "$WORK/native.txt" |
    join -t"$(printf '\t')" -a1 - "$WORK/browser.txt" |
    awk -F'\t' '{
        n = $3 / $2
        printf "%-20s %-13s %-11s %5.2fx  ", $1, $2, $3, n
        if ($4 != "") printf "%-11s %5.2fx", $4, $4 / $2
        printf "\n"
    }'
