#!/bin/sh
# Indirect calls with more trailing arguments than the function takes, as
# GLib makes them (GObject's class and instance initialisers, a signal
# handler without its user data, a GFunc that is g_free), a function that
# returns a value called as returning none, and one that returns none
# called through an unprototyped int () (bash's unwind-protect cleanups):
# wasm traps on each, so a
# library built with GEXCC_LENIENT_CALLS=1 cuts them as C on a real machine
# does. The library calls the program's functions; the same library built
# without it must still trap. Native gexc (GEXCC_GEXC, or the install's)
# and the browser runtime (run-node), each against the host's output.
#
#   test/fixtures/gexcc/dl/lenientcalls.sh DIR   (gexcc on PATH; GEX_ROOT: a gex
#                                      checkout with page/public/gex built;
#                                      DIR: where gex can see, not /dev/shm)
set -u
command -v gexcc > /dev/null || { echo "gexcc not on PATH (toolchain/gexcc/install.sh)"; exit 2; }
ROOT=${GEX_ROOT:?set GEX_ROOT to a gex checkout}
ARCH=${GEX_ARCH:-x86}
case $ARCH in x86) INFIX=.x86 ;; *) INFIX= ;; esac
SEED=$ROOT/page/public/gex/seed$INFIX.shared.wasm
[ -f "$SEED" ] || { echo "no $SEED"; exit 2; }
W=$(mktemp -d "${1:?DIR}/lenientcalls.XXXXXX")
trap 'rm -rf "$W"' EXIT
cat > "$W/l.c" <<'X'
typedef void (*init_fn)(void *, void *);
typedef void (*marshal_fn)(void *, int, void *);
typedef void (*void_fn)(void *);
typedef int (*int_fn)();
void call_init(void *f, void *a) { ((init_fn)f)(a, (void *)0x1234); }
void call_marshal(void *f, void *a) { ((marshal_fn)f)(a, 5, (void *)0); }
void call_void(void *f, void *a) { ((void_fn)f)(a); }
/* its result is unused: a void function's is no value */
void call_int(void *f, void *a) { (void)((int_fn)f)(a); }
X
cat > "$W/p.c" <<'X'
#include <stdio.h>
void call_init(void *f, void *a);
void call_marshal(void *f, void *a);
void call_void(void *f, void *a);
void call_int(void *f, void *a);
static int got;
static void one(void *a) { got += *(int *)a; }
static void two(void *a, int n) { (void)a; got += n; }
static int counted(void *a) { (void)a; got += 100; return 9; }
int main(void)
{
    int v = 3;
    call_init((void *)one, &v);
    printf("one of two %d\n", got);
    call_marshal((void *)two, &v);
    printf("two of three %d\n", got);
    call_marshal((void *)one, &v);
    printf("one of three %d\n", got);
    call_void((void *)counted, &v);
    printf("result dropped %d\n", got);
    call_init((void *)counted, &v);
    printf("both %d\n", got);
    call_int((void *)one, &v);
    printf("none for an int %d\n", got);
    return 0;
}
X
LIBDIR=$(dirname "$(command -v gexcc)")/../sysroot/lib
cc -O1 -w -shared -fPIC -o "$W/libl.native.so" "$W/l.c" &&
    cc -O1 -w -o "$W/p.native" "$W/p.c" -L"$W" -l:libl.native.so -Wl,-rpath,"$W" || { echo "FAIL build native"; exit 1; }
want=$("$W/p.native")
mkdir -p "$W/strict"
GEXCC_SHARED=1 GEXCC_LENIENT_CALLS=1 gexcc -w -fPIC -shared -o "$W/libl.so" "$W/l.c" &&
    GEXCC_SHARED=1 gexcc -w -fPIC -shared -o "$W/strict/libl.so" "$W/l.c" &&
    GEXCC_SHARED=1 gexcc -w -o "$W/p" "$W/p.c" -L"$W" -ll || { echo "FAIL build"; exit 1; }
fail=0
check() { # name got status
    if [ "$2" = "$want" ] && [ "$3" = 0 ]; then echo "ok   lenientcalls $1"; else
        echo "FAIL lenientcalls $1 (exit $3)"; printf '%s\n' "$2" | tail -8; fail=1; fi
}
got=$(cd "$W" && GEX_LOG=off LD_LIBRARY_PATH="$W:$LIBDIR" ./p 2>&1); s=$?
check native "$got" $s
got=$(cd "$W" && GEX_LOG=off LD_LIBRARY_PATH="$W/strict:$LIBDIR" ./p 2>&1); s=$?
if [ $s != 0 ]; then echo "ok   lenientcalls native, without: traps"; else echo "FAIL lenientcalls native, without: did not trap"; fail=1; fi
run_node() { # library
    GEX_ARCH=$ARCH GEX_SHARED=1 timeout "${TMO:-300}" \
        node "$ROOT/hosts/browser/js/run-node.mjs" \
        --file "/usr/lib/wasm32-linux-musl/libc.so=$LIBDIR/libc.so" \
        --file "/usr/lib/wasm32-linux-musl/libl.so=$1" \
        --file "/t/p=$W/p" --cwd /t --argv0 ./p "$SEED" "$W/p" 2>&1
}
got=$(run_node "$W/libl.so"); s=$?
check browser "$(printf '%s\n' "$got" | grep -v '^\[')" $s
got=$(run_node "$W/strict/libl.so"); s=$?
# The trap is the guest's SIGSEGV (a call through a mismatched pointer, as a
# wild call is on a real machine): the process dies of it, status 139; a
# runtime from before that reported the engine's "signature mismatch".
case $s:$got in 139:*|*"signature mismatch"*) echo "ok   lenientcalls browser, without: traps" ;;
    *) echo "FAIL lenientcalls browser, without: did not trap (exit $s)"; fail=1 ;; esac
exit $fail
