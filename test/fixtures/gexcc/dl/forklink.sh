#!/bin/sh
# A fork's child of a program with many shared libraries takes its parent's
# link (hosts/browser/src/cgen/dylink.rs `adopt_inherited`: the binding lists,
# without reading or parsing a library again) and is still a whole process:
# it calls its parent's libraries, dlsyms one the parent dlopened, dlopens
# one of its own that needs the parent's, forks a grandchild that does the
# same from the child's link, and execs. Forks while another thread dlopens
# give children whose link is one moment's (taken under the fork's hold).
# In the browser runtime (run-node), where GEX_LOG=forklink says the child
# took the link, and natively (gexcc-exec, the libraries beside the program:
# its run path is their directory; libc.so from gexcc's sysroot, the host
# having no wasm32-linux-musl directory of its own).
#
#   test/fixtures/gexcc/dl/forklink.sh DIR   (gexcc and node on PATH; GEX_ROOT: a gex
#                                 checkout with page/public/gex built; DIR: where gex
#                                 can see, not /dev/shm)
set -u
command -v gexcc > /dev/null || { echo "skip forklink: gexcc not on PATH (toolchain/gexcc/install.sh)"; exit 0; }
ROOT=${GEX_ROOT:?set GEX_ROOT to a gex checkout}
ARCH=${GEX_ARCH:-x86}
case $ARCH in x86) INFIX=.x86 ;; *) INFIX= ;; esac
SEED=$ROOT/page/public/gex/seed$INFIX.shared.wasm
[ -f "$SEED" ] || { echo "skip forklink: no $SEED"; exit 0; }
W=$(mktemp -d "${1:?DIR}/forklink.XXXXXX")
trap 'rm -rf "$W"' EXIT
N=40 R=8
LIBDIR=$(dirname "$(command -v gexcc)")/../sysroot/lib
L=/usr/lib/wasm32-linux-musl
files="--file $L/libc.so=$LIBDIR/libc.so"
build() { GEXCC_SHARED=1 gexcc -O1 -fPIC -shared "$@" || { echo "FAIL forklink: build $*"; exit 1; }; }
links=
for i in $(seq 1 $N); do
    # many functions, so the link is a real one to bind
    { for f in $(seq 1 400); do printf 'int k%d_%d(int x) { return x * %d + %d; }\n' $i $f $f $i; done
      printf 'int k%d(int x) { return k%d_1(x) * 0 + x * %d + %d; }\n' $i $i $i $i; } > "$W/k$i.c"
    build -o "$W/libk$i.so" "$W/k$i.c"
    files="$files --file $L/libk$i.so=$W/libk$i.so"; links="$links -lk$i"
done
printf 'int old_fn(int x) { return x + 1000; }\n' > "$W/old.c"
printf 'int k1(int);\nint new_fn(int x) { return k1(x) + 2000; }\n' > "$W/new.c"
build -o "$W/libold.so" "$W/old.c"
build -o "$W/libnew.so" "$W/new.c" -L"$W" -lk1 -Wl,-rpath,"$W"
files="$files --file $L/libold.so=$W/libold.so --file $L/libnew.so=$W/libnew.so"
for i in $(seq 1 $R); do
    { for f in $(seq 1 1500); do printf 'int r%d_%d(int x) { return x + %d; }\n' $i $f $f; done
      printf 'int race%d(void) { return r%d_1(0) * 0 + %d * 7; }\n' $i $i $i; } > "$W/r$i.c"
    build -o "$W/librace$i.so" "$W/r$i.c"
    files="$files --file $L/librace$i.so=$W/librace$i.so"
done
{
    printf '#define N %d\n#define R %d\n' $N $R
    for i in $(seq 1 $N); do printf 'int k%d(int);\n' $i; done
    printf 'static int (*const ks[])(int) = {0'; for i in $(seq 1 $N); do printf ', k%d' $i; done; printf '};\n'
} > "$W/m.c"
cat >> "$W/m.c" <<'X'
#include <dlfcn.h>
#include <pthread.h>
#include <stdio.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>
typedef int (*fn)(int);
static int libs_ok(void) {
    for (int i = 1; i <= N; i++) if (ks[i](3) != 3 * i + i) return 0;
    return 1;
}
static int sym_ok(const char *name, int arg, int want) {
    fn f = (fn)dlsym(RTLD_DEFAULT, name);
    return f && f(arg) == want;
}
/* the child's half: 0, or which check failed */
static int child(void) {
    if (!libs_ok()) return 2;
    if (!sym_ok("old_fn", 5, 1005)) return 3;
    if (!sym_ok("k17", 2, 2 * 17 + 17)) return 4;
    void *h = dlopen("libnew.so", RTLD_NOW);
    if (!h) { printf("child dlopen: %s\n", dlerror()); return 5; }
    fn nf = (fn)dlsym(h, "new_fn");
    if (!nf || nf(4) != 4 + 1 + 2000) return 6;
    return 0;
}
/* a grandchild of a child that has not looked a symbol up: its link is the
 * one the child took */
