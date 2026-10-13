/* Differential test for the CRT math providers (docs/merge-plan.md §9.9.3):
 * calls msvcr120.dll's math exports (whichever msvcr120.dll loads: one
 * beside the exe first) on special values, random bit patterns and random
 * values in common ranges, under each rounding mode for sqrt; the
 * trigonometry (sin, cos, tan, atan, atan2, rint and their _CI* and
 * __libm_sse2_* entries) under the default control word, rounding up,
 * 64-bit precision, and with an x87 register below the argument that is
 * no double (the last three are where Wine's provider falls back to the
 * lifted body: tests/providers/run.sh checks it did); and prints,
 * per function, a hash of every result's bits, the integer part modf
 * stores, errno after the call, and the x87 stack depth after it. Run once
 * with GEX_PROVIDERS=0 (the guest's own code) and once with providers; the
 * outputs must be identical (tests/providers/run.sh).
 *
 *   crtmath [-v NAME]   -v prints each case of NAME instead of hashing. */
#include <float.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <windows.h>

typedef int *(__cdecl *errno_t_)(void);

static errno_t_ get_errno;
static const char *verbose;
static uint64_t seed = 0x9e3779b97f4a7c15ull;
static uint64_t rnd(void) {
    seed ^= seed << 13;
    seed ^= seed >> 7;
    seed ^= seed << 17;
    return seed;
}
static uint64_t bits(double d) {
    uint64_t b;
    memcpy(&b, &d, 8);
    return b;
}
static double from(uint64_t b) {
    double d;
    memcpy(&d, &b, 8);
    return d;
}

#define NIN 72000
static double in[NIN];
static int nin;
static void inputs(void) {
    static const uint64_t specials[] = {
        0, 0x8000000000000000ull, 0x7ff0000000000000ull, 0xfff0000000000000ull,
        0x7ff8000000000000ull, 0xfff8000000000000ull, 0x7ff0000000000001ull, 0x7ff4000000000123ull,
        0xfff0000000000005ull, 0x7ff8000000abcdefull, 1, 0x8000000000000001ull, 0x000fffffffffffffull,
        0x0010000000000000ull, 0x7fefffffffffffffull, 0xffefffffffffffffull, 0x3ff0000000000000ull,
        0xbff0000000000000ull, 0x3fe0000000000000ull, 0xbfe0000000000000ull, 0x4330000000000000ull,
        0x4330000000000001ull, 0xc330000000000001ull, 0x432fffffffffffffull, 0x3fefffffffffffffull,
        0xbfefffffffffffffull, 0x3ff0000000000001ull, 0x4000000000000000ull,
    };
    nin = 0;
    for (unsigned i = 0; i < sizeof specials / sizeof *specials; i++) in[nin++] = from(specials[i]);
    for (int i = -40; i <= 40; i++) {
        in[nin++] = i * 0.5;
        in[nin++] = i * 0.5 + 1e-12;
        in[nin++] = i * 0.5 - 1e-12;
    }
    while (nin < 20000) in[nin++] = from(rnd());
    while (nin < 40000) in[nin++] = ((double)(int64_t)rnd() / 9.2233720368547758e18) * 1e6;
    while (nin < 60000) in[nin++] = ((double)(int64_t)rnd() / 9.2233720368547758e18) * 64.0;
    /* For __rem_pio2: near multiples of pi/2 (its cancellation cases), up
     * to and past 2^20 pi/2 (the large reduction), and huge. */
    for (int k = 1; nin < 64000; k += 1 + k / 64) {
        double m = k * 1.5707963267948966;
        in[nin++] = m;
        in[nin++] = -m;
        in[nin++] = from(bits(m) + 1);
        in[nin++] = from(bits(m) - 1);
    }
    while (nin < 68000) in[nin++] = from((rnd() & 0x800fffffffffffffull) | ((uint64_t)(0x3ff + 15 + rnd() % 12) << 52));
    while (nin < NIN) in[nin++] = from((rnd() & 0x800fffffffffffffull) | ((uint64_t)(0x3ff + 26 + rnd() % 990) << 52));
}

static uint64_t hash;
static void mix(uint64_t v) {
    hash ^= v;
    hash *= 0x100000001b3ull;
}
/* x87 stack depth: the tag word's empty slots (fnstenv). */
static int x87_depth(void) {
    unsigned short env[14];
    __asm__ volatile("fnstenv %0; fldenv %0" : "=m"(env));
    int n = 0;
    for (int i = 0; i < 8; i++) n += ((env[4] >> (2 * i)) & 3) != 3;
    return n;
}
static void record(const char *name, uint64_t x, uint64_t r, uint64_t extra) {
    int e = *get_errno();
    int depth = x87_depth();
    if (verbose && !strcmp(verbose, name)) {
        printf("%s(%016llx) = %016llx extra %016llx errno %d depth %d\n", name, (unsigned long long)x,
               (unsigned long long)r, (unsigned long long)extra, e, depth);
    }
    mix(r);
    mix(extra);
    mix((uint64_t)e);
    mix((uint64_t)depth);
}

