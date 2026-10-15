#!/bin/sh
# A process's Worker given back to the pool, in the page (host.mjs
# `#giveBack`, worker.mjs `reset`): tests/x86-programs/reuse-state.c starts
# processes that leave state behind (an interval timer, signal handlers, a
# umask, a cwd, a thread parked reading a pipe) and checks that the
# processes after them see none of it, started by posix_spawn, vfork + exec,
# fork + exec and an exec from a second thread. The output must be the
# native run's, and some of those processes must have started on given-back
# Workers -- none in the single-thread build (GEX_SINGLE=1), whose processes
# are fibers of one realm.
#
#   tests/browser-reuse.sh [x86] [a64] [sp]      (default all three)
#
# x86-64 and SP run against the host's glibc; AArch64 against the demo
# rootfs's (A64_ROOTFS, default web/dist-demo/rootfs/base.tar.zst), built
# with zig. Needs the page's shared builds: `GEX_ARCH=x86 GEX_MEM64=1
# shims/browser/build.sh` and `shims/browser/build.sh`. A missing piece is a
# skip, not a pass.
set -u
cd "$(dirname "$0")/.."
SRC=tests/x86-programs/reuse-state.c
ROUNDS=${ROUNDS:-6}
WORK=${WORK:-$(mktemp -d)}
mkdir -p "$WORK"
A64_ROOTFS=${A64_ROOTFS:-web/dist-demo/rootfs/base.tar.zst}
fail=0
want="reuse-state ok $ROUNDS"

# check LABEL TRACEFILE OUTFILE STATUS SINGLE
check() {
    got=$(grep -v '^\[' "$3")
    back=$(grep -c '^\[pool\] pid [0-9]*: given-back worker' "$2" 2>/dev/null)
    if [ "$5" = 1 ]; then ok=$([ "${back:-0}" = 0 ] && echo 1); else ok=$([ "${back:-0}" -ge 1 ] && echo 1); fi
    if [ "$4" = 0 ] && [ "$got" = "$want" ] && [ "$ok" = 1 ]; then
        echo "ok   reuse-state ($1, page, $ROUNDS rounds, ${back:-0} processes on given-back workers)"
    else
        echo "FAIL reuse-state ($1, page): exit $4, ${back:-0} processes on given-back workers"
        printf '%s\n' "$got" | head -10
        fail=1
    fi
}

# The single-thread build needs JSPI (on by default from node 25, behind a
# flag before) and runs with no SharedArrayBuffer, as the SP page does.
jspi=
node -e 'process.exit(typeof WebAssembly.Suspending === "function" ? 0 : 1)' 2>/dev/null || jspi=--experimental-wasm-jspi

x86() { # LABEL SINGLE
    if [ "$2" = 1 ]; then seed=web/public/gex/seed.x86.shared.wasm; s=1; m64=
    else seed=web/public/gex/seed.x86.m64.shared.wasm; s=; m64=1; fi
    ld=/lib64/ld-linux-x86-64.so.2
    libc=/lib/x86_64-linux-gnu/libc.so.6
    if [ ! -f "$seed" ] || [ ! -e "$ld" ] || ! command -v gcc >/dev/null; then
        echo "skip reuse-state ($1, page): no seed, gcc or glibc"; return
    fi
    bin="$WORK/reuse-state-x86"
    gcc -O1 -pthread -o "$bin" "$SRC" || { fail=1; return; }
    rm -f "$WORK/$1.trace"
    env ${s:+GEX_SINGLE=1} ${m64:+GEX_MEM64=1} GEX_VFORK_TRACE="$WORK/$1.trace" GEX_ARCH=x86 GEX_SHARED=1 \
        timeout 300 node $jspi \
        --import 'data:text/javascript,if(process.env.GEX_SINGLE)delete globalThis.SharedArrayBuffer' \
        shims/browser/js/run-node.mjs --file /bin/reuse-state="$bin" \
        --file "$ld=$(readlink -f $ld)" --file "$libc=$(readlink -f $libc)" \
        --argv0 /bin/reuse-state "$seed" "$bin" "$ROUNDS" >"$WORK/$1.out" 2>"$WORK/$1.err"
    check "$1" "$WORK/$1.trace" "$WORK/$1.out" $? "$2"
}

for arch in ${*:-x86 a64 sp}; do
    case $arch in
    x86) x86 x86 0 ;;
    sp) x86 sp 1 ;;
    a64)
        seed=web/public/gex/seed.shared.wasm
        if [ ! -f "$seed" ] || [ ! -f "$A64_ROOTFS" ] || ! command -v zig >/dev/null; then
            echo "skip reuse-state (a64, page): no seed, rootfs or zig"; continue
        fi
        root="$WORK/a64root"
        mkdir -p "$root"
        zstd -dc "$A64_ROOTFS" | tar -x -C "$root" ./usr/lib/aarch64-linux-gnu/ld-linux-aarch64.so.1 \
            ./usr/lib/aarch64-linux-gnu/libc.so.6 || { fail=1; continue; }
        bin="$WORK/reuse-state-a64"
        zig cc -target aarch64-linux-gnu.2.36 -O1 -pthread -o "$bin" "$SRC" || { fail=1; continue; }
        rm -f "$WORK/a64.trace"
        GEX_VFORK_TRACE="$WORK/a64.trace" GEX_SHARED=1 timeout 300 \
            node shims/browser/js/run-node.mjs --file /bin/reuse-state="$bin" \
            --file /lib/ld-linux-aarch64.so.1="$root/usr/lib/aarch64-linux-gnu/ld-linux-aarch64.so.1" \
            --file /lib/aarch64-linux-gnu/libc.so.6="$root/usr/lib/aarch64-linux-gnu/libc.so.6" \
            --argv0 /bin/reuse-state "$seed" "$bin" "$ROUNDS" >"$WORK/a64.out" 2>"$WORK/a64.err"
        check a64 "$WORK/a64.trace" "$WORK/a64.out" $? 0
        ;;
    esac
done
exit $fail
