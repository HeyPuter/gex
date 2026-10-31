/* mem64-fcache: the guest of test/page/mem64-fcache.test.js. */
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
