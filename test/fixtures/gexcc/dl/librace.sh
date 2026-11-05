#!/bin/sh
# A program's threads loading libraries at once: each thread's Worker
# instantiates the modules the others loaded, copying a library's bytes out
# of the shim, and each, once it has, lets the shim give them up -- which
# freed them under a Worker still copying them ("expected magic word").
# Eight libraries with 1 MiB of initialised data each (so a copy takes a
# while), loaded by dlopen from twelve threads released together, each in
# its own order. In the browser runtime (run-node).
#
#   test/fixtures/gexcc/dl/librace.sh DIR   (gexcc on PATH; GEX_ROOT: a gex checkout
#                                 with page/public/gex built; DIR: where gex
#                                 can see, not /dev/shm)
set -u
command -v gexcc > /dev/null || { echo "gexcc not on PATH (toolchain/gexcc/install.sh)"; exit 2; }
ROOT=${GEX_ROOT:?set GEX_ROOT to a gex checkout}
ARCH=${GEX_ARCH:-x86}
case $ARCH in x86) INFIX=.x86 ;; *) INFIX= ;; esac
SEED=$ROOT/page/public/gex/seed$INFIX.shared.wasm
[ -f "$SEED" ] || { echo "no $SEED"; exit 2; }
W=$(mktemp -d "${1:?DIR}/librace.XXXXXX")
trap 'rm -rf "$W"' EXIT
N=8
LIBDIR=$(dirname "$(command -v gexcc)")/../sysroot/lib
libs=; files="--file /usr/lib/wasm32-linux-musl/libc.so=$LIBDIR/libc.so"
for i in $(seq 1 $N); do
    # initialised data, so the module's bytes are large
    printf 'unsigned char big%d[1 << 20] = { %d, 1, 2, 3 };\nint get%d(void) { return big%d[0] + big%d[(1 << 20) - 1]; }\n' $i $i $i $i $i > "$W/l$i.c"
    GEXCC_SHARED=1 gexcc -O1 -fPIC -shared -o "$W/libl$i.so" "$W/l$i.c" || { echo "FAIL build l$i"; exit 1; }
    libs="$libs -ll$i"; files="$files --file /usr/lib/wasm32-linux-musl/libl$i.so=$W/libl$i.so"
done
cat > "$W/p.c" <<X
#include <dlfcn.h>
#include <pthread.h>
#include <stdio.h>
#define N $N
X
cat >> "$W/p.c" <<'X'
static pthread_barrier_t go;
static void *run(void *a)
{
    long k = (long)a, s = 0;
    pthread_barrier_wait(&go);
    for (int j = 0; j < N; j++) {
        int i = (int)((k + j) % N) + 1;
        char lib[64], fn[16];
        snprintf(lib, sizeof lib, "libl%d.so", i);
        snprintf(fn, sizeof fn, "get%d", i);
        void *h = dlopen(lib, RTLD_NOW);
        if (!h) { printf("dlopen %s: %s\n", lib, dlerror()); return (void *)-1; }
        int (*g)(void) = (int (*)(void))dlsym(h, fn);
        s += g();
    }
    return (void *)s;
}
int main(void)
{
    pthread_t t[12];
    pthread_barrier_init(&go, 0, 12);
    for (long k = 0; k < 12; k++) pthread_create(&t[k], 0, run, (void *)k);
    for (int k = 0; k < 12; k++) { void *v; pthread_join(t[k], &v); if ((long)v != 36) { printf("thread %d: %ld\n", k, (long)v); return 1; } }
    puts("12 threads loaded the libraries at once");
    return 0;
}
X
GEXCC_SHARED=1 gexcc -O1 -pthread -o "$W/p" "$W/p.c" || { echo "FAIL build"; exit 1; }
out=$(GEX_ARCH=$ARCH GEX_SHARED=1 timeout "${TMO:-300}" node "$ROOT/hosts/browser/js/run-node.mjs" $files \
    --file "/t/p=$W/p" --cwd /t --argv0 ./p "$SEED" "$W/p" 2>&1); s=$?
got=$(printf '%s\n' "$out" | grep -v '^\[' | grep -v '^$')
if [ "$got" = "12 threads loaded the libraries at once" ] && [ $s = 0 ]; then echo "ok   librace"; exit 0; fi
echo "FAIL librace (exit $s)"; printf '%s\n' "$out" | grep -v '^\[gex\] loaded' | tail -8
exit 1
