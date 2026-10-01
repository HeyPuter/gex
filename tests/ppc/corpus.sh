#!/bin/sh
# The pewasm Tier-0 benchmark corpus on the PowerPC guest (docs/ppc.md §11):
# build every kernel for ppc32, run it under qemu-ppc (the oracle), natively
# on the host (x86-64), and under `gexc run` (the native wasmtime shim), and
# compare their "result" lines.
#
# C kernels are built with tools/ppc/cc.sh (clang + the ppc32 musl sysroot).
# The C++ ones need a libc++, which the sysroot has not got, so they are built
# entirely with zig's powerpc-linux-musleabihf toolchain (its own musl and
# libc++; the two are never mixed in one binary).
#
#   tests/ppc/corpus.sh [--gexc PATH] [--out DIR] [kernel...]
#
# PEWASM (default ~/src/noita-wasm/pewasm) locates the corpus.
set -u
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
gexc=$root/target/ppc/release/gexc
out=$root/target/ppc-corpus
while [ $# -gt 0 ]; do
    case $1 in
        --gexc) gexc=$2; shift 2 ;;
        --out) out=$2; shift 2 ;;
        *) break ;;
    esac
done
bench=${PEWASM:-$HOME/src/noita-wasm/pewasm}/bench
mkdir -p "$out"
names=${*:-$(ls "$bench/programs")}
pass=0 fail=0
for n in $names; do
    src=$bench/programs/$n/$n.c
    cxx=no
    [ -f "$src" ] || { src=$bench/programs/$n/$n.cc; cxx=yes; }
    elf=$out/$n
    if [ $cxx = yes ]; then
        tc="zig c++ powerpc-linux-musleabihf"
        zig c++ -target powerpc-linux-musleabihf -mcpu=ppc -O2 -static -fno-pic -ffp-contract=off -Wl,--image-base=0x1000000 \
            -o "$elf" "$src" -lm 2>"$out/$n.build.log" || { echo "$n: BUILD FAILED ($tc)"; fail=$((fail+1)); continue; }
    else
        tc="clang+sysroot"
        "$root/tools/ppc/cc.sh" -O2 -ffp-contract=off -o "$elf" "$src" -lm 2>"$out/$n.build.log" \
            || { echo "$n: BUILD FAILED ($tc)"; fail=$((fail+1)); continue; }
    fi
    if [ ! -x "$out/$n.host" ] || [ "$src" -nt "$out/$n.host" ]; then
        if [ $cxx = yes ]; then c=c++; else c=cc; fi
        zig $c -target x86_64-linux-musl -O2 -static -ffp-contract=off -o "$out/$n.host" "$src" -lm 2>/dev/null
    fi
    q=$(qemu-ppc "$elf" 2>&1 | grep '^result')
    h=$("$out/$n.host" 2>&1 | grep '^result')
    g=$("$gexc" run "$elf" 2>"$out/$n.gex.err" | grep '^result')
    if [ -n "$q" ] && [ "$q" = "$g" ] && [ "$q" = "$h" ]; then
        echo "$n: ok ($q) [$tc]"
        pass=$((pass+1))
    else
        echo "$n: MISMATCH qemu=[$q] host=[$h] gex=[$g] [$tc] (see $out/$n.gex.err)"
        fail=$((fail+1))
    fi
done
echo "corpus: $pass passed, $fail failed"
[ $fail -eq 0 ]
