#!/bin/sh
# gexcc programs checked against the host: each NAME.c (NAME.cpp) is built
# with cc (c++) and with gexcc (gexc++), run (directly, or by NAME.sh given the binary), and the output
# and exit status must match. The gexcc build runs itself under gexc (the
# container's host launcher).
#
#   tests/gexcc/run.sh             (gexcc on PATH: tools/gexcc/install.sh)
#   GEXCC_RUNNER=GEXC tests/gexcc/run.sh   to pick the gexc
set -u
HERE=$(cd "$(dirname "$0")" && pwd)
WORK=${WORK:-$(mktemp -d)}
command -v gexcc > /dev/null || { echo "gexcc not on PATH (tools/gexcc/install.sh)"; exit 2; }
fail=0
for src in "$HERE"/*.c "$HERE"/*.cpp; do
    case $src in
        *.cpp) name=$(basename "$src" .cpp); hcc=c++; gcc_=gexc++ ;;
        *) name=$(basename "$src" .c); hcc=cc; gcc_=gexcc ;;
    esac
    $hcc -O1 -w -o "$WORK/$name.native" "$src" -lpthread || { echo "FAIL $name: $hcc"; fail=1; continue; }
    extra=; case $name in fpcast*) extra=-fgex-fpcast-emu ;; esac
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