static void set_round(unsigned rc) {
    unsigned short cw;
    __asm__ volatile("fnstcw %0" : "=m"(cw));
    cw = (cw & ~0xc00) | (rc << 10);
    __asm__ volatile("fldcw %0" ::"m"(cw));
    unsigned mx;
    __asm__ volatile("stmxcsr %0" : "=m"(mx));
    mx = (mx & ~0x6000u) | (rc << 13);
    __asm__ volatile("ldmxcsr %0" ::"m"(mx));
}

/* Calls with the argument's raw bits (no x87 load, which would quiet a
 * signaling NaN) and the result stored without one either. */
static uint64_t call_x87(void *f, uint64_t x) {
    uint64_t r;
    __asm__ volatile("pushl %2\n\tpushl %1\n\tcall *%3\n\taddl $8, %%esp\n\tfstpl %0"
                     : "=m"(r)
                     : "r"((uint32_t)x), "r"((uint32_t)(x >> 32)), "r"(f)
                     : "eax", "ecx", "edx", "memory");
    return r;
}
static uint64_t call_modf(void *f, uint64_t x, uint64_t *ip) {
    uint64_t r;
    static volatile uint32_t a[3]; /* static: not esp-relative across the pushes */
    static void *volatile fn;
    a[0] = (uint32_t)x, a[1] = (uint32_t)(x >> 32), a[2] = (uint32_t)(uintptr_t)ip, fn = f;
    __asm__ volatile("pushl %3\n\tpushl %2\n\tpushl %1\n\tcall *%4\n\taddl $12, %%esp\n\tfstpl %0"
                     : "=m"(r)
                     : "m"(a[0]), "m"(a[1]), "m"(a[2]), "m"(fn)
                     : "eax", "ecx", "edx", "memory");
    return r;
}
/* An x87-ABI entry (_CI*): the argument in ST0 (loaded from memory, as a
 * compiler passes it), the result in ST0; `under` pushes 80-bit pi first,
 * a register no double holds, popped after. */
static int under;
static uint64_t call_ci1(void *f, uint64_t x) {
    uint64_t r;
    static volatile uint64_t a;
    static void *volatile fn;
    a = x, fn = f;
    if (under) {
        __asm__ volatile("fldpi\n\tfldl %1\n\tcall *%2\n\tfstpl %0\n\tfstp %%st(0)" : "=m"(r) : "m"(a), "m"(fn) : "eax", "ecx", "edx", "memory");
    } else {
        __asm__ volatile("fldl %1\n\tcall *%2\n\tfstpl %0" : "=m"(r) : "m"(a), "m"(fn) : "eax", "ecx", "edx", "memory");
    }
    return r;
}
static uint64_t call_ci2(void *f, uint64_t y, uint64_t x) {
    uint64_t r;
    static volatile uint64_t a, b;
    static void *volatile fn;
    a = y, b = x, fn = f;
    if (under) {
        __asm__ volatile("fldpi\n\tfldl %1\n\tfldl %2\n\tcall *%3\n\tfstpl %0\n\tfstp %%st(0)" : "=m"(r) : "m"(a), "m"(b), "m"(fn) : "eax", "ecx", "edx", "memory");
    } else {
        __asm__ volatile("fldl %1\n\tfldl %2\n\tcall *%3\n\tfstpl %0" : "=m"(r) : "m"(a), "m"(b), "m"(fn) : "eax", "ecx", "edx", "memory");
    }
    return r;
}
static uint64_t call_x87_2(void *f, uint64_t y, uint64_t x) {
    uint64_t r;
    static volatile uint32_t a[4];
    static void *volatile fn;
    a[0] = (uint32_t)y, a[1] = (uint32_t)(y >> 32), a[2] = (uint32_t)x, a[3] = (uint32_t)(x >> 32), fn = f;
    __asm__ volatile("pushl %4\n\tpushl %3\n\tpushl %2\n\tpushl %1\n\tcall *%5\n\taddl $16, %%esp\n\tfstpl %0"
                     : "=m"(r)
                     : "m"(a[0]), "m"(a[1]), "m"(a[2]), "m"(a[3]), "m"(fn)
                     : "eax", "ecx", "edx", "memory");
    return r;
}
static uint64_t call_sse2_2(void *f, uint64_t y, uint64_t x) {
    uint64_t r;
    __asm__ volatile("movq %1, %%xmm0\n\tmovq %2, %%xmm1\n\tcall *%3\n\tmovq %%xmm0, %0"
                     : "=m"(r)
                     : "m"(y), "m"(x), "r"(f)
                     : "eax", "ecx", "edx", "xmm0", "xmm1", "xmm2", "xmm3", "xmm4", "xmm5", "xmm6", "xmm7", "memory");
    return r;
}

/* _libm_sse2_*_precise: the argument and result in xmm0. */
static uint64_t call_sse2(void *f, uint64_t x) {
    uint64_t r;
    __asm__ volatile("movq %1, %%xmm0\n\tcall *%2\n\tmovq %%xmm0, %0"
                     : "=m"(r)
                     : "m"(x), "r"(f)
                     : "eax", "ecx", "edx", "xmm0", "xmm1", "xmm2", "xmm3", "xmm4", "xmm5", "xmm6", "xmm7", "memory");
    return r;
}