static int grandchild(void) {
    pid_t g = fork();
    if (g == 0) _exit(libs_ok() && sym_ok("k9", 1, 9 + 9) && sym_ok("old_fn", 0, 1000) ? 0 : 7);
    int st = 0;
    waitpid(g, &st, 0);
    return WIFEXITED(st) ? WEXITSTATUS(st) : 8;
}
static int wait_ok(pid_t p, const char *what) {
    int st = 0;
    if (waitpid(p, &st, 0) != p || !WIFEXITED(st) || WEXITSTATUS(st)) {
        printf("FAIL %s: status %#x\n", what, st);
        return 0;
    }
    return 1;
}
static volatile int racing = 1;
static void *loader(void *a) {
    for (int i = 1; i <= R; i++) {
        char lib[32];
        snprintf(lib, sizeof lib, "librace%d.so", i);
        if (!dlopen(lib, RTLD_NOW)) printf("FAIL dlopen %s: %s\n", lib, dlerror());
    }
    racing = 0;
    return a;
}
/* every race library the child's link has works in the child */
static int race_child(void) {
    for (int i = 1; i <= R; i++) {
        char name[16];
        snprintf(name, sizeof name, "race%d", i);
        int (*f)(void) = (int (*)(void))dlsym(RTLD_DEFAULT, name);
        if (f && f() != i * 7) return 10 + i;
    }
    return 0;
}
int main(int argc, char **argv) {
    if (argc > 1 && !strcmp(argv[1], "exec")) { printf("exec ok\n"); return 0; }
    int ok = 1;
    if (!libs_ok()) { printf("FAIL parent's libraries\n"); return 1; }
    void *old = dlopen("libold.so", RTLD_NOW);
    if (!old) { printf("FAIL dlopen libold.so: %s\n", dlerror()); return 1; }
    pid_t p = fork();
    if (p == 0) _exit(child());
    ok &= wait_ok(p, "child: libraries, dlsym of the parent's, dlopen of its own");
    p = fork();
    if (p == 0) _exit(grandchild());
    ok &= wait_ok(p, "grandchild");
    fflush(stdout);
    p = fork();
    if (p == 0) { execl(argv[0], argv[0], "exec", (char *)0); _exit(9); }
    ok &= wait_ok(p, "fork then exec");
    pthread_t t;
    pthread_create(&t, 0, loader, 0);
    int forks = 0;
    while (racing || forks < 4) {
        p = fork();
        if (p == 0) _exit(race_child());
        ok &= wait_ok(p, "fork while another thread dlopens");
        forks++;
    }
    pthread_join(t, 0);
    p = fork();
    if (p == 0) _exit(race_child() || !sym_ok("old_fn", 1, 1001));
    ok &= wait_ok(p, "fork after the dlopens");
    printf("%s\n", ok ? "ok" : "FAIL");
    return !ok;
}
X
GEXCC_SHARED=1 gexcc -O1 -pthread -o "$W/m" "$W/m.c" -L"$W" $links -Wl,-rpath,"$W" || { echo "FAIL forklink: build"; exit 1; }
fail=0
out=$(GEX_ARCH=$ARCH GEX_SHARED=1 GEX_LOG=forklink timeout "${TMO:-600}" node "$ROOT/hosts/browser/js/run-node.mjs" $files \
    --file "/t/m=$W/m" --cwd /t --argv0 /t/m "$SEED" "$W/m" 2>&1); s=$?
res=$(printf '%s\n' "$out" | grep -v '^\[' | grep -v '^$')
took=$(printf '%s\n' "$out" | grep -c "\[forklink\] .*its parent's link")
if [ "$(printf '%s\n' "$res" | tail -1)" = ok ] && [ $s = 0 ] && printf '%s\n' "$res" | grep -qx 'exec ok' && [ "$took" -ge 4 ]; then
    echo "ok forklink (browser runtime, $took children took the link)"
else
    echo "FAIL forklink (browser runtime, exit $s, $took children took the link)"; printf '%s\n' "$out" | grep -v '^\[gex\] loaded' | tail -14; fail=1
fi
out=$(cd "$W" && LD_LIBRARY_PATH="$(gexcc --print-sysroot)/lib:$W" GEXCC_SLOTS=0 timeout "${TMO:-600}" ./m 2>&1); s=$?
res=$(printf '%s\n' "$out" | grep -v '^\[' | grep -v '^$')
if [ "$(printf '%s\n' "$res" | tail -1)" = ok ] && [ $s = 0 ] && printf '%s\n' "$res" | grep -qx 'exec ok'; then
    echo "ok forklink (native)"
else
    echo "FAIL forklink (native, exit $s)"; printf '%s\n' "$out" | tail -14; fail=1
fi
exit $fail
