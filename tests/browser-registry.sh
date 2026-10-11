#!/bin/sh
# The translated-module registry of a threaded process keeps its bytes in the
# process's JS-side module store (`Gex#moduleStore`), not in the shim's heap.
#
# The registry is what a thread Worker that joins later replays to catch up.
# It used to hold every module's bytes in the shim's heap, which shares a
# fixed 2 GiB arena with the guest's low mappings; in a long fjit run (Muck on
# Unity's own Mono, in the page) it reached 980 MiB and the heap could not
# grow. Here a threaded guest generates and runs a few thousand distinct
# functions, each a fresh translation the registry keeps, and then starts one
# more thread, which has to catch up on all of them. The store must have grown
# by megabytes while the registry's bytes in the heap stay at what was lifted
# before the first thread (about one MiB, kept there as before).
#
#   tests/browser-registry.sh            (x86, the shared memory64 shim, else the shared one)
set -eu
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORK="${TMPDIR:-/tmp}/gex-registry-$$"
export GEX_ARCH=x86 GEX_SHARED=1
SEED="$ROOT/web/public/gex/seed.x86.m64.shared.wasm"
if [ -e "$SEED" ] && [ -e "$ROOT/web/public/gex/gex_browser.x86.m64.shared.wasm" ]; then
    export GEX_MEM64=1
else
    SEED="$ROOT/web/public/gex/seed.x86.shared.wasm"
    [ -e "$ROOT/web/public/gex/gex_browser.x86.shared.wasm" ] || { echo "skip: no shared x86 shim built" >&2; exit 0; }
fi
[ -e "$SEED" ] || { echo "skip: $SEED is not built" >&2; exit 0; }
command -v zig >/dev/null || { echo "skip: no zig to build the guest" >&2; exit 0; }
command -v node >/dev/null || { echo "skip: no node" >&2; exit 0; }
mkdir -p "$WORK"
trap 'rm -rf "$WORK"' EXIT

cat > "$WORK/jitreg.c" <<'C'
#include <pthread.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <time.h>
#include <unistd.h>
#define N 3000
/* Salted per run, so no cache of an earlier run's translations serves them. */
static int salt;
static long expect(int i, long x) {
    for (int k = 0; k < 40; k++) {
        int v = (i * 131 + k) ^ salt;
        x += v;
        x ^= (long)(v ^ 0x5a5a);
    }
    return x;
}
static void *idle(void *a) { return a; }
static void *check(void *a) {
    /* A thread that joins after the functions exist: catches up on them all. */
    long bad = 0;
    unsigned char *code = a;
    for (int i = 0; i < N; i += 97) bad += ((long (*)(long))(code + (long)i * 4096))(i) != expect(i, i);
    return (void *)bad;
}
int main(void) {
    pthread_t t;
    struct timespec ts;
    clock_gettime(CLOCK_REALTIME, &ts);
    salt = (int)(ts.tv_nsec ^ getpid() << 8) & 0x3fffffff;
    /* Threaded first: only then does the registry keep anything. */
    pthread_create(&t, 0, idle, 0);
    pthread_join(t, 0);
    unsigned char *code = mmap(0, (size_t)N * 4096, PROT_READ | PROT_WRITE | PROT_EXEC, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (code == MAP_FAILED) { perror("mmap"); return 1; }
    long bad = 0;
    for (int i = 0; i < N; i++) {
        unsigned char *f = code + (long)i * 4096, *p = f;
        /* mov rax, rdi; then 40 x (add rax, imm32; xor rax, imm32); ret */
        *p++ = 0x48; *p++ = 0x89; *p++ = 0xf8;
        for (int k = 0; k < 40; k++) {
            int v = (i * 131 + k) ^ salt;
            *p++ = 0x48; *p++ = 0x05; memcpy(p, &v, 4); p += 4;
            *p++ = 0x48; *p++ = 0x35; v ^= 0x5a5a; memcpy(p, &v, 4); p += 4;
        }
        *p++ = 0xc3;
        /* Often enough to be translated, not interpreted. */
        for (int k = 0; k < 64; k++) bad += ((long (*)(long))f)(i + k) != expect(i, i + k);
    }
    void *r;
    pthread_create(&t, 0, check, code);
    pthread_join(t, &r);
    printf("%d functions, %ld wrong, %ld wrong in the joining thread\n", N, bad, (long)r);
    return 0;
}
C
zig cc -target x86_64-linux-musl -O1 -static -o "$WORK/jitreg" "$WORK/jitreg.c"
native=$("$WORK/jitreg")
# The host's own `[host] ...` progress lines share stdout.
out=$(GEX_REGISTRY_TRACE=1 timeout 300 node "$ROOT/shims/browser/js/run-node.mjs" "$SEED" "$WORK/jitreg" 2>"$WORK/err" | grep -v '^\[') || true
# The largest of each: Workers' console lines arrive in no particular order.
lines=$(grep -o 'module store at [0-9]* MiB; registry bytes in the shim heap: [0-9]* MiB' "$WORK/err" || true)
store=$(echo "$lines" | sed -n 's/module store at \([0-9]*\) MiB.*/\1/p' | sort -n | tail -1)
heap=$(echo "$lines" | sed -n 's/.*shim heap: \([0-9]*\) MiB/\1/p' | sort -n | tail -1)
if [ "$out" != "$native" ]; then
    echo "FAIL browser-registry: output '$out', native '$native'"; tail -5 "$WORK/err"; exit 1
fi
if [ -z "$store" ] || [ "$store" -lt 2 ] || [ "$heap" -gt 2 ]; then
    echo "FAIL browser-registry: module store ${store:-none} MiB, registry in the shim heap ${heap:-?} MiB"; tail -5 "$WORK/err"; exit 1
fi
echo "ok   browser-registry ($out; module store $store MiB, registry in the shim heap $heap MiB)"
