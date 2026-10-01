/* The x87's arithmetic and compares: fadd, fsub(r), fmul, fdiv(r) in the
 * register, popping and memory forms (single, double, 16/32-bit integer),
 * and fcom, fucom, fcomi, fucomi, ficom, ftst, plus operations on an empty
 * register. Every case runs under all 16 precision and rounding modes
 * (exceptions masked); results, exception flags and condition codes are
 * hashed per operation, so the output is a digest to compare with native
 * execution. With an argument, every case is printed instead. Operands come
 * from the same edge-biased generator as x87-conversions.c. */
#include <stdint.h>
#include <stdio.h>
#include <string.h>

static uint64_t rs = 0x9e3779b97f4a7c15ull;
static uint64_t rnd(void) {
    rs ^= rs << 13;
    rs ^= rs >> 7;
    rs ^= rs << 17;
    return rs;
}

struct f80 {
    uint64_t sig;
    uint16_t se;
} __attribute__((packed));

static struct f80 rand80(void) {
    struct f80 v;
    uint64_t r = rnd();
    uint64_t sig;
    switch (rnd() % 6) {
    case 0: sig = 1ull << 63 | (r & 0xffff) << 40; break;
    case 1: sig = 1ull << 63 | (r & 0x7ff); break;
    case 2: sig = ~0ull - (r & 0xfff); break;
    case 3: sig = 1ull << 63 | (r >> 1 & ~0x7ffull); break;
    case 4: sig = 1ull << 63 | (r >> 1 & ~0xffffffffffull) | (r & 0xffffff) << 16; break;
    default: sig = 1ull << 63 | r; break;
    }
    unsigned e;
    switch (rnd() % 14) {
    case 0: e = 0; break;
    case 1: e = 1 + rnd() % 70; break;
    case 2: e = 0x7ffe - rnd() % 70; break;
    case 3: e = 0x7fff; break;
    case 4: e = 0x3fff - 1100 + rnd() % 80; break;  /* double's denormals */
    case 5: e = 0x3fff + 1000 + rnd() % 50; break;  /* double's overflow */
    case 6: e = 0x3fff - 160 + rnd() % 40; break;   /* single's denormals */
    case 7: e = 0x3fff + 110 + rnd() % 30; break;   /* single's overflow */
    case 8: e = 0x3fff + rnd() % 66; break;         /* integers */
    case 9: e = 0x3fff - 3 + rnd() % 6; break;      /* around one */
    default: e = 0x3fff - 64 + rnd() % 128; break;
    }
    if (e == 0) {
        switch (rnd() % 4) {
        case 0: sig = 0; break;
        case 1: break; /* pseudo-denormal */
        default: sig >>= 1 + rnd() % 63; break;
        }
    } else if (e == 0x7fff) {
        switch (rnd() % 5) {
        case 0: sig = 1ull << 63; break;
        case 1: sig = 0xc000000000000000ull | r >> 2; break;
        case 2: sig = 0x8000000000000000ull | (r >> 2 | 1); break;
        case 3: sig = r >> 1; break; /* pseudo */
        default: sig = 0xc000000000000000ull; break;
        }
    } else if (rnd() % 40 == 0) {
        sig >>= 1; /* unnormal */
    }
    v.sig = sig;
    v.se = (uint16_t)(e | (rnd() & 1) << 15);
    return v;
}

static uint64_t rand_bits(int bits) {
    uint64_t r = rnd();
    if (bits == 64) {
        switch (rnd() % 6) {
        case 0: return r & 0x800fffffffffffffull;  /* denormals */
        case 1: return r | 0x7ff0000000000000ull;  /* NaN, infinity */
        case 2: return r & 0x8000000000000000ull;  /* zeros */
        case 3: return (r & 0x800fffffffffffffull) | 0x7ff0000000000000ull;
        default: return r;
        }
    }
    if (bits == 32) {
        switch (rnd() % 6) {
        case 0: return r & 0x807fffff;
        case 1: return (r | 0x7f800000) & 0xffffffff;
        case 2: return r & 0x80000000;
        case 3: return (r & 0x807fffff) | 0x7f800000;
        default: return r & 0xffffffff;
        }
    }
    return r & 0xffff;
}

static int verbose;

/* FNV-1a over a case's result, per operation. */
struct acc {
    const char *name;
    uint64_t h;
    unsigned n;
};

