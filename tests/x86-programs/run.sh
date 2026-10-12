#!/bin/sh
# x86 guest programs whose behaviour is checked against native execution:
# each is built static for x86-64 and i386, run natively and under gexc, and
# its output and exit status must match.
#
#   tests/x86-programs/run.sh [gexc]      (default target/x86/release/gexc)
#
# Needs an x86-64 host and zig (for the musl targets). The `page` tier runs the
# x86-64 build under the browser shim in Node (run-node.mjs, memory64 shared
# seed) for the programs listed in PAGE_PROGRAMS, when that seed has been built
# (GEX_ARCH=x86 GEX_MEM64=1 shims/browser/build.sh) and node is on PATH.
set -u
HERE=$(cd "$(dirname "$0")" && pwd)
GEXC=${1:-target/x86/release/gexc}
WORK=${WORK:-$(mktemp -d)}
mkdir -p "$WORK"
fail=0
SEED64=web/public/gex/seed.x86.m64.shared.wasm
# tgkill-futex: a signal sent to a thread parked on a futex must end its wait
# (Boehm's stop-the-world; the page asked the wrong thread's signal state).
# exec-tail: code ending in the last bytes of an executable mapping (the page
# fetched only up to four bytes before the end: SIGILL in Mono's JIT code).
# retire-during-lift: code retired while the translator lifts other code
# outside the kernel lock (docs/kernel-locking.md step 5). Tier-1 only: the
# function JIT livelocks on its chain natively (gexc --fjit, 2026-10-12).
# close-while-reading: descriptors read without the kernel lock while other
# threads close and replace them; pipe and socket stress (step 3).
PAGE_PROGRAMS=${PAGE_PROGRAMS:-"tgkill-futex exec-tail mem64-io mem64-smc mem64-reserve retire-during-lift close-while-reading"}
for src in "$HERE"/*.c; do
    name=$(basename "$src" .c)
    # mem64-*: a 64-bit guest memory (docs/mem64.md), x86-64 only.
    gexflags=""
    targets="x86_64-linux-musl x86-linux-musl"
    case $name in mem64-*) gexflags=--mem64; targets=x86_64-linux-musl ;; esac
    # *smc*: self-modifying code, under the function JIT as well (which
    # must leave writable-and-executable code to tier-1's guards).
    tiers="t1"
    case $name in *smc* | copy-loop) tiers="t1 fjit" ;; esac
    case " $PAGE_PROGRAMS " in *" $name "*)
        if [ -f "$SEED64" ] && command -v node >/dev/null; then tiers="$tiers page"; fi ;;
    esac
    for target in $targets; do
    for tier in $tiers; do
        [ $tier = page ] && [ "$target" != x86_64-linux-musl ] && continue
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
        if [ $tier = page ]; then
            env $envs GEX_ARCH=x86 GEX_SHARED=1 GEX_MEM64=1 timeout 120 node shims/browser/js/run-node.mjs "$SEED64" "$bin" >"$WORK/$name.page" 2>/dev/null; gs=$?
            # 125: run-node's flaky "nothing could wake it" after a threaded
            # program has finished (OPTIMIZATION-BACKLOG.md); once more.
            if [ $gs = 125 ]; then
                echo "     $name (page): exit 125, once more"
                env $envs GEX_ARCH=x86 GEX_SHARED=1 GEX_MEM64=1 timeout 120 node shims/browser/js/run-node.mjs "$SEED64" "$bin" >"$WORK/$name.page" 2>/dev/null; gs=$?
            fi
            gex=$(grep -v '^\[host\]' "$WORK/$name.page")
        else
            gex=$(env $envs "$GEXC" run $flags "$bin" 2>/dev/null); gs=$?
        fi
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
