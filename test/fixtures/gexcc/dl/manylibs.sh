#!/bin/sh
# More shared libraries than libc's registry once had room for (256): a
# program that starts with N of them (default 300), each with a constructor
# and thread-local data, and one more they all need (the constructors'
# tally); it reads them from the main thread and a second one, then dlopens
# 20 more. Every constructor runs once and before main, every thread sees
# each library's initial value and then its own. Natively (gexcc-exec) and
# in the browser runtime (run-node).
#
#   test/fixtures/gexcc/dl/manylibs.sh DIR [N]   (gexcc on PATH; GEX_ROOT: a
#                                 gex checkout with page/public/gex built;
#                                 DIR: where gex can see, not /dev/shm)
set -u
command -v gexcc > /dev/null || { echo "gexcc not on PATH (toolchain/gexcc/install.sh)"; exit 2; }
ROOT=${GEX_ROOT:?set GEX_ROOT to a gex checkout}
ARCH=${GEX_ARCH:-x86}
case $ARCH in x86) INFIX=.x86 ;; *) INFIX= ;; esac
SEED=$ROOT/page/public/gex/seed$INFIX.shared.wasm
W=$(mktemp -d "${1:?DIR}/manylibs.XXXXXX")
trap 'rm -rf "$W"' EXIT
N=${2:-300}
D=20
mkdir -p "$W/lib" "$W/dl"
echo "int ctor_ran[$((N + D))];" > "$W/base.c"
GEXCC_SHARED=1 gexcc -fPIC -shared -o "$W/lib/libbase.so" "$W/base.c" || { echo "FAIL build base"; exit 1; }
k=0
while [ $k -lt $((N + D)) ]; do
    # (the dlopened ones without thread-local data: dltls.sh tests that)
    tl=_Thread_local; [ $k -lt $N ] || tl=
    cat > "$W/l$k.c" <<X
extern int ctor_ran[];
$tl int v$k = $k;
__attribute__((constructor)) static void c(void) { ctor_ran[$k]++; }
int get$k(void) { return v$k; }
void set$k(int x) { v$k = x; }
X
    if [ $k -lt $N ]; then out=$W/lib/libl$k.so; else out=$W/dl/libl$k.so; fi
    GEXCC_SHARED=1 gexcc -fPIC -shared -o "$out" "$W/l$k.c" -L"$W/lib" -lbase -Wl,-rpath,"$W/lib" || { echo "FAIL build l$k"; exit 1; }
    k=$((k + 1))
done
{
    echo '#include <dlfcn.h>'
    echo '#include <pthread.h>'
    echo '#include <stdio.h>'
    echo "#define N $N"
    echo "#define D $D"
    echo "extern int ctor_ran[N + D];"
    k=0; while [ $k -lt $N ]; do echo "int get$k(void); void set$k(int);"; k=$((k + 1)); done
    echo 'static int (*get[N])(void) = {'
    k=0; while [ $k -lt $N ]; do echo "get$k,"; k=$((k + 1)); done
    echo '};'
    echo 'static void (*set[N])(int) = {'
    k=0; while [ $k -lt $N ]; do echo "set$k,"; k=$((k + 1)); done
    echo '};'
    cat <<'X'
static int bad;
static void check(const char *what, int k, int got, int want) {
    if (got != want) { printf("bad %s %d: %d, want %d\n", what, k, got, want); bad = 1; }
}
static void *th(void *a) {
    (void)a;
    for (int k = 0; k < N; k++) check("thread's first value", k, get[k](), k);
    for (int k = 0; k < N; k++) set[k](2000 + k);
    for (int k = 0; k < N; k++) check("thread's value", k, get[k](), 2000 + k);
    return 0;
}
int main(int argc, char **argv) {
    for (int k = 0; k < N; k++) check("constructor before main", k, ctor_ran[k], 1);
    for (int k = 0; k < N; k++) check("main's first value", k, get[k](), k);
    for (int k = 0; k < N; k++) set[k](1000 + k);
    pthread_t t;
    pthread_create(&t, 0, th, 0);
    pthread_join(t, 0);
    for (int k = 0; k < N; k++) check("main's value", k, get[k](), 1000 + k);
    for (int k = N; k < N + D; k++) {
        char p[512], s[32];
        snprintf(p, sizeof p, "%s/libl%d.so", argv[1], k);
        void *h = dlopen(p, RTLD_NOW);
        if (!h) { printf("bad dlopen %s: %s\n", p, dlerror()); bad = 1; continue; }
        check("dlopened constructor", k, ctor_ran[k], 1);
        snprintf(s, sizeof s, "get%d", k);
        int (*g)(void) = (int (*)(void))dlsym(h, s);
        if (!g) { printf("bad dlsym %s\n", s); bad = 1; continue; }
        check("dlopened value", k, g(), k);
    }
    puts(bad ? "bad" : "all good");
    return bad;
}
X
} > "$W/m.c"
libs=
k=0; while [ $k -lt $N ]; do libs="$libs -ll$k"; k=$((k + 1)); done
GEXCC_SHARED=1 gexcc -pthread -o "$W/m" "$W/m.c" -L"$W/lib" $libs -lbase -Wl,-rpath,"$W/lib" || { echo "FAIL build m"; exit 1; }
s=0
out=$(cd "$W" && timeout "${TMO:-600}" gexcc-exec "$W/m" "$W/dl" 2>&1)
if [ "$(printf '%s\n' "$out" | tail -1)" = "all good" ]; then echo "ok   manylibs native ($N + $D libraries)"
else echo "FAIL manylibs native"; printf '%s\n' "$out" | tail -8; s=1; fi
if [ ! -f "$SEED" ]; then echo "skip: no $SEED (the browser leg)"; exit $s; fi
LIBDIR=$(dirname "$(command -v gexcc)")/../sysroot/lib
out=$(GEX_ARCH=$ARCH GEX_SHARED=1 GEX_LOG=off timeout "${TMO:-600}" node "$ROOT/hosts/browser/js/run-node.mjs" --dir "$W=$W" \
    --file "/usr/lib/wasm32-linux-musl/libc.so=$LIBDIR/libc.so" "$SEED" "$W/m" "$W/dl" 2>&1 | grep -v '^\[')
if [ "$(printf '%s\n' "$out" | tail -1)" = "all good" ]; then echo "ok   manylibs browser ($N + $D libraries)"
else echo "FAIL manylibs browser"; printf '%s\n' "$out" | tail -8; s=1; fi
exit $s
