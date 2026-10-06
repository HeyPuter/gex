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
mkdir -p "$WORK"
fail=0
for src in "$HERE"/*.c; do
    name=$(basename "$src" .c)
    # mem64-*: a 64-bit guest memory (docs/mem64.md), x86-64 only.
    gexflags=""
    targets="x86_64-linux-musl x86-linux-musl"
    case $name in mem64-*) gexflags=--mem64; targets=x86_64-linux-musl ;; esac
    # *smc*: self-modifying code, under the function JIT as well (which
    # must leave writable-and-executable code to tier-1's guards).
    tiers="t1"
    case $name in *smc*) tiers="t1 fjit" ;; esac
    for target in $targets; do
    for tier in $tiers; do
        flags=$gexflags
        [ $tier = fjit ] && flags="$flags --fjit"
        extra=""
        [ "$target" = x86-linux-musl ] && extra="-Wl,--image-base=0x1000000"
        bin="$WORK/$name-$target"
        [ -x "$bin" ] || zig cc -target "$target" -O1 -static $extra -o "$bin" "$src" || { fail=1; continue; }
        native=$("$bin" 2>/dev/null); ns=$?
        # smc-race: the translator waits between decoding a JIT-code block
        # and fixing its guard, so the writer's stores land in between.
        envs=""
        [ $name = smc-race ] && envs="GEX_TEST_LIFT_DELAY_US=300"
        gex=$(env $envs "$GEXC" run $flags "$bin" 2>/dev/null); gs=$?
        if [ "$native" = "$gex" ] && [ $ns = $gs ]; then
            echo "ok   $name ($target, $tier, exit $ns)"
        else
            echo "FAIL $name ($target, $tier): native exit $ns, gex exit $gs"
            printf '%s\n' "$native" > "$WORK/$name-$target.native"
            printf '%s\n' "$gex" > "$WORK/$name-$target.gex"
            diff "$WORK/$name-$target.native" "$WORK/$name-$target.gex" | head -20
            fail=1
        fi
    done
    done
done
exit $fail
