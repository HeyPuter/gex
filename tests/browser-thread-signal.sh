#!/bin/sh
# A signal reaches a thread that is spinning in its own code, under the
# browser runtime.
#
# Such a thread makes no syscalls, so only a back-edge safepoint can deliver
# to it. A `pthread_kill` from another thread of the same process raises the
# signal in that process's kernel, for that thread, and the safepoint asked
# about whichever thread last entered the kernel instead: the handler never
# ran and the join on the spinner never returned. Five rounds of that, then
# an `alarm` that must end a spin on the main thread (the timer path the same
# safepoint check serves). docs/kernel-locking.md, step 3: the safepoint takes
# the kernel's lock only on a hint, and this is what the hints must not miss.
#
#   tests/browser-thread-signal.sh [a64|x86]
#
# Needs zig, node, the guest's gexc, and the shared shim and kernel in
# web/public/gex (shims/browser/build.sh).
set -eu
ARCH="${1:-a64}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORK="${TMPDIR:-/tmp}/gex-thread-signal-$ARCH-$$"
case "$ARCH" in
a64) TARGET=aarch64-linux-musl; GEXC="${GEXC:-$ROOT/target/release/gexc}"; INFIX="" ;;
x86) TARGET=x86_64-linux-musl; GEXC="${GEXC:-$ROOT/target/x86/release/gexc}"; INFIX=".x86"; export GEX_ARCH=x86 ;;
*) echo "usage: $0 [a64|x86]" >&2; exit 2 ;;
esac
for need in "$GEXC" "$ROOT/web/public/gex/gex_browser$INFIX.shared.wasm" "$ROOT/web/public/gex/gex_kernel$INFIX.wasm"; do
    [ -e "$need" ] || { echo "skip: $need is not built" >&2; exit 0; }
done
command -v zig >/dev/null || { echo "skip: no zig to build the guest" >&2; exit 0; }
mkdir -p "$WORK"
[ -n "${KEEP:-}" ] || trap 'rm -rf "$WORK"' EXIT

cat > "$WORK/ts.c" <<'EOF'
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <time.h>
#include <unistd.h>
static volatile sig_atomic_t hit, rang, ready;
static volatile unsigned long sink;
static void on_usr1(int s) { (void)s; hit = 1; }
static void on_alrm(int s) { (void)s; rang = 1; }
static double now(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec + t.tv_nsec / 1e9;
}
/* No syscall in the loop: only a safepoint can stop it. */
static void *spin(void *a) {
    (void)a;
    ready = 1;
    unsigned long h = 1;
    while (!hit) { h = h * 6364136223846793005UL + 1; sink = h; }
    return 0;
}
int main(void) {
    setvbuf(stdout, 0, _IONBF, 0);
    signal(SIGUSR1, on_usr1);
    signal(SIGALRM, on_alrm);
    int late = 0;
    for (int round = 0; round < 5; round++) {
        hit = 0;
        ready = 0;
        pthread_t t;
        pthread_create(&t, 0, spin, 0);
        while (!ready) usleep(1000);
        usleep(20000);
        double t0 = now();
        pthread_kill(t, SIGUSR1);
        pthread_join(t, 0);
        if (now() - t0 >= 1.0) late++;
    }
    printf("spinning threads took SIGUSR1 promptly: %s\n", late ? "no" : "yes");
    alarm(1);
    double t0 = now();
    unsigned long h = 1;
    while (!rang && now() - t0 < 10) {
        for (int i = 0; i < 1000; i++) { h = h * 6364136223846793005UL + 1; sink = h; }
    }
    printf("alarm ended a spin: %s\n", rang ? "yes" : "no");
    return 0;
}
EOF
zig cc -target "$TARGET" -O2 -static -Wl,--image-base=0x1000000 -o "$WORK/ts" "$WORK/ts.c" -lpthread
"$GEXC" seed -o "$WORK/seed.wasm" >/dev/null 2>&1
want="spinning threads took SIGUSR1 promptly: yes
alarm ended a spin: yes"
GEX_SHARED=1 timeout "${TMO:-120}" node "$ROOT/shims/browser/js/run-node.mjs" "$WORK/seed.wasm" "$WORK/ts" > "$WORK/out" 2> "$WORK/err" || true
got=$(grep -v '^\[' "$WORK/out" || true)
if [ "$got" = "$want" ]; then
    echo "ok: a pthread_kill reaches a thread spinning in its own code, and an alarm a spinning main thread ($ARCH)"
    exit 0
fi
echo "FAIL: signals at safepoints ($ARCH): got" >&2
echo "$got" >&2
tail -5 "$WORK/err" >&2
exit 1
