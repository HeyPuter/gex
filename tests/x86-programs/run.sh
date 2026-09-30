#!/bin/sh
# x86 guest programs whose behaviour is checked against native execution:
# each is built static for x86-64 and i386, run natively and under gexc, and
# its output and exit status must match.
#
#   tests/x86-programs/run.sh [gexc]      (default target/x86/release/gexc)
#
# Needs an x86-64 host and zig (for the musl targets).
set -u
HERE=$(cd "$(dirname "$0")" && pwd)
GEXC=${1:-target/x86/release/gexc}
WORK=${WORK:-$(mktemp -d)}
fail=0
for src in "$HERE"/*.c; do
    name=$(basename "$src" .c)
    for target in x86_64-linux-musl x86-linux-musl; do
        extra=""
        [ "$target" = x86-linux-musl ] && extra="-Wl,--image-base=0x1000000"
        bin="$WORK/$name-$target"
        zig cc -target "$target" -O1 -static $extra -o "$bin" "$src" || { fail=1; continue; }
        native=$("$bin" 2>/dev/null); ns=$?
        gex=$("$GEXC" run "$bin" 2>/dev/null); gs=$?
        if [ "$native" = "$gex" ] && [ $ns = $gs ]; then
            echo "ok   $name ($target, exit $ns)"
        else
            echo "FAIL $name ($target): native exit $ns, gex exit $gs"
            printf '%s\n' "$native" > "$WORK/$name-$target.native"
            printf '%s\n' "$gex" > "$WORK/$name-$target.gex"
            diff "$WORK/$name-$target.native" "$WORK/$name-$target.gex" | head -20
            fail=1
        fi
    done
done
exit $fail
