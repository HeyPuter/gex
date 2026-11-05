#!/bin/sh
# A function pointer from a dlopen handed to a thread with no system call
# on its side: the thread spins calling through the pointer (the program's
# own function before the dlopen, the library's after), entering the
# kernel only at gexcc's loop polls (one per so many back-edges), so its
# first calls through the new pointer can come before the catch-up at a
# system call's return has run there. Not
# (i32) -> i32, the type of the stand-in a slot holds until it is caught up
# with, which would catch up itself.
#
# A known gap by default: that call can trap, and gex names the cause. With
# GEX_DL_CHECKED_CALLS=1 every indirect call is guarded and the thread
# catches up at the first miss; that run must pass.
#
#   test/fixtures/gexcc/dl/dlspin.sh DIR   (gexcc on PATH; DIR: where gex can see,
#                                not /dev/shm)
set -u
command -v gexcc > /dev/null || { echo "gexcc not on PATH (toolchain/gexcc/install.sh)"; exit 2; }
W=$(mktemp -d "${1:?DIR}/dlspin.XXXXXX")
trap 'rm -rf "$W"' EXIT
cat > "$W/sp.c" <<'X'
double sp_f(double x) { return x + 1; }
X
cat > "$W/m.c" <<'X'
#include <dlfcn.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdio.h>
static double own_f(double x) { return x + 2; }
static double (*_Atomic fp)(double) = own_f;
static atomic_int phase;
static double sum;
static void *th(void *a) {
    (void)a;
    atomic_store(&phase, 1);
    int after = 0;
    for (;;) {
        double (*f)(double) = atomic_load(&fp);
        sum += f(1);
        if (f != own_f && ++after == 1000) break;
    }
    atomic_store(&phase, 2);
    return 0;
}
int main(int argc, char **argv) {
    pthread_t t;
    pthread_create(&t, 0, th, 0);
    while (atomic_load(&phase) != 1) {}
    void *h = dlopen(argc > 1 ? argv[1] : "libsp.so", RTLD_NOW);
    if (!h) { printf("FAIL dlopen: %s\n", dlerror()); return 1; }
    double (*f)(double) = (double (*)(double))dlsym(h, "sp_f");
    if (!f) { printf("FAIL dlsym\n"); return 1; }
    atomic_store(&fp, f);
    while (atomic_load(&phase) != 2) {}
    pthread_join(t, 0);
    printf("ok\n");
    return 0;
}
X
GEXCC_SHARED=1 gexcc -fPIC -shared -o "$W/libsp.so" "$W/sp.c" &&
    GEXCC_SHARED=1 gexcc -O2 -pthread -o "$W/m" "$W/m.c" || { echo "FAIL build"; exit 1; }
out=$(cd "$W" && GEX_DL_CHECKED_CALLS= gexcc-exec "$W/m" "$W/libsp.so" 2>&1)
if [ "$(printf '%s\n' "$out" | tail -1)" = ok ]; then
    echo "ok dlspin (unchecked: the handoff happened to be seen)"
elif printf '%s\n' "$out" | grep -q 'had not caught up with'; then
    echo "known gap dlspin (unchecked): the call traps, named"
else
    printf '%s\n' "$out" | tail -8; echo "FAIL dlspin (unchecked: not the named trap)"; exit 1
fi
out=$(cd "$W" && GEX_DL_CHECKED_CALLS=1 gexcc-exec "$W/m" "$W/libsp.so" 2>&1)
if [ "$(printf '%s\n' "$out" | tail -1)" = ok ]; then echo "ok dlspin (checked)"; else printf '%s\n' "$out" | tail -8; echo "FAIL dlspin (checked)"; exit 1; fi