static void mix(struct acc *a, const void *p, size_t len, uint16_t sw) {
    const unsigned char *b = p;
    for (size_t i = 0; i < len; i++) a->h = (a->h ^ b[i]) * 0x100000001b3ull;
    uint16_t f = sw & 0x27f; /* exception flags, SF and C1 */
    a->h = (a->h ^ (f & 0xff)) * 0x100000001b3ull;
    a->h = (a->h ^ (f >> 8)) * 0x100000001b3ull;
    a->n++;
    if (verbose) {
        printf("%s:", a->name);
        for (size_t i = 0; i < len; i++) printf("%02x", b[len - 1 - i]);
        printf(" sw=%04x\n", f);
    }
}

#define PRE "fninit\n fldcw %[cw]\n"

/* One binary operation on two extended operands, st(0) = a, st(1) = b. */
#define BIN(acc, ins) do { \
    __asm__ volatile(PRE "fldt %[b]\n fldt %[a]\n" ins "\n fnstsw %[sw]\n fstpt %[o]\n fstp %%st(0)" \
                     : [o] "=m"(o80), [sw] "=m"(sw) : [cw] "m"(cw), [a] "m"(x), [b] "m"(y) : "st", "st(1)"); \
    mix(&acc, &o80, 10, sw); \
} while (0)

/* A popping form, st(1) op= st(0) then pop: the result is st(0) after. */
#define BINP(acc, ins) do { \
    __asm__ volatile(PRE "fldt %[b]\n fldt %[a]\n" ins "\n fnstsw %[sw]\n fstpt %[o]" \
                     : [o] "=m"(o80), [sw] "=m"(sw) : [cw] "m"(cw), [a] "m"(x), [b] "m"(y) : "st", "st(1)"); \
    mix(&acc, &o80, 10, sw); \
} while (0)

/* A memory-operand form: st(0) = a op [m]. */
#define MEM(acc, ins, val) do { \
    __asm__ volatile(PRE "fldt %[a]\n" ins " %[mo]\n fnstsw %[sw]\n fstpt %[o]" \
                     : [o] "=m"(o80), [sw] "=m"(sw) : [cw] "m"(cw), [a] "m"(x), [mo] "m"(val) : "st"); \
    mix(&acc, &o80, 10, sw); \
} while (0)

/* A compare: the status word (C0-C3 included) and, for fcomi, the flags. */
#define CMP(acc, ins) do { \
    __asm__ volatile(PRE "fldt %[b]\n fldt %[a]\n" ins "\n fnstsw %[sw]\n fninit" \
                     : [sw] "=m"(sw) : [cw] "m"(cw), [a] "m"(x), [b] "m"(y) : "st", "st(1)"); \
    uint16_t c = sw & 0x4700; mix(&acc, &c, 2, sw); \
} while (0)

