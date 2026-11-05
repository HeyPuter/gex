#!/bin/sh
# An exec from a threaded wasm32 program linked against shared libraries:
# the new program's threads must not bring the old program back. A thread's
# Worker catches up with the process's modules when the thread starts; if
# the old program's were still listed, it instantiated them, and a module
# whose data-initialised flag fell on a zero of the new program's
# initialised its data again at its old place -- the old program's own,
# where the new program's is now (both programs' data at the base after
# their images, which are about the same size). old has an initialised
# array and its flag after it; new a smaller one and zeros after it, where
# old's flag falls. The new program fills its array and its heap,
# starts threads, and checks nothing of either changed. In the browser
# runtime (run-node).
#
#   test/fixtures/gexcc/dl/execdata.sh DIR   (gexcc on PATH; GEX_ROOT: a gex checkout
#                                  with page/public/gex built; DIR: where gex
#                                  can see, not /dev/shm)
set -u
command -v gexcc > /dev/null || { echo "gexcc not on PATH (toolchain/gexcc/install.sh)"; exit 2; }
ROOT=${GEX_ROOT:?set GEX_ROOT to a gex checkout}
ARCH=${GEX_ARCH:-x86}
case $ARCH in x86) INFIX=.x86 ;; *) INFIX= ;; esac
SEED=$ROOT/page/public/gex/seed$INFIX.shared.wasm
[ -f "$SEED" ] || { echo "no $SEED"; exit 2; }
W=$(mktemp -d "${1:?DIR}/execdata.XXXXXX")
trap 'rm -rf "$W"' EXIT
# a library, so both programs are linked against shared ones
cat > "$W/l.c" <<'X'
int l_get(void) { return 1; }
X
cat > "$W/data.h" <<'X'
#define V(x) x, x, x, x, x, x, x, x
#define V64(x) V(x), V(x), V(x), V(x), V(x), V(x), V(x), V(x)
#define DATA { V64(0x11), V64(0x22), V64(0x33), V64(0x44) }
X
cat > "$W/old.c" <<'X'
#include <pthread.h>
#include <stdio.h>
#include <sys/wait.h>
#include <unistd.h>
#include "data.h"
int l_get(void);
unsigned char o_data[69632] = DATA;
static void *th(void *p) { return o_data + l_get(); }
int main(void) {
    pthread_t t;
    pthread_create(&t, 0, th, 0);
    pthread_join(t, 0);
    pid_t c = fork();
    if (c == 0) {
        execl("/t/new", "new", (char *)0);
        _exit(127);
    }
    int s = 0;
    waitpid(c, &s, 0);
    printf("new exited %d\n", WIFEXITED(s) ? WEXITSTATUS(s) : 128 + WTERMSIG(s));
    return 0;
}
X
cat > "$W/new.c" <<'X'
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include "data.h"
int l_get(void);
unsigned char n_data[65536] = DATA;
unsigned char n_zero[131072];
static void *th(void *p) { return p; }
static int changed(const unsigned char *p, size_t n, unsigned char v) {
    size_t k = 0;
    for (size_t i = 0; i < n; i++) k += p[i] != v;
    return (int)k;
}
int main(void) {
    unsigned char *d = n_data + l_get() - 1;
    memset(d, 0x5a, 65536);
    size_t hn = 4 << 20;
    unsigned char *h = malloc(hn);
    memset(h, 0x6b, hn);
    for (int k = 0; k < 3; k++) {
        pthread_t t;
        pthread_create(&t, 0, th, 0);
        pthread_join(t, 0);
        usleep(200000);
    }
    int bd = changed(d, 65536, 0x5a), bh = changed(h, hn, 0x6b);
    printf("library data: %s\n", bd ? "CHANGED" : "kept");
    printf("heap: %s\n", bh ? "CHANGED" : "kept");
    return bd || bh;
}
X
LIBDIR=$(dirname "$(command -v gexcc)")/../sysroot/lib
GEXCC_SHARED=1 gexcc -fPIC -shared -o "$W/libl.so" "$W/l.c" &&
    GEXCC_SHARED=1 gexcc -pthread -I"$W" -o "$W/old" "$W/old.c" -L"$W" -ll &&
    GEXCC_SHARED=1 gexcc -pthread -I"$W" -o "$W/new" "$W/new.c" -L"$W" -ll || { echo "FAIL build"; exit 1; }
out=$(GEX_ARCH=$ARCH GEX_SHARED=1 timeout "${TMO:-300}" \
    node "$ROOT/hosts/browser/js/run-node.mjs" \
    --file "/usr/lib/wasm32-linux-musl/libc.so=$LIBDIR/libc.so" \
    --file "/usr/lib/wasm32-linux-musl/libl.so=$W/libl.so" \
    --file "/t/old=$W/old" --file "/t/new=$W/new" --cwd /t --argv0 ./old \
    "$SEED" "$W/old" 2>&1)
got=$(printf '%s\n' "$out" | grep -v '^\[' | grep -v '^$')
want='library data: kept
heap: kept
new exited 0'
if [ "$got" = "$want" ]; then echo "ok execdata"; exit 0; fi
echo "FAIL execdata"; printf '%s\n' "$out" | tail -20
exit 1
