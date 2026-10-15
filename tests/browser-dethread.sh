#!/bin/sh
# An exec from a second thread, in the page (Linux's de_thread,
# shims/browser/src/lib.rs `exec_waits`): tests/x86-programs/exec-dethread.c
# execs from a new thread while the first thread joins it, spins in
# translated code, blocks reading a pipe, or waits on a futex, each ROUNDS
# times. Every one of those processes' new program must start and exit 0;
# with the old image replaced under a running thread, that thread ran the
# old code over the new one.
#
#   tests/browser-dethread.sh [x86] [a64] [sp]      (default all three)
#
# x86-64 and SP run against the host's glibc; AArch64 against the demo
# rootfs's (A64_ROOTFS, default web/dist-demo/rootfs/base.tar.zst), built
# with zig. Needs the page's shared builds: `GEX_ARCH=x86 GEX_MEM64=1
# shims/browser/build.sh` and `shims/browser/build.sh`. A missing piece is a
# skip, not a pass.
set -u
cd "$(dirname "$0")/.."
SRC=tests/x86-programs/exec-dethread.c
ROUNDS=${ROUNDS:-20}
# Fewer in the single-thread build, whose spinning thread holds the one
# realm for long slices (about 20 s a spin case, before this fix as after).
SP_ROUNDS=${SP_ROUNDS:-2}
WORK=${WORK:-$(mktemp -d)}
mkdir -p "$WORK"
A64_ROOTFS=${A64_ROOTFS:-web/dist-demo/rootfs/base.tar.zst}
fail=0

# check LABEL OUTFILE STATUS ERRFILE
check() {
    want="exec-dethread ok $rounds"
    got=$(grep -v '^\[' "$2")
    if [ "$3" = 0 ] && [ "$got" = "$want" ]; then
        echo "ok   exec-dethread ($1, page, $rounds rounds of 4 cases)"
    else
        echo "FAIL exec-dethread ($1, page): exit $3"
        printf '%s\n' "$got" | head -10
        grep -m3 'nothing to translate\|went ahead after' "$4"
        fail=1
    fi
}

# The single-thread build needs JSPI (on by default from node 25, behind a
# flag before) and runs with no SharedArrayBuffer, as the SP page does.
jspi=
node -e 'process.exit(typeof WebAssembly.Suspending === "function" ? 0 : 1)' 2>/dev/null || jspi=--experimental-wasm-jspi

x86() { # LABEL SINGLE
    if [ "$2" = 1 ]; then seed=web/public/gex/seed.x86.shared.wasm; s=1; m64=; rounds=$SP_ROUNDS
    else seed=web/public/gex/seed.x86.m64.shared.wasm; s=; m64=1; rounds=$ROUNDS; fi
    ld=/lib64/ld-linux-x86-64.so.2
    libc=/lib/x86_64-linux-gnu/libc.so.6
    if [ ! -f "$seed" ] || [ ! -e "$ld" ] || ! command -v gcc >/dev/null; then
        echo "skip exec-dethread ($1, page): no seed, gcc or glibc"; return
    fi
    bin="$WORK/exec-dethread-x86"
    gcc -O1 -pthread -o "$bin" "$SRC" || { fail=1; return; }
    env ${s:+GEX_SINGLE=1} ${m64:+GEX_MEM64=1} GEX_ARCH=x86 GEX_SHARED=1 \
        timeout 600 node $jspi \
        --import 'data:text/javascript,if(process.env.GEX_SINGLE)delete globalThis.SharedArrayBuffer' \
        shims/browser/js/run-node.mjs --file /bin/exec-dethread="$bin" \
        --file "$ld=$(readlink -f $ld)" --file "$libc=$(readlink -f $libc)" \
        --argv0 /bin/exec-dethread "$seed" "$bin" "$rounds" >"$WORK/$1.out" 2>"$WORK/$1.err"
    check "$1" "$WORK/$1.out" $? "$WORK/$1.err"
}

for arch in ${*:-x86 a64 sp}; do
    case $arch in
    x86) x86 x86 0 ;;
    sp) x86 sp 1 ;;
    a64)
        seed=web/public/gex/seed.shared.wasm
        if [ ! -f "$seed" ] || [ ! -f "$A64_ROOTFS" ] || ! command -v zig >/dev/null; then
            echo "skip exec-dethread (a64, page): no seed, rootfs or zig"; continue
        fi
        root="$WORK/a64root"
        mkdir -p "$root"
        zstd -dc "$A64_ROOTFS" | tar -x -C "$root" ./usr/lib/aarch64-linux-gnu/ld-linux-aarch64.so.1 \
            ./usr/lib/aarch64-linux-gnu/libc.so.6 || { fail=1; continue; }
        bin="$WORK/exec-dethread-a64"
        zig cc -target aarch64-linux-gnu.2.36 -O1 -pthread -o "$bin" "$SRC" || { fail=1; continue; }
        rounds=$ROUNDS
        GEX_SHARED=1 timeout 600 \
            node shims/browser/js/run-node.mjs --file /bin/exec-dethread="$bin" \
            --file /lib/ld-linux-aarch64.so.1="$root/usr/lib/aarch64-linux-gnu/ld-linux-aarch64.so.1" \
            --file /lib/aarch64-linux-gnu/libc.so.6="$root/usr/lib/aarch64-linux-gnu/libc.so.6" \
            --argv0 /bin/exec-dethread "$seed" "$bin" "$rounds" >"$WORK/a64.out" 2>"$WORK/a64.err"
        check a64 "$WORK/a64.out" $? "$WORK/a64.err"
        ;;
    esac
done
exit $fail