int main(int argc, char **argv) {
    verbose = argc > 1;
#define A(n) struct acc n = {#n, 0xcbf29ce484222325ull}
    A(fadd); A(fsub); A(fsubr); A(fmul); A(fdiv); A(fdivr); A(faddp_sti); A(fsubp_sti); A(fdivrp_sti);
    A(fadd_m64); A(fmul_m32); A(fsub_m64); A(fdivr_m32); A(fiadd_m32); A(fimul_m16); A(fidiv_m32);
    A(fcom); A(fucom); A(fcomi); A(fucomi); A(fcom_m64); A(ficom_m32); A(ftst); A(underflow);
    for (int iter = 0; iter < 6000; iter++) {
        for (int mode = 0; mode < 16; mode++) {
            uint16_t cw = 0x7f | (uint16_t)((mode & 3) << 8) | (uint16_t)((mode >> 2) << 10);
            uint16_t sw;
            struct f80 x = rand80(), y = rand80(), o80;
            if (rnd() % 8 == 0) y = x;
            uint64_t d = rand_bits(64);
            uint32_t f = (uint32_t)rand_bits(32);
            uint32_t i32 = (uint32_t)rnd();
            uint16_t i16 = (uint16_t)rnd();
            if (rnd() % 4 == 0) i32 >>= rnd() % 32;
            BIN(fadd, "fadd %%st(1), %%st");
            BIN(fsub, "fsub %%st(1), %%st");
            BIN(fsubr, "fsubr %%st(1), %%st");
            BIN(fmul, "fmul %%st(1), %%st");
            BIN(fdiv, "fdiv %%st(1), %%st");
            BIN(fdivr, "fdivr %%st(1), %%st");
            /* st(i) op= st(0), popping: the result lands in st(0) after the pop. */
            BINP(faddp_sti, "faddp %%st, %%st(1)");
            BINP(fsubp_sti, "fsubp %%st, %%st(1)");
            BINP(fdivrp_sti, "fdivrp %%st, %%st(1)");
            MEM(fadd_m64, "faddl", d);
            MEM(fmul_m32, "fmuls", f);
            MEM(fsub_m64, "fsubl", d);
            MEM(fdivr_m32, "fdivrs", f);
            MEM(fiadd_m32, "fiaddl", i32);
            MEM(fimul_m16, "fimuls", i16);
            MEM(fidiv_m32, "fidivl", i32);
            CMP(fcom, "fcom %%st(1)");
            CMP(fucom, "fucom %%st(1)");
            {
                uint8_t z, p, c;
                __asm__ volatile(PRE "fldt %[b]\n fldt %[a]\n fcomi %%st(1), %%st\n setz %[z]\n setp %[p]\n setc %[c]\n fnstsw %[sw]\n fninit"
                                 : [sw] "=m"(sw), [z] "=r"(z), [p] "=r"(p), [c] "=r"(c) : [cw] "m"(cw), [a] "m"(x), [b] "m"(y) : "st", "st(1)", "cc");
                uint8_t zpc = z | p << 1 | c << 2;
                mix(&fcomi, &zpc, 1, sw);
                __asm__ volatile(PRE "fldt %[b]\n fldt %[a]\n fucomi %%st(1), %%st\n setz %[z]\n setp %[p]\n setc %[c]\n fnstsw %[sw]\n fninit"
                                 : [sw] "=m"(sw), [z] "=r"(z), [p] "=r"(p), [c] "=r"(c) : [cw] "m"(cw), [a] "m"(x), [b] "m"(y) : "st", "st(1)", "cc");
                zpc = z | p << 1 | c << 2;
                mix(&fucomi, &zpc, 1, sw);
            }
            {
                __asm__ volatile(PRE "fldt %[a]\n fcoml %[m]\n fnstsw %[sw]\n fninit" : [sw] "=m"(sw) : [cw] "m"(cw), [a] "m"(x), [m] "m"(d) : "st");
                uint16_t c = sw & 0x4700; mix(&fcom_m64, &c, 2, sw);
                __asm__ volatile(PRE "fldt %[a]\n ficoml %[m]\n fnstsw %[sw]\n fninit" : [sw] "=m"(sw) : [cw] "m"(cw), [a] "m"(x), [m] "m"(i32) : "st");
                c = sw & 0x4700; mix(&ficom_m32, &c, 2, sw);
                __asm__ volatile(PRE "fldt %[a]\n ftst\n fnstsw %[sw]\n fninit" : [sw] "=m"(sw) : [cw] "m"(cw), [a] "m"(x) : "st");
                c = sw & 0x4700; mix(&ftst, &c, 2, sw);
            }
        }
    }
    for (int mode = 0; mode < 16; mode++) {
        uint16_t cw = 0x7f | (uint16_t)((mode & 3) << 8) | (uint16_t)((mode >> 2) << 10);
        uint16_t sw;
        struct f80 o80;
        /* Operating on an empty register: IE and SF, the indefinite. */
        __asm__ volatile(PRE "fld1\n fadd %%st(3), %%st\n fnstsw %[sw]\n fstpt %[o]" : [o] "=m"(o80), [sw] "=m"(sw) : [cw] "m"(cw) : "st");
        mix(&underflow, &o80, 10, sw);
        __asm__ volatile(PRE "fld1\n fcom %%st(2)\n fnstsw %[sw]\n fninit" : [sw] "=m"(sw) : [cw] "m"(cw) : "st");
        mix(&underflow, &sw, 2, sw);
        __asm__ volatile(PRE "ftst\n fnstsw %[sw]\n fninit" : [sw] "=m"(sw) : [cw] "m"(cw) : "st");
        mix(&underflow, &sw, 2, sw);
    }
    __asm__ volatile("fninit");
    struct acc *all[] = {&fadd, &fsub, &fsubr, &fmul, &fdiv, &fdivr, &faddp_sti, &fsubp_sti, &fdivrp_sti,
                         &fadd_m64, &fmul_m32, &fsub_m64, &fdivr_m32, &fiadd_m32, &fimul_m16, &fidiv_m32,
                         &fcom, &fucom, &fcomi, &fucomi, &fcom_m64, &ficom_m32, &ftst, &underflow};
    if (!verbose)
        for (unsigned i = 0; i < sizeof all / sizeof *all; i++)
            printf("%-12s %6u cases  %016llx\n", all[i]->name, all[i]->n, (unsigned long long)all[i]->h);
    return 0;
}
