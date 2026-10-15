#!/bin/sh
# glibc's posix_spawn in the page, x86-64 and AArch64: tests/x86-programs/
# spawn-threads.c, dynamically linked, from a parent of four threads. glibc
# clones with clone3(CLONE_VM | CLONE_VFORK) onto a stack of its own; the
# page runs the child in the calling thread's Worker until its exec
# (shims/browser/src/vfork.rs). The output must be the native run's, and
# every child (24 spawns and a missing program) must have run in place
# rather than as a copied fork.
#
#   tests/browser-spawn.sh [x86] [a64]      (default both)
#
# x86-64 runs against the host's glibc (gcc, /lib64/ld-linux-x86-64.so.2);
# AArch64 against the demo rootfs's (A64_ROOTFS, default
# web/dist-demo/rootfs/base.tar.zst), built with zig. Needs the page's
# shared builds: `GEX_ARCH=x86 GEX_MEM64=1 shims/browser/build.sh` and
# `shims/browser/build.sh`. A missing piece is a skip, not a pass.
set -u
cd "$(dirname "$0")/.."
SRC=tests/x86-programs/spawn-threads.c
WORK=${WORK:-$(mktemp -d)}
mkdir -p "$WORK"
A64_ROOTFS=${A64_ROOTFS:-web/dist-demo/rootfs/base.tar.zst}
fail=0
want="24 of 24 children right
parent: fd5 open, usr2 handled, usr1 open
missing program: ENOENT
3 of 3 other threads ran"

# check LABEL TRACEFILE OUTFILE STATUS
check() {
    got=$(grep -v '^\[' "$3")
    inplace=$(grep -c '^\[vfork\] pid [0-9]*: \(exec\|exit\)' "$2" 2>/dev/null)
    if [ "$4" = 0 ] && [ "$got" = "$want" ] && [ "${inplace:-0}" = 25 ]; then
        echo "ok   spawn-threads ($1, page, 25 children in place)"
    else
        echo "FAIL spawn-threads ($1, page): exit $4, ${inplace:-0} of 25 children in place"
        printf '%s\n' "$got" | head -10
        fail=1
    fi
}

for arch in ${*:-x86 a64}; do
    case $arch in
    x86)
        seed=web/public/gex/seed.x86.m64.shared.wasm
        ld=/lib64/ld-linux-x86-64.so.2
        libc=/lib/x86_64-linux-gnu/libc.so.6
        if [ ! -f "$seed" ] || [ ! -e "$ld" ] || ! command -v gcc >/dev/null; then
            echo "skip spawn-threads (x86, page): no seed, gcc or glibc"; continue
        fi
        bin="$WORK/spawn-threads-x86"
        gcc -O1 -o "$bin" "$SRC" || { fail=1; continue; }
        rm -f "$WORK/x86.trace"
        GEX_VFORK_TRACE="$WORK/x86.trace" GEX_ARCH=x86 GEX_SHARED=1 GEX_MEM64=1 timeout 180 \
            node shims/browser/js/run-node.mjs --file /bin/spawn-threads="$bin" \
            --file "$ld=$(readlink -f $ld)" --file "$libc=$(readlink -f $libc)" \
            --argv0 /bin/spawn-threads "$seed" "$bin" >"$WORK/x86.out" 2>/dev/null
        check x86 "$WORK/x86.trace" "$WORK/x86.out" $?
        ;;
    a64)
        seed=web/public/gex/seed.shared.wasm
        if [ ! -f "$seed" ] || [ ! -f "$A64_ROOTFS" ] || ! command -v zig >/dev/null; then
            echo "skip spawn-threads (a64, page): no seed, rootfs or zig"; continue
        fi
        root="$WORK/a64root"
        mkdir -p "$root"
        zstd -dc "$A64_ROOTFS" | tar -x -C "$root" ./usr/lib/aarch64-linux-gnu/ld-linux-aarch64.so.1 \
            ./usr/lib/aarch64-linux-gnu/libc.so.6 || { fail=1; continue; }
        bin="$WORK/spawn-threads-a64"
        zig cc -target aarch64-linux-gnu.2.36 -O1 -o "$bin" "$SRC" || { fail=1; continue; }
        rm -f "$WORK/a64.trace"
        GEX_VFORK_TRACE="$WORK/a64.trace" GEX_SHARED=1 timeout 180 \
            node shims/browser/js/run-node.mjs --file /bin/spawn-threads="$bin" \
            --file /lib/ld-linux-aarch64.so.1="$root/usr/lib/aarch64-linux-gnu/ld-linux-aarch64.so.1" \
            --file /lib/aarch64-linux-gnu/libc.so.6="$root/usr/lib/aarch64-linux-gnu/libc.so.6" \
            --argv0 /bin/spawn-threads "$seed" "$bin" >"$WORK/a64.out" 2>/dev/null
        check a64 "$WORK/a64.trace" "$WORK/a64.out" $?
        ;;
    esac
done
exit $fail
