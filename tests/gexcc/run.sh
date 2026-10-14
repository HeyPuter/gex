#!/bin/sh
# gexcc programs checked against the host: each NAME.c (NAME.cpp) is built
# with cc (c++) and with gexcc (gexc++), run (directly, or by NAME.sh given the binary), and the output
# and exit status must match. The gexcc build runs itself under gexc (the
# container's host launcher).
#
#   tests/gexcc/run.sh [GEXC]      (gexcc on PATH: tools/gexcc/install.sh)
#
# GEXC, or the GEXC environment variable, is the gexc the programs run
# under; without either, the one the gexcc install names (gexcc-exec's
# default, GEXCC_GEXC). A gexcc program is an ELF launcher that execs
# gexcc-exec, which reads GEXCC_GEXC -- not GEXC -- so a gate that set only
# GEXC tested whatever gexc the install was made with (wt/gexcc's, three
# days stale: sarestart, sockclose and sockpair "failed" on fixes it did
# not have).
set -u
G=${1:-${GEXC:-}}
if [ -n "$G" ]; then
    [ -x "$G" ] || { echo "no gexc at $G"; exit 2; }
    GEXCC_GEXC=$(cd "$(dirname "$G")" && pwd)/$(basename "$G")
    export GEXCC_GEXC
    echo "gexc: $GEXCC_GEXC"
fi
HERE=$(cd "$(dirname "$0")" && pwd)
WORK=${WORK:-$(mktemp -d)}
command -v gexcc > /dev/null || { echo "gexcc not on PATH (tools/gexcc/install.sh)"; exit 2; }
# GEXCC_SHARED=1: every program linked against libc.so (docs/gexcc.md 7),
# found through LD_LIBRARY_PATH (the host's root is the guest's here)
if [ "${GEXCC_SHARED:-0}" != 0 ]; then
    export LD_LIBRARY_PATH="$(gexcc --print-sysroot)/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
fi
fail=0
for src in "$HERE"/*.c "$HERE"/*.cpp; do
    case $src in
        *.cpp) name=$(basename "$src" .cpp); hcc=c++; gcc_=gexc++ ;;
        *) name=$(basename "$src" .c); hcc=cc; gcc_=gexcc ;;
    esac
    $hcc -O1 -w -o "$WORK/$name.native" "$src" -lpthread || { echo "FAIL $name: $hcc"; fail=1; continue; }
    extra=; case $name in fpcast*) extra=-fgex-fpcast-emu ;; esac
    # (function pointer cast emulation is static-only)
    case $name in fpcast*) [ "${GEXCC_SHARED:-0}" != 0 ] && extra="$extra -static" ;; esac
    $gcc_ -O2 -w $extra -o "$WORK/$name" "$src" || { echo "FAIL $name: $gcc_"; fail=1; continue; }
    if [ -f "$HERE/$name.sh" ]; then
        native=$(cd "$WORK" && sh "$HERE/$name.sh" "./$name.native" 2>&1); ns=$?
        gex=$(cd "$WORK" && sh "$HERE/$name.sh" "./$name" 2>&1); gs=$?
    else
        native=$(cd "$WORK" && "./$name.native" 2>&1); ns=$?
        gex=$(cd "$WORK" && "./$name" 2>&1); gs=$?
    fi
    if [ "$native" = "$gex" ] && [ $ns = $gs ]; then
        echo "ok   $name"
    else
        echo "FAIL $name: native exit $ns, gex exit $gs"
        printf '%s\n' "$native" > "$WORK/$name.out-native"
        printf '%s\n' "$gex" > "$WORK/$name.out-gex"
        diff "$WORK/$name.out-native" "$WORK/$name.out-gex" | head -20
        fail=1
    fi
done
exit $fail
