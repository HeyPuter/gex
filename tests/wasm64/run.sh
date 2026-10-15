#!/bin/sh
# W64's LP64 programs (docs/wine-native-x64.md §1.1, §5 W64.0) checked against
# native execution: each is built for the host (x86-64, cc) and with
# tools/wine-wasm/wcc64 (wasm64, lowered to wasm32, ELF64 container), and its
# output and exit status must match. The sources are tests/wasm32's that do
# not fork (signals, threads with posix_spawn, dlopen's registry, setjmp,
# sockets) plus lp64.c (the x86-64 kernel layouts).
#
#   tests/wasm64/run.sh [gexc]   (an x86 gexc; GEX_WASM64_SYSROOT = the
#                                 sysroot from tools/wine-wasm/build-sysroot64.sh)
set -u
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
GEXC=${1:-target/x86/release/gexc}
case $GEXC in /*) ;; *) GEXC=$PWD/$GEXC ;; esac
: "${GEX_WASM64_SYSROOT:?set GEX_WASM64_SYSROOT (tools/wine-wasm/build-sysroot64.sh)}"
WORK=${WORK:-$(mktemp -d)}
fail=0
# wasm-elf.py's ELF32 (wasm32) containers must not change with --elf64's
# addition: a fixed module wrapped as before (sha256 of the output from the
# script before W64, with and without --guest-i386).
for v in ":be053519a05d50ffb3adab7bc44684d88356adbd12bb0e8b524132c3c1d2ffab" \
         "--guest-i386:289f1a814079d44b18abf68bd14e84817fdbce7f0371bd4af7fcb5f4f2bf45ba"; do
    python3 "$ROOT/tools/wine-wasm/wasm-elf.py" "$HERE/elf32-fixture.wasm" "$WORK/elf32" ${v%%:*}
    if [ "$(sha256sum < "$WORK/elf32" | cut -c1-64)" = "${v#*:}" ]; then
        echo "ok   wasm64/elf32-container ${v%%:*}"
    else
        echo "FAIL wasm64/elf32-container ${v%%:*}: the ELF32 output changed"; fail=1
    fi
done
for src in "$HERE/lp64.c" "$HERE/../wasm32/signals.c" "$HERE/../wasm32/threads.c" \
           "$HERE/../wasm32/dlopen.c" "$HERE/../wasm32/sjlj.c" "$HERE/../wasm32/inet.c"; do
    name=$(basename "$src" .c)
    cc -O1 -o "$WORK/$name.native" "$src" -lpthread || { fail=1; continue; }
    "$ROOT/tools/wine-wasm/wcc64" -O2 -o "$WORK/$name" "$src" || { echo "FAIL $name: wcc64"; fail=1; continue; }
    native=$(cd "$WORK" && "./$name.native" 2>/dev/null); ns=$?
    gex=$(cd "$WORK" && timeout 120 "$GEXC" run --cwd "$WORK" "./$name" 2>"$WORK/$name.err"); gs=$?
    if [ "$native" = "$gex" ] && [ $ns = $gs ]; then
        echo "ok   wasm64/$name (exit $ns)"
    else
        echo "FAIL wasm64/$name: native exit $ns, gex exit $gs"
        printf '%s\n' "$native" > "$WORK/$name.out-native"
        printf '%s\n' "$gex" > "$WORK/$name.out-gex"
        diff "$WORK/$name.out-native" "$WORK/$name.out-gex" | head -20
        head -5 "$WORK/$name.err" | sed 's/^/  stderr: /'
        fail=1
    fi
done
# The --gex-guest-x86-64 flag (header flag 2): Windows' address space.
want='anonymous mapping below the USD: 1
USD and the page after it free: 1'
if "$ROOT/tools/wine-wasm/wcc64" -O2 -Wl,--gex-guest-x86-64 -o "$WORK/winspace" "$HERE/winspace.c"; then
    got=$(cd "$WORK" && timeout 120 "$GEXC" run --cwd "$WORK" ./winspace 2>"$WORK/winspace.err")
    if [ "$got" = "$want" ]; then
        echo "ok   wasm64/winspace (guest x86-64 flag)"
    else
        echo "FAIL wasm64/winspace (guest x86-64 flag)"; printf '%s\n' "$got" | sed 's/^/  got: /'; fail=1
    fi
else
    echo "FAIL winspace: wcc64"; fail=1
fi
# More address space than wasm32 has: refused, and not learned as a program
# that needs a 64-bit memory (gexc's mem64 record, docs/mem64.md §5).
if "$ROOT/tools/wine-wasm/wcc64" -O2 -o "$WORK/bigmap" "$HERE/bigmap.c"; then
    rm -rf "$WORK/xdg"
    got=$(cd "$WORK" && XDG_CACHE_HOME="$WORK/xdg" timeout 120 "$GEXC" run --cwd "$WORK" ./bigmap 2>"$WORK/bigmap.err")
    if [ "$got" = "6 GiB of mappings refused: 1" ] && ! [ -d "$WORK/xdg/gex/mem64" ] && ! grep -q "needs more than 4 GiB" "$WORK/bigmap.err"; then
        echo "ok   wasm64/bigmap (no 64-bit memory learned)"
    else
        echo "FAIL wasm64/bigmap"; printf '%s\n' "$got" | sed 's/^/  got: /'; sed 's/^/  stderr: /' "$WORK/bigmap.err" | head -5; fail=1
    fi
else
    echo "FAIL bigmap: wcc64"; fail=1
fi
exit $fail
