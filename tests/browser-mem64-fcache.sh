#!/bin/sh
# The function table's probe cache in a 64-bit memory (gex_abi::FCACHE): two
# functions 4 GiB apart, at t and t + 4 GiB, share a slot (the slot hashes the
# low 32 bits) and their low 32 bits. Called in turn through pointers from
# the function JIT's code, each call must reach its own function: an entry
# that kept only the low 32 bits sent every call at t to the function at
# t + 4 GiB (and a 64-bit memory's images, near the top of its 16 GiB, never
# hit at all).
#
#   tests/browser-mem64-fcache.sh
#
# Needs zig, node, and in web/public/gex the 64-bit memory's x86 shim and
# seed (GEX_ARCH=x86 GEX_SHARED=1 GEX_MEM64=1 shims/browser/build.sh).
set -eu
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORK="${TMPDIR:-/tmp}/gex-mem64-fcache-$$"
export GEX_ARCH=x86
P="$ROOT/web/public/gex"
for need in "$P/gex_browser.x86.m64.shared.wasm" "$P/seed.x86.m64.shared.wasm"; do
    [ -e "$need" ] || { echo "skip: $need is not built" >&2; exit 0; }
done
command -v zig >/dev/null || { echo "skip: no zig to build the guest" >&2; exit 0; }
mkdir -p "$WORK"
[ -n "${KEEP:-}" ] || trap 'rm -rf "$WORK"' EXIT

cat > "$WORK/fc.c" <<'EOF2'
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
typedef int (*fn)(void);
static void *code_at(uint64_t a, int v) {
    unsigned char *p = mmap((void *)a, 4096, PROT_READ | PROT_WRITE | PROT_EXEC, MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED_NOREPLACE, -1, 0);
    if (p == MAP_FAILED || (uint64_t)p != a) return 0;
    p[0] = 0xb8; memcpy(p + 1, &v, 4); p[5] = 0xc3;
    mprotect(p, 4096, PROT_READ | PROT_EXEC);
    return p;
}
__attribute__((noinline)) static long calls(fn *f, int n) {
    long s = 0;
    for (int i = 0; i < n; i++) s += f[i & 1]();
    return s;
}
int main(void) {
    setvbuf(stdout, 0, _IONBF, 0);
    uint64_t t = 0x20000000ull;
    fn f[2] = { (fn)code_at(t, 1), (fn)code_at(t + (1ull << 32), 1000) };
    if (!f[0] || !f[1]) { printf("FAIL: no code at %#llx and %#llx\n", (unsigned long long)t, (unsigned long long)(t + (1ull << 32))); return 1; }
    long want = 0, got = 0;
    for (int r = 0; r < 200; r++) { got += calls(f, 1000); want += 500L * 1 + 500L * 1000; }
    printf("%s: %ld calls through functions at %#llx and %#llx (4 GiB apart): sum %ld, want %ld\n",
           got == want ? "ok" : "FAIL", 200L * 1000, (unsigned long long)t, (unsigned long long)(t + (1ull << 32)), got, want);
    return got != want;
}
EOF2
zig cc -target x86_64-linux-musl -O2 -static -o "$WORK/fc" "$WORK/fc.c"
GEX_SHARED=1 GEX_MEM64=1 GEX_FJIT=1 timeout "${TMO:-180}" node "$ROOT/shims/browser/js/run-node.mjs" --file /g/fc="$WORK/fc" --argv0 /g/fc "$P/seed.x86.m64.shared.wasm" "$WORK/fc" > "$WORK/out" 2> "$WORK/err" || true
got=$(grep -E '^(ok|FAIL)' "$WORK/out" || true)
case $got in
ok:*) echo "$got"; exit 0 ;;
esac
echo "FAIL: the probe cache in a 64-bit memory: ${got:-no result}" >&2
tail -8 "$WORK/err" >&2
exit 1
