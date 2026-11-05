#!/bin/sh
# Library constructors that dlopen runs: every library's, once. Two threads
# loading at once (a second thread loads big libraries while the first loads
# small ones, each with a constructor that records it ran), which ran the
# other thread's list or none in place of the caller's; and a constructor
# that itself loads a library, in a library loaded with one it needs, which
# left the second's constructor out. In the browser runtime (run-node).
#
#   test/fixtures/gexcc/dl/dlctors.sh DIR   (gexcc on PATH; GEX_ROOT: a gex checkout
#                                 with page/public/gex built; DIR: where gex
#                                 can see, not /dev/shm)
set -u
command -v gexcc > /dev/null || { echo "gexcc not on PATH (toolchain/gexcc/install.sh)"; exit 2; }
ROOT=${GEX_ROOT:?set GEX_ROOT to a gex checkout}
ARCH=${GEX_ARCH:-x86}
case $ARCH in x86) INFIX=.x86 ;; *) INFIX= ;; esac
SEED=$ROOT/page/public/gex/seed$INFIX.shared.wasm
[ -f "$SEED" ] || { echo "no $SEED"; exit 2; }
W=$(mktemp -d "${1:?DIR}/dlctors.XXXXXX")
trap 'rm -rf "$W"' EXIT
N=12 B=12
LIBDIR=$(dirname "$(command -v gexcc)")/../sysroot/lib
files="--file /usr/lib/wasm32-linux-musl/libc.so=$LIBDIR/libc.so"
build() { GEXCC_SHARED=1 gexcc -O1 -fPIC -shared "$@" || { echo "FAIL build $*"; exit 1; }; }
for i in $(seq 1 $N); do
    { for f in $(seq 1 3000); do printf 'int s%d_%d(int x) { return x * %d + %d; }\n' $i $f $f $i; done
      printf 'static volatile int ran;\n__attribute__((constructor)) static void init(void) { ran++; }\nint ran%d(void) { return ran + s%d_1(0) - %d; }\n' $i $i $i; } > "$W/s$i.c"
    build -o "$W/libs$i.so" "$W/s$i.c"; files="$files --file /usr/lib/wasm32-linux-musl/libs$i.so=$W/libs$i.so"
done
for i in $(seq 1 $B); do
    # many functions, so a load takes a while
    { for f in $(seq 1 3000); do printf 'int b%d_%d(int x) { return x * %d + %d; }\n' $i $f $f $i; done
      printf 'static volatile int ran;\n__attribute__((constructor)) static void init(void) { ran++; }\nint ranb%d(void) { return ran + b%d_1(0) - %d; }\n' $i $i $i; } > "$W/b$i.c"
    build -o "$W/libb$i.so" "$W/b$i.c"; files="$files --file /usr/lib/wasm32-linux-musl/libb$i.so=$W/libb$i.so"
done
# a constructor loading a library, in a library another one needs
printf 'static volatile int ran;\n__attribute__((constructor)) static void init(void) { ran++; }\nint rann(void) { return ran; }\n' > "$W/n.c"
printf '#include <dlfcn.h>\nstatic volatile int ran;\n__attribute__((constructor)) static void init(void) { ran += dlopen("libn.so", RTLD_NOW) != 0; }\nint randep(void) { return ran; }\n' > "$W/dep.c"
printf 'int randep(void);\nstatic volatile int ran;\n__attribute__((constructor)) static void init(void) { ran++; }\nint rana(void) { return ran + randep() - 1; }\n' > "$W/a.c"
build -o "$W/libn.so" "$W/n.c"
build -o "$W/libdep.so" "$W/dep.c"
build -o "$W/liba.so" "$W/a.c" -L"$W" -ldep
files="$files --file /usr/lib/wasm32-linux-musl/libn.so=$W/libn.so --file /usr/lib/wasm32-linux-musl/libdep.so=$W/libdep.so --file /usr/lib/wasm32-linux-musl/liba.so=$W/liba.so"
cat > "$W/m.c" <<X
#include <dlfcn.h>
#include <pthread.h>
#include <stdio.h>
#define N $N
#define B $B
X
cat >> "$W/m.c" <<'X'
static int bad;
static int ran(const char *lib, const char *fn) {
    void *h = dlopen(lib, RTLD_NOW);
    if (!h) { printf("FAIL dlopen %s: %s\n", lib, dlerror()); return -1; }
    int (*f)(void) = (int (*)(void))dlsym(h, fn);
    return f ? f() : -1;
}
static void check(const char *lib, int got) {
    if (got != 1) { printf("FAIL %s: its constructor ran %d times, want once\n", lib, got); bad = 1; }
}
static void *busy(void *a) {
    for (int i = 1; i <= B; i++) {
        char lib[32], fn[16];
        snprintf(lib, sizeof lib, "libb%d.so", i); snprintf(fn, sizeof fn, "ranb%d", i);
        check(lib, ran(lib, fn));
    }
    return a;
}
int main(void) {
    pthread_t t;
    pthread_create(&t, 0, busy, 0);
    for (int i = 1; i <= N; i++) {
        char lib[32], fn[16];
        snprintf(lib, sizeof lib, "libs%d.so", i); snprintf(fn, sizeof fn, "ran%d", i);
        check(lib, ran(lib, fn));
    }
    pthread_join(t, 0);
    check("liba.so", ran("liba.so", "rana"));
    check("libn.so", ran("libn.so", "rann"));
    printf("%s\n", bad ? "FAIL" : "ok");
    return bad;
}
X
GEXCC_SHARED=1 gexcc -O1 -pthread -o "$W/m" "$W/m.c" || { echo "FAIL build"; exit 1; }
out=$(GEX_ARCH=$ARCH GEX_SHARED=1 timeout "${TMO:-300}" node "$ROOT/hosts/browser/js/run-node.mjs" $files \
    --file "/t/m=$W/m" --cwd /t --argv0 ./m "$SEED" "$W/m" 2>&1); s=$?
last=$(printf '%s\n' "$out" | grep -v '^\[' | grep -v '^$' | tail -1)
if [ "$last" = ok ] && [ $s = 0 ]; then echo "ok dlctors"; exit 0; fi
echo "FAIL dlctors (exit $s)"; printf '%s\n' "$out" | grep -v '^\[gex\] loaded' | tail -12
exit 1
