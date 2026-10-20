#!/bin/sh
# Message passing between two guest threads, with plain x86 `mov`s, keeps x86
# order in the TSO flavour (GEX_TSO=1, the page's `?tso=1`).
#
# Thread A writes data[i] then flag[i]; thread B reads flag[i] then data[i].
# x86 never lets B see the flag without the data. Translated to plain wasm
# accesses, V8 may reorder them (rarely on an x86 host, freely on arm64); in
# the TSO flavour they are atomic and must not. The plain run's count is
# printed for information (nonzero on an arm64 host is expected).
#
#   tests/browser-tso.sh [rounds]
set -eu
ROUNDS="${1:-400}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORK="${TMPDIR:-/tmp}/gex-tso-$$"
GEXC="${GEXC:-$ROOT/target/x86/release/gexc}"
export GEX_ARCH=x86
for need in "$GEXC" "$ROOT/web/public/gex/gex_browser.x86.shared.wasm" "$ROOT/web/public/gex/gex_kernel.x86.wasm"; do
    [ -e "$need" ] || { echo "skip: $need is not built" >&2; exit 0; }
done
command -v zig >/dev/null || { echo "skip: no zig to build the guest" >&2; exit 0; }
mkdir -p "$WORK"
[ -n "${KEEP:-}" ] || trap 'rm -rf "$WORK"' EXIT
cat > "$WORK/mp.c" <<'EOF'
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#define N 4096
/* One cache line per slot; volatile so the guest compiler keeps the order
 * (x86 compiles each to a plain mov). */
static volatile int data[N * 16], flag[N * 16];
static int go, done_;
static int rounds;
static long bad, seen;
static void barrier(int *w, int target) {
    __atomic_add_fetch(w, 1, __ATOMIC_SEQ_CST);
    while (__atomic_load_n(w, __ATOMIC_SEQ_CST) < target) { }
}
static void *writer(void *_) {
    for (int r = 0; r < rounds; r++) {
        int tag = r + 1;
        barrier(&go, 2 * (r + 1));
        for (int i = 0; i < N; i++) { data[i * 16] = tag; flag[i * 16] = tag; }
        barrier(&done_, 2 * (r + 1));
    }
    return 0;
}
int main(int argc, char **argv) {
    rounds = atoi(argv[1]);
    pthread_t t;
    pthread_create(&t, 0, writer, 0);
    for (int r = 0; r < rounds; r++) {
        int tag = r + 1;
        barrier(&go, 2 * (r + 1));
        for (int i = 0; i < N; i++) {
            int f = flag[i * 16];
            int d = data[i * 16];
            if (f == tag) { seen++; if (d != tag) bad++; }
        }
        barrier(&done_, 2 * (r + 1));
    }
    pthread_join(t, 0);
    printf("mp %ld of %ld\n", bad, seen);
    return 0;
}
EOF
zig cc -target x86_64-linux-musl -O2 -static -Wl,--image-base=0x1000000 -o "$WORK/mp" "$WORK/mp.c"
"$GEXC" seed -o "$WORK/seed.wasm" >/dev/null 2>&1
run() {
    env GEX_SHARED=1 "$@" timeout "${TMO:-600}" node "$ROOT/shims/browser/js/run-node.mjs" "$WORK/seed.wasm" "$WORK/mp" "$ROUNDS" 2>/dev/null | grep '^mp ' || echo "mp ? (the run failed)"
}
plain=$(run GEX_TSO=0)
tso=$(run GEX_TSO=1)
echo "info plain: $plain"
case "$tso" in
"mp 0 of "*) case "$tso" in *" of 0") echo "FAIL tso: the reader never saw a flag ($tso)"; exit 1 ;; esac
    echo "ok   tso: $tso reordered (plain: ${plain#mp })" ;;
*) echo "FAIL tso: $tso"; exit 1 ;;
esac
