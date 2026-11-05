/* x87 arithmetic with zero operands, and zeros stored narrow: fadd, fsub,
 * fsubr and fmul of +-0 with +-0 and with normal numbers (whose sums are
 * rounded to the precision control), and fst m32 / m64 of +-0, under all 16
 * precision and rounding modes. Results and status words are hashed, so the
 * output is a digest to compare with native execution; with an argument,
 * every case is printed instead. (x87-arith.c's random operands meet two
 * zeros too rarely to tell the sign rules apart.) */
#include <stdint.h>
#include <stdio.h>
#include <string.h>

struct f80 {
    uint64_t sig;
    uint16_t se;
} __attribute__((packed));

static uint64_t rs = 0x2545f4914f6cdd1dull;
static uint64_t rnd(void) {
    rs ^= rs << 13;
    rs ^= rs >> 7;
    rs ^= rs << 17;
    return rs;
}

static uint64_t hash = 0xcbf29ce484222325ull;
static int verbose;
static void mix(const void *p, size_t n) {
    const unsigned char *c = p;
    for (size_t i = 0; i < n; i++)
        hash = (hash ^ c[i]) * 0x100000001b3ull;
}

/* op: 0 fadd, 1 fsub (a - b), 2 fsubr (b - a), 3 fmul. */
static void arith(int op, struct f80 a, struct f80 b, uint16_t cw) {
    struct f80 r;
    uint16_t sw;
    __asm__ volatile("fninit\n\tfldcw %4\n\tfldt %3\n\tfldt %2\n\t" /* st0 = a, st1 = b */
                     "cmpl $0, %5\n\tjne 1f\n\tfaddp\n\tjmp 4f\n"
                     "1:\n\tcmpl $1, %5\n\tjne 2f\n\tfsubrp\n\tjmp 4f\n"
                     "2:\n\tcmpl $2, %5\n\tjne 3f\n\tfsubp\n\tjmp 4f\n"
                     "3:\n\tfmulp\n"
                     "4:\n\tfnstsw %%ax\n\tmovw %%ax, %1\n\tfstpt %0\n\tfninit"
                     : "=m"(r), "=m"(sw)
                     : "m"(a), "m"(b), "m"(cw), "r"(op)
                     : "eax", "memory", "cc");
    sw &= 0x3f | 0x200;
    mix(&r, sizeof r);
    mix(&sw, sizeof sw);
    if (verbose)
        printf("op%d %04x:%016llx %04x:%016llx cw %04x -> %04x:%016llx sw %04x\n", op, a.se,
               (unsigned long long)a.sig, b.se, (unsigned long long)b.sig, cw, r.se,
               (unsigned long long)r.sig, sw);
}

static void narrow(struct f80 a, uint16_t cw) {
    float f;
    double d;
    uint16_t sw1, sw2;
    __asm__ volatile("fninit\n\tfldcw %3\n\tfldt %2\n\tfsts %0\n\tfnstsw %%ax\n\tmovw %%ax, %1\n\t"
                     "fninit"
                     : "=m"(f), "=m"(sw1)
                     : "m"(a), "m"(cw)
                     : "eax", "memory");
    __asm__ volatile("fninit\n\tfldcw %3\n\tfldt %2\n\tfstl %0\n\tfnstsw %%ax\n\tmovw %%ax, %1\n\t"
                     "fninit"
                     : "=m"(d), "=m"(sw2)
                     : "m"(a), "m"(cw)
                     : "eax", "memory");
    sw1 &= 0x23f;
    sw2 &= 0x23f;
    mix(&f, sizeof f);
    mix(&d, sizeof d);
    mix(&sw1, 2);
    mix(&sw2, 2);
    if (verbose) {
        uint32_t fb;
        uint64_t db;
        memcpy(&fb, &f, 4);
        memcpy(&db, &d, 8);
        printf("fst %04x:%016llx cw %04x -> %08x sw %04x, %016llx sw %04x\n", a.se,
               (unsigned long long)a.sig, cw, fb, sw1, (unsigned long long)db, sw2);
    }
}

static struct f80 normal(void) {
    struct f80 v;
    uint64_t r = rnd();
    switch (rnd() % 4) {
    case 0: v.sig = 1ull << 63 | (r & 0xffffff) << 40; break; /* exact in single */
    case 1: v.sig = 1ull << 63 | (r >> 1 & ~0x7ffull); break; /* exact in double */
    case 2: v.sig = ~0ull - (r & 0xfff); break;               /* rounds up and carries */
    default: v.sig = 1ull << 63 | r; break;
    }
    unsigned e;
    switch (rnd() % 4) {
    case 0: e = 1 + rnd() % 8; break;
    case 1: e = 0x7ffe - rnd() % 8; break;
    default: e = 0x3fff - 64 + rnd() % 128; break;
    }
    v.se = (uint16_t)(e | (rnd() & 1) << 15);
    return v;
}

int main(int argc, char **argv) {
    verbose = argc > 1;
    struct f80 zero[2] = {{0, 0}, {0, 0x8000}};
    for (int pc = 0; pc < 4; pc++)
        for (int rc = 0; rc < 4; rc++) {
            uint16_t cw = (uint16_t)(0x7f | pc << 8 | rc << 10);
            for (int op = 0; op < 4; op++)
                for (int i = 0; i < 2; i++)
                    for (int j = 0; j < 2; j++)
                        arith(op, zero[i], zero[j], cw);
            for (int i = 0; i < 2; i++)
                narrow(zero[i], cw);
            for (int n = 0; n < 200; n++) {
                struct f80 x = normal();
                for (int op = 0; op < 4; op++)
                    for (int i = 0; i < 2; i++) {
                        arith(op, zero[i], x, cw);
                        arith(op, x, zero[i], cw);
                    }
            }
        }
    printf("x87-zeros %016llx\n", (unsigned long long)hash);
    return 0;
}
