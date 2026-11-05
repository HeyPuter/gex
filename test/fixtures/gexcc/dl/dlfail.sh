#!/bin/sh
# A dlopen that fails part way (a library that needs one that is not there)
# leaves the link map as it was (hosts/browser/src/cgen/dylink.rs
# `dlopen_in`, which adds to the map in place and drops what it added on a
# failure): the next dlopen's library takes its place, and a symbol both
# define is the second one's, one only the failed one defines is not found.
# Twice over, and after a fork. In the browser runtime (run-node) and
# natively (gexcc-exec).
#
#   test/fixtures/gexcc/dl/dlfail.sh DIR   (gexcc and node on PATH; GEX_ROOT: a gex
#                                checkout with page/public/gex built)
set -u
command -v gexcc > /dev/null || { echo "skip dlfail: gexcc not on PATH (toolchain/gexcc/install.sh)"; exit 0; }
ROOT=${GEX_ROOT:?set GEX_ROOT to a gex checkout}
ARCH=${GEX_ARCH:-x86}
case $ARCH in x86) INFIX=.x86 ;; *) INFIX= ;; esac
SEED=$ROOT/page/public/gex/seed$INFIX.shared.wasm
[ -f "$SEED" ] || { echo "skip dlfail: no $SEED"; exit 0; }
W=$(mktemp -d "${1:?DIR}/dlfail.XXXXXX")
trap 'rm -rf "$W"' EXIT
LIBDIR=$(dirname "$(command -v gexcc)")/../sysroot/lib
L=/usr/lib/wasm32-linux-musl
build() { GEXCC_SHARED=1 gexcc -O1 -fPIC -shared "$@" || { echo "FAIL dlfail: build $*"; exit 1; }; }
printf 'int gone_fn(int x) { return x; }\n' > "$W/gone.c"
printf 'int gone_fn(int);\nint both(void) { return 1; }\nint a_only(void) { return gone_fn(7); }\n' > "$W/a.c"
printf 'int both(void) { return 2; }\nint b_only(void) { return 22; }\n' > "$W/b.c"
build -o "$W/libgone.so" "$W/gone.c"
build -o "$W/liba.so" "$W/a.c" -L"$W" -lgone
build -o "$W/libb.so" "$W/b.c"
rm -f "$W/libgone.so"
cat > "$W/m.c" <<'X'
#include <dlfcn.h>
#include <stdio.h>
#include <sys/wait.h>
#include <unistd.h>
static int check(const char *when) {
    int (*both)(void) = (int (*)(void))dlsym(RTLD_DEFAULT, "both");
    int (*bo)(void) = (int (*)(void))dlsym(RTLD_DEFAULT, "b_only");
    if (dlsym(RTLD_DEFAULT, "a_only")) { printf("FAIL %s: a_only found\n", when); return 0; }
    if (!both || both() != 2) { printf("FAIL %s: both is not libb's\n", when); return 0; }
    if (!bo || bo() != 22) { printf("FAIL %s: b_only\n", when); return 0; }
    return 1;
}
int main(void) {
    for (int i = 0; i < 2; i++) if (dlopen("liba.so", RTLD_NOW)) { printf("FAIL liba.so opened\n"); return 1; }
    if (!dlopen("libb.so", RTLD_NOW)) { printf("FAIL dlopen libb.so: %s\n", dlerror()); return 1; }
    int ok = check("after");
    if (dlopen("liba.so", RTLD_NOW)) { printf("FAIL liba.so opened later\n"); return 1; }
    ok &= check("after another failure");
    fflush(stdout);
    pid_t p = fork();
    if (p == 0) _exit(check("fork's child") ? 0 : 1);
    int st = 0;
    waitpid(p, &st, 0);
    ok &= WIFEXITED(st) && !WEXITSTATUS(st);
    printf("%s\n", ok ? "ok" : "FAIL");
    return !ok;
}
X
GEXCC_SHARED=1 gexcc -O1 -o "$W/m" "$W/m.c" -Wl,-rpath,"$W" || { echo "FAIL dlfail: build"; exit 1; }
fail=0
out=$(GEX_ARCH=$ARCH GEX_SHARED=1 timeout "${TMO:-300}" node "$ROOT/hosts/browser/js/run-node.mjs" \
    --file "$L/libc.so=$LIBDIR/libc.so" --file "$L/liba.so=$W/liba.so" --file "$L/libb.so=$W/libb.so" \
    --file "/t/m=$W/m" --cwd /t --argv0 /t/m "$SEED" "$W/m" 2>&1); s=$?
res=$(printf '%s\n' "$out" | grep -v '^\[' | grep -v '^$')
if [ "$(printf '%s\n' "$res" | tail -1)" = ok ] && [ $s = 0 ]; then echo "ok dlfail (browser runtime)"
else echo "FAIL dlfail (browser runtime, exit $s)"; printf '%s\n' "$out" | tail -10; fail=1; fi
out=$(cd "$W" && LD_LIBRARY_PATH="$(gexcc --print-sysroot)/lib:$W" GEXCC_SLOTS=0 timeout "${TMO:-300}" ./m 2>&1); s=$?
res=$(printf '%s\n' "$out" | grep -v '^\[' | grep -v '^$')
if [ "$(printf '%s\n' "$res" | tail -1)" = ok ] && [ $s = 0 ]; then echo "ok dlfail (native)"
else echo "FAIL dlfail (native, exit $s)"; printf '%s\n' "$out" | tail -10; fail=1; fi
exit $fail
