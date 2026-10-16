#!/bin/sh
# Timed waits in the page that only their deadline ends
# (tests/x86-programs/poll-timeout.c): poll, ppoll, select and epoll_wait on a
# pipe nobody writes, 100 ms each, in a main thread, a second thread and a
# forked child. Such a wait gives the stack back to the run loop by a trap,
# and the trap left the syscall's clock scope (`NowScope`) open: the clock
# stayed at the call's own reading and the deadline never came.
#
#   tests/browser-poll-timeout.sh [x86] [a64] [sp]      (default all three)
#
# Static builds: gcc for x86-64 and SP, zig for AArch64. Needs the page's
# shared builds: `GEX_ARCH=x86 GEX_MEM64=1 shims/browser/build.sh` and
# `shims/browser/build.sh`. A missing piece is a skip, not a pass.
set -u
cd "$(dirname "$0")/.."
SRC=tests/x86-programs/poll-timeout.c
WORK=${WORK:-$(mktemp -d)}
mkdir -p "$WORK"
fail=0
jspi=
node -e 'process.exit(typeof WebAssembly.Suspending === "function" ? 0 : 1)' 2>/dev/null || jspi=--experimental-wasm-jspi

# run LABEL SEED BIN ENV...
run() {
    label=$1 seed=$2 bin=$3
    shift 3
    env "$@" timeout 120 node $jspi \
        --import 'data:text/javascript,if(process.env.GEX_SINGLE)delete globalThis.SharedArrayBuffer' \
        shims/browser/js/run-node.mjs "$seed" "$bin" </dev/null >"$WORK/$label.out" 2>"$WORK/$label.err"
    st=$?
    got=$(grep -v '^\[' "$WORK/$label.out")
    if [ $st = 0 ] && [ "$got" = "poll-timeout ok" ]; then
        echo "ok   poll-timeout ($label, page)"
    else
        echo "FAIL poll-timeout ($label, page): exit $st"
        printf '%s\n' "$got" | head -10
        fail=1
    fi
}

for arch in ${*:-x86 a64 sp}; do
    case $arch in
    x86 | sp)
        if [ $arch = x86 ]; then seed=web/public/gex/seed.x86.m64.shared.wasm; else seed=web/public/gex/seed.x86.shared.wasm; fi
        if [ ! -f "$seed" ] || ! command -v gcc >/dev/null; then
            echo "skip poll-timeout ($arch, page): no seed or gcc"; continue
        fi
        bin="$WORK/poll-timeout-x86"
        [ -x "$bin" ] || gcc -O1 -static -pthread -o "$bin" "$SRC" || { fail=1; continue; }
        if [ $arch = x86 ]; then run x86 "$seed" "$bin" GEX_ARCH=x86 GEX_MEM64=1 GEX_SHARED=1
        else run sp "$seed" "$bin" GEX_ARCH=x86 GEX_SHARED=1 GEX_SINGLE=1; fi
        ;;
    a64)
        seed=web/public/gex/seed.shared.wasm
        if [ ! -f "$seed" ] || ! command -v zig >/dev/null; then
            echo "skip poll-timeout (a64, page): no seed or zig"; continue
        fi
        bin="$WORK/poll-timeout-a64"
        zig cc -target aarch64-linux-musl -static -O1 -pthread -o "$bin" "$SRC" || { fail=1; continue; }
        run a64 "$seed" "$bin" GEX_SHARED=1
        ;;
    esac
done
exit $fail
