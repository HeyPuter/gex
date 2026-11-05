/* The x87's conversions between the register format and memory: fld and
 * fild from single, double, extended and 16/32/64-bit integers, fst/fstp to
 * single, double and extended, fist/fistp/fisttp to integers, fbld and fbstp
 * (18-digit packed BCD), the constants,
 * and stack overflow and underflow. Every case runs under all 16 precision
 * and rounding modes (exceptions masked); the result bytes and the status
 * word's exception flags and C1 are hashed per operation, so the output is a
 * digest to compare with native execution. With an argument, every case is
 * printed instead. Operands are biased toward the edges: zeros, denormals
 * and pseudo-denormals, the ends of every format's range, infinities, both
 * kinds of NaN, unnormals, and values next to rounding boundaries. */
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

int main(int argc, char **argv) {
    verbose = argc > 1;
    struct acc a_st64 = {"fstp m64", 0xcbf29ce484222325ull}, a_st32 = {"fstp m32", 0xcbf29ce484222325ull},
               a_st80 = {"fstp m80", 0xcbf29ce484222325ull}, a_ist16 = {"fistp m16", 0xcbf29ce484222325ull},
               a_ist32 = {"fistp m32", 0xcbf29ce484222325ull}, a_ist64 = {"fistp m64", 0xcbf29ce484222325ull},
               a_itt16 = {"fisttp m16", 0xcbf29ce484222325ull}, a_itt32 = {"fisttp m32", 0xcbf29ce484222325ull},
               a_itt64 = {"fisttp m64", 0xcbf29ce484222325ull}, a_ld64 = {"fld m64", 0xcbf29ce484222325ull},
               a_ld32 = {"fld m32", 0xcbf29ce484222325ull}, a_ild16 = {"fild m16", 0xcbf29ce484222325ull},
               a_ild32 = {"fild m32", 0xcbf29ce484222325ull}, a_ild64 = {"fild m64", 0xcbf29ce484222325ull},
               a_const = {"fld const", 0xcbf29ce484222325ull}, a_stack = {"stack faults", 0xcbf29ce484222325ull},
               a_bst = {"fbstp", 0xcbf29ce484222325ull}, a_ibst = {"fild fbstp", 0xcbf29ce484222325ull},
               a_bld = {"fbld", 0xcbf29ce484222325ull};
    for (int iter = 0; iter < 4000; iter++) {
        for (int mode = 0; mode < 16; mode++) {
            uint16_t cw = 0x7f | (uint16_t)((mode & 3) << 8) | (uint16_t)((mode >> 2) << 10);
            uint16_t sw;
            struct f80 x = rand80(), o80;
            uint64_t o64 = 0;
            uint32_t o32 = 0;
            uint16_t o16 = 0;

            __asm__ volatile(PRE "fldt %[x]\n fstpl %[o]\n fnstsw %[sw]"
                             : [o] "=m"(o64), [sw] "=m"(sw) : [cw] "m"(cw), [x] "m"(x) : "st");
            mix(&a_st64, &o64, 8, sw);
            __asm__ volatile(PRE "fldt %[x]\n fstps %[o]\n fnstsw %[sw]"
                             : [o] "=m"(o32), [sw] "=m"(sw) : [cw] "m"(cw), [x] "m"(x) : "st");
            mix(&a_st32, &o32, 4, sw);
            __asm__ volatile(PRE "fldt %[x]\n fstpt %[o]\n fnstsw %[sw]"
                             : [o] "=m"(o80), [sw] "=m"(sw) : [cw] "m"(cw), [x] "m"(x) : "st");
            mix(&a_st80, &o80, 10, sw);
            __asm__ volatile(PRE "fldt %[x]\n fistps %[o]\n fnstsw %[sw]"
                             : [o] "=m"(o16), [sw] "=m"(sw) : [cw] "m"(cw), [x] "m"(x) : "st");
            mix(&a_ist16, &o16, 2, sw);
            __asm__ volatile(PRE "fldt %[x]\n fistpl %[o]\n fnstsw %[sw]"
                             : [o] "=m"(o32), [sw] "=m"(sw) : [cw] "m"(cw), [x] "m"(x) : "st");
            mix(&a_ist32, &o32, 4, sw);
            __asm__ volatile(PRE "fldt %[x]\n fistpll %[o]\n fnstsw %[sw]"
                             : [o] "=m"(o64), [sw] "=m"(sw) : [cw] "m"(cw), [x] "m"(x) : "st");
            mix(&a_ist64, &o64, 8, sw);
            __asm__ volatile(PRE "fldt %[x]\n fisttps %[o]\n fnstsw %[sw]"
                             : [o] "=m"(o16), [sw] "=m"(sw) : [cw] "m"(cw), [x] "m"(x) : "st");
            mix(&a_itt16, &o16, 2, sw);
            __asm__ volatile(PRE "fldt %[x]\n fisttpl %[o]\n fnstsw %[sw]"
                             : [o] "=m"(o32), [sw] "=m"(sw) : [cw] "m"(cw), [x] "m"(x) : "st");
            mix(&a_itt32, &o32, 4, sw);
            __asm__ volatile(PRE "fldt %[x]\n fisttpll %[o]\n fnstsw %[sw]"
                             : [o] "=m"(o64), [sw] "=m"(sw) : [cw] "m"(cw), [x] "m"(x) : "st");
            mix(&a_itt64, &o64, 8, sw);

            uint64_t d = rand_bits(64);
            __asm__ volatile(PRE "fldl %[d]\n fnstsw %[sw]\n fstpt %[o]"
                             : [o] "=m"(o80), [sw] "=m"(sw) : [cw] "m"(cw), [d] "m"(d) : "st");
            mix(&a_ld64, &o80, 10, sw);
            uint32_t f = (uint32_t)rand_bits(32);
            __asm__ volatile(PRE "flds %[d]\n fnstsw %[sw]\n fstpt %[o]"
                             : [o] "=m"(o80), [sw] "=m"(sw) : [cw] "m"(cw), [d] "m"(f) : "st");
            mix(&a_ld32, &o80, 10, sw);
            uint16_t i16 = (uint16_t)rand_bits(16);
            __asm__ volatile(PRE "filds %[d]\n fnstsw %[sw]\n fstpt %[o]"
                             : [o] "=m"(o80), [sw] "=m"(sw) : [cw] "m"(cw), [d] "m"(i16) : "st");
            mix(&a_ild16, &o80, 10, sw);
            uint32_t i32 = (uint32_t)rnd();
            if (rnd() % 4 == 0) i32 = 0x80000000u >> (rnd() % 32);
            __asm__ volatile(PRE "fildl %[d]\n fnstsw %[sw]\n fstpt %[o]"
                             : [o] "=m"(o80), [sw] "=m"(sw) : [cw] "m"(cw), [d] "m"(i32) : "st");
            mix(&a_ild32, &o80, 10, sw);
            uint64_t i64 = rnd();
            if (rnd() % 4 == 0) i64 = 0x8000000000000000ull >> (rnd() % 64);
            __asm__ volatile(PRE "fildll %[d]\n fnstsw %[sw]\n fstpt %[o]"
                             : [o] "=m"(o80), [sw] "=m"(sw) : [cw] "m"(cw), [d] "m"(i64) : "st");
            mix(&a_ild64, &o80, 10, sw);

            /* fbstp: the edge-biased value, then integers around the 18
             * digits' limit, a fraction to round (a sixteenth) beside them */
            struct f80 b80;
            __asm__ volatile(PRE "fldt %[x]\n fbstp %[o]\n fnstsw %[sw]"
                             : [o] "=m"(b80), [sw] "=m"(sw) : [cw] "m"(cw), [x] "m"(x) : "st");
            mix(&a_bst, &b80, 10, sw);
            i64 = (int64_t)(rnd() >> (rnd() % 64)) * (rnd() % 2 ? 1 : -1);
            if (rnd() % 4 == 0) i64 = 999999999999999999ll + (int64_t)(rnd() % 5) - 2;
            int16_t sixteen = 16;
            __asm__ volatile(PRE "fildll %[d]\n fbstp %[o]\n fnstsw %[sw]"
                             : [o] "=m"(b80), [sw] "=m"(sw) : [cw] "m"(cw), [d] "m"(i64) : "st");
            mix(&a_ibst, &b80, 10, sw);
            __asm__ volatile(PRE "fildll %[d]\n fidivs %[k]\n fbstp %[o]\n fnstsw %[sw]"
                             : [o] "=m"(b80), [sw] "=m"(sw) : [cw] "m"(cw), [d] "m"(i64), [k] "m"(sixteen) : "st");
            mix(&a_ibst, &b80, 10, sw);
            /* fbld: 18 random decimal digits, either sign */
            unsigned char bcd[10] = {0};
            uint64_t r = rnd();
            for (int k = 0; k < 9; k++) bcd[k] = (unsigned char)((r % 10) | (r / 10 % 10) << 4), r = r / 100 ^ rnd();
            bcd[9] = rnd() % 2 ? 0x80 : 0;
            __asm__ volatile(PRE "fbld %[d]\n fnstsw %[sw]\n fstpt %[o]"
                             : [o] "=m"(o80), [sw] "=m"(sw) : [cw] "m"(cw), [d] "m"(bcd) : "st");
            mix(&a_bld, &o80, 10, sw);
        }
    }
    for (int mode = 0; mode < 16; mode++) {
        uint16_t cw = 0x7f | (uint16_t)((mode & 3) << 8) | (uint16_t)((mode >> 2) << 10);
        uint16_t sw;
        struct f80 o80;
#define K(ins)                                                                              \
    __asm__ volatile(PRE ins "\n fnstsw %[sw]\n fstpt %[o]"                                 \
                     : [o] "=m"(o80), [sw] "=m"(sw) : [cw] "m"(cw) : "st");               \
    mix(&a_const, &o80, 10, sw);
        K("fld1") K("fldl2t") K("fldl2e") K("fldpi") K("fldlg2") K("fldln2") K("fldz")
        /* Nine pushes: the ninth overflows (IE, SF, C1) and pushes the
         * indefinite. Then a store from an empty register (IE, SF). */
        struct f80 x = rand80();
        uint64_t o64;
        __asm__ volatile(PRE "fldt %[x]\n fld1\n fld1\n fld1\n fld1\n fld1\n fld1\n fld1\n fldt %[x]\n fnstsw %[sw]\n fstpt %[o]"
                         : [o] "=m"(o80), [sw] "=m"(sw) : [cw] "m"(cw), [x] "m"(x) : "st", "st(1)", "st(2)", "st(3)", "st(4)", "st(5)", "st(6)", "st(7)");
        mix(&a_stack, &o80, 10, sw);
        __asm__ volatile(PRE "fstpl %[o]\n fnstsw %[sw]" : [o] "=m"(o64), [sw] "=m"(sw) : [cw] "m"(cw) : "st");
        mix(&a_stack, &o64, 8, sw);
        __asm__ volatile(PRE "fistpl %[o]\n fnstsw %[sw]" : [o] "=m"(o64), [sw] "=m"(sw) : [cw] "m"(cw) : "st");
        mix(&a_stack, &o64, 4, sw);
    }
    __asm__ volatile("fninit");
    struct acc *all[] = {&a_st64, &a_st32, &a_st80, &a_ist16, &a_ist32, &a_ist64, &a_itt16, &a_itt32,
                         &a_itt64, &a_ld64, &a_ld32, &a_ild16, &a_ild32, &a_ild64, &a_const, &a_stack, &a_bst, &a_ibst, &a_bld};
    if (!verbose)
        for (unsigned i = 0; i < sizeof all / sizeof *all; i++)
            printf("%-12s %6u cases  %016llx\n", all[i]->name, all[i]->n, (unsigned long long)all[i]->h);
    return 0;
}
