#!/bin/sh
# A Worker given back to the pool keeps the modules it compiled, and the
# next process it runs must not take one that only fit the last: a wasm32
# program (no dense block numbering) after an AArch64 program that numbered
# densely and made function JIT modules, in turn, four rounds, each wasm32
# program starting a thread on a spare. Before, the wasm32 program's Worker
# took the AArch64 process's function table runtime, which imports
# `blocks`, and threw a LinkError; and an AArch64 process took another's
# JIT module by its number and hung. In the browser runtime (run-node,
# AArch64 shim, GEX_FJIT=1, every program given a spare).
#
#   test/fixtures/gexcc/dl/poolflavour.sh DIR   (gexcc and zig on PATH; GEX_ROOT: a gex
#                                     checkout with page/public/gex built;
#                                     A64_ROOTFS as tests/browser-reuse.sh;
#                                     DIR: where gex can see, not /dev/shm)
set -u
command -v gexcc > /dev/null || { echo "skip poolflavour: gexcc not on PATH (toolchain/gexcc/install.sh)"; exit 0; }
command -v zig > /dev/null || { echo "skip poolflavour: no zig"; exit 0; }
ROOT=${GEX_ROOT:?set GEX_ROOT to a gex checkout}
SEED=$ROOT/page/public/gex/seed.shared.wasm
A64_ROOTFS=${A64_ROOTFS:-$HOME/gex-sandbox/scratch/gate/a64-base.tar.zst}
[ -f "$SEED" ] && [ -f "$A64_ROOTFS" ] || { echo "skip poolflavour: no $SEED or $A64_ROOTFS"; exit 0; }
W=$(mktemp -d "${1:?DIR}/poolflavour.XXXXXX")
trap 'rm -rf "$W"' EXIT
zstd -dc "$A64_ROOTFS" | tar -x -C "$W" ./usr/lib/aarch64-linux-gnu/ld-linux-aarch64.so.1 \
    ./usr/lib/aarch64-linux-gnu/libc.so.6 || { echo "FAIL poolflavour: rootfs"; exit 1; }
cat > "$W/x.c" <<'X'
#include <stdio.h>
int main(void) { puts("x"); return 0; }
X
cat > "$W/m.c" <<'X'
#include <pthread.h>
#include <stdio.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>
static void *th(void *a) { return (void *)((long)a + 1); }
static int run(char *const argv[]) {
    pid_t c = fork();
    if (c == 0) { execv(argv[0], argv); _exit(127); }
    int s = 0;
    waitpid(c, &s, 0);
    return WIFEXITED(s) ? WEXITSTATUS(s) : 128 + WTERMSIG(s);
}
int main(int argc, char **argv) {
    if (argc > 1 && !strcmp(argv[1], "thread")) {
        void *r = 0;
        pthread_t t;
        if (pthread_create(&t, 0, th, (void *)41L) || pthread_join(t, &r)) return 2;
        return r == (void *)42L ? 0 : 3;
    }
    int bad = 0;
    for (int k = 0; k < 4; k++) {
        int a = run((char *const[]){"/t/x", 0});
        int b = run((char *const[]){"/t/m", "thread", 0});
        if (a || b) { printf("round %d: AArch64 program %d, wasm32 program %d\n", k, a, b); bad = 1; }
    }
    printf("%s\n", bad ? "FAIL" : "ok");
    return bad;
}
X
LIBDIR=$(dirname "$(command -v gexcc)")/../sysroot/lib
zig cc -target aarch64-linux-gnu.2.36 -O1 -o "$W/x" "$W/x.c" &&
    GEXCC_SHARED=1 gexcc -pthread -o "$W/m" "$W/m.c" || { echo "FAIL poolflavour: build"; exit 1; }
out=$(GEX_FJIT=1 GEX_TEST_SPARE_MODULES=1 GEX_SHARED=1 timeout "${TMO:-300}" \
    node "$ROOT/hosts/browser/js/run-node.mjs" \
    --file "/usr/lib/wasm32-linux-musl/libc.so=$LIBDIR/libc.so" \
    --file /lib/ld-linux-aarch64.so.1="$W/usr/lib/aarch64-linux-gnu/ld-linux-aarch64.so.1" \
    --file /lib/aarch64-linux-gnu/libc.so.6="$W/usr/lib/aarch64-linux-gnu/libc.so.6" \
    --file "/t/m=$W/m" --file "/t/x=$W/x" --cwd /t --argv0 /t/m "$SEED" "$W/m" 2>&1)
rc=$?
last=$(printf '%s\n' "$out" | grep -v '^\[' | grep -v '^$' | tail -1)
if [ $rc = 0 ] && [ "$last" = ok ]; then
    echo "ok poolflavour"
else
    echo "FAIL poolflavour: exit $rc, the program said '$last'"
    printf '%s\n' "$out" | grep -v 'fjit:\|missing unit' | tail -8
    exit 1
fi
