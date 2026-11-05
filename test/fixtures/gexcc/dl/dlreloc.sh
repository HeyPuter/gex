#!/bin/sh
# A library's function pointers in data (a C++ vtable's shape) as the
# thread that dlopened it first uses them, while other threads of the
# process, making system calls, instantiate the library as they catch up:
# one of those may apply its data relocations, and the dlopening thread
# ran the library before they were applied (pointers still offsets into its
# own table range: another module's function, or a signature mismatch).
# Twelve libraries of 4000 pointers each, opened in turn, while eight
# threads loop on a system call. In the browser runtime (run-node).
#
#   test/fixtures/gexcc/dl/dlreloc.sh DIR   (gexcc on PATH; GEX_ROOT: a gex checkout
#                                 with page/public/gex built; DIR: where gex
#                                 can see, not /dev/shm)
set -u
command -v gexcc > /dev/null || { echo "gexcc not on PATH (toolchain/gexcc/install.sh)"; exit 2; }
ROOT=${GEX_ROOT:?set GEX_ROOT to a gex checkout}
ARCH=${GEX_ARCH:-x86}
case $ARCH in x86) INFIX=.x86 ;; *) INFIX= ;; esac
SEED=$ROOT/page/public/gex/seed$INFIX.shared.wasm
[ -f "$SEED" ] || { echo "no $SEED"; exit 2; }
W=$(mktemp -d "${1:?DIR}/dlreloc.XXXXXX")
trap 'rm -rf "$W"' EXIT
N=12 F=4000
LIBDIR=$(dirname "$(command -v gexcc)")/../sysroot/lib
files="--file /usr/lib/wasm32-linux-musl/libc.so=$LIBDIR/libc.so"
for i in $(seq 1 $N); do
    { for f in $(seq 1 $F); do printf 'static long v%d(long x) { return x + %d; }\n' $f $f; done
      printf 'long (*const tab%d[])(long) = {\n' $i
      for f in $(seq 1 $F); do printf ' v%d,\n' $f; done
      printf '};\nlong (*const *gettab%d(void))(long) { return tab%d; }\n' $i $i; } > "$W/r$i.c"
    GEXCC_SHARED=1 gexcc -O1 -fPIC -shared -o "$W/libr$i.so" "$W/r$i.c" || { echo "FAIL build r$i"; exit 1; }
    files="$files --file /usr/lib/wasm32-linux-musl/libr$i.so=$W/libr$i.so"
done
cat > "$W/m.c" <<X
#include <dlfcn.h>
#include <pthread.h>
#include <stdio.h>
#include <unistd.h>
#define N $N
#define F $F
X
cat >> "$W/m.c" <<'X'
static volatile int stop;
static void *spin(void *a) {
    while (!stop) getppid();
    return a;
}
int main(void) {
    pthread_t t[8];
    for (int k = 0; k < 8; k++) pthread_create(&t[k], 0, spin, 0);
    usleep(300000);
    int bad = 0;
    for (int i = 1; i <= N && !bad; i++) {
        char lib[32], fn[16];
        snprintf(lib, sizeof lib, "libr%d.so", i); snprintf(fn, sizeof fn, "gettab%d", i);
        void *h = dlopen(lib, RTLD_NOW);
        if (!h) { printf("FAIL dlopen %s: %s\n", lib, dlerror()); bad = 1; break; }
        long (*const *(*g)(void))(long) = (long (*const *(*)(void))(long))dlsym(h, fn);
        long (*const *tab)(long) = g();
        for (int f = 0; f < F; f++) {
            long got = tab[f](0);
            if (got != f + 1) { printf("FAIL %s: pointer %d gave %ld, want %d\n", lib, f, got, f + 1); bad = 1; break; }
        }
    }
    stop = 1;
    for (int k = 0; k < 8; k++) pthread_join(t[k], 0);
    printf("%s\n", bad ? "FAIL" : "ok");
    return bad;
}
X
GEXCC_SHARED=1 gexcc -O1 -pthread -o "$W/m" "$W/m.c" || { echo "FAIL build"; exit 1; }
out=$(GEX_ARCH=$ARCH GEX_SHARED=1 timeout "${TMO:-300}" node "$ROOT/hosts/browser/js/run-node.mjs" $files \
    --file "/t/m=$W/m" --cwd /t --argv0 ./m "$SEED" "$W/m" 2>&1); s=$?
last=$(printf '%s\n' "$out" | grep -v '^\[' | grep -v '^$' | tail -1)
if [ "$last" = ok ] && [ $s = 0 ]; then echo "ok dlreloc"; exit 0; fi
echo "FAIL dlreloc (exit $s)"; printf '%s\n' "$out" | grep -v '^\[gex\] loaded' | grep -v "gex:   \|^\[gex-console-error\] *at \|    at " | tail -12
exit 1