static void run1(const char *name, void *f) {
    if (!f) {
        printf("%-28s missing\n", name);
        return;
    }
    hash = 0xcbf29ce484222325ull;
    for (int i = 0; i < nin; i++) {
        *get_errno() = 0;
        uint64_t r = call_x87(f, bits(in[i]));
        record(name, bits(in[i]), r, 0);
    }
    printf("%-28s %d %016llx\n", name, nin, (unsigned long long)hash);
}
static void run_sse2(const char *name, void *f) {
    if (!f) {
        printf("%-28s missing\n", name);
        return;
    }
    hash = 0xcbf29ce484222325ull;
    for (int i = 0; i < nin; i++) {
        *get_errno() = 0;
        uint64_t r = call_sse2(f, bits(in[i]));
        record(name, bits(in[i]), r, 0);
    }
    printf("%-28s %d %016llx\n", name, nin, (unsigned long long)hash);
}

enum { X87, CI, SSE2 };
static void run_any(const char *name, void *f, int abi, int two) {
    if (!f) {
        printf("%-28s missing\n", name);
        return;
    }
    hash = 0xcbf29ce484222325ull;
    for (int i = 0; i < nin; i++) {
        uint64_t x = bits(in[i]), y = bits(in[(i * 7 + 3) % nin]), r;
        *get_errno() = 0;
        if (two) {
            r = abi == X87 ? call_x87_2(f, x, y) : abi == CI ? call_ci2(f, x, y) : call_sse2_2(f, x, y);
        } else {
            r = abi == X87 ? call_x87(f, x) : abi == CI ? call_ci1(f, x) : call_sse2(f, x);
        }
        record(name, x, r, two ? y : 0);
    }
    printf("%-28s %d %016llx\n", name, nin, (unsigned long long)hash);
}
static void set_cw(unsigned short cw) {
    __asm__ volatile("fldcw %0" ::"m"(cw));
}
static void trig(HMODULE m, const char *mode) {
    static const struct { const char *name; int abi, two; } fs[] = {
        {"sin", X87, 0}, {"cos", X87, 0}, {"tan", X87, 0}, {"atan", X87, 0}, {"atan2", X87, 1}, {"rint", X87, 0},
        {"_CIsin", CI, 0}, {"_CIcos", CI, 0}, {"_CItan", CI, 0}, {"_CIatan", CI, 0}, {"_CIatan2", CI, 1},
        {"_libm_sse2_sin_precise", SSE2, 0}, {"_libm_sse2_cos_precise", SSE2, 0}, {"_libm_sse2_tan_precise", SSE2, 0},
        {"_libm_sse2_atan_precise", SSE2, 0}, {"__libm_sse2_atan2", SSE2, 1},
    };
    for (unsigned i = 0; i < sizeof fs / sizeof *fs; i++) {
        char n[80];
        if (under && fs[i].abi != CI) continue;
        snprintf(n, sizeof n, "%s/%s", fs[i].name, mode);
        run_any(n, (void *)GetProcAddress(m, fs[i].name), fs[i].abi, fs[i].two);
    }
}

int main(int argc, char **argv) {
    if (argc > 2 && !strcmp(argv[1], "-v")) verbose = argv[2];
    HMODULE m = LoadLibraryA("msvcr120.dll");
    if (!m) {
        printf("no msvcr120.dll\n");
        return 2;
    }
    get_errno = (errno_t_)GetProcAddress(m, "_errno");
    inputs();
    run1("floor", (void *)GetProcAddress(m, "floor"));
    run1("ceil", (void *)GetProcAddress(m, "ceil"));
    void *mf = (void *)GetProcAddress(m, "modf");
    hash = 0xcbf29ce484222325ull;
    for (int i = 0; i < nin; i++) {
        uint64_t ip = bits(12345.0);
        *get_errno() = 0;
        uint64_t r = call_modf(mf, bits(in[i]), &ip);
        record("modf", bits(in[i]), r, ip);
    }
    printf("%-28s %d %016llx\n", "modf", nin, (unsigned long long)hash);
    static const char *rcn[] = {"nearest", "down", "up", "zero"};
    for (unsigned rc = 0; rc < 4; rc++) {
        char n1[64], n2[64];
        snprintf(n1, sizeof n1, "sqrt/%s", rcn[rc]);
        snprintf(n2, sizeof n2, "_libm_sse2_sqrt_precise/%s", rcn[rc]);
        set_round(rc);
        run1(n1, (void *)GetProcAddress(m, "sqrt"));
        run_sse2(n2, (void *)GetProcAddress(m, "_libm_sse2_sqrt_precise"));
        set_round(0);
    }
    unsigned short cw0;
    __asm__ volatile("fnstcw %0" : "=m"(cw0));
    trig(m, "default");
    set_cw((cw0 & ~0xc00) | 0x800); /* rounding up */
    trig(m, "up");
    set_cw(cw0 | 0x300); /* 64-bit precision */
    trig(m, "pc64");
    set_cw(cw0);
    under = 1;
    trig(m, "under");
    under = 0;
    return 0;
}
