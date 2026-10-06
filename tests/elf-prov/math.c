/* ELF providers' conformance: libm's floor, ceil, trunc, round and their
 * float forms, bit for bit over a grid of values plus the special ones
 * (zeros, subnormals, infinities, quiet and signalling NaNs with payloads,
 * halfway cases, values past 2^52 / 2^23). Prints each result's bits. */
#include <math.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

static double d_of(uint64_t b) { double d; memcpy(&d, &b, 8); return d; }
static float f_of(uint32_t b) { float f; memcpy(&f, &b, 4); return f; }
static uint64_t b_of(double d) { uint64_t b; memcpy(&b, &d, 8); return b; }
static uint32_t bf_of(float f) { uint32_t b; memcpy(&b, &f, 4); return b; }

int main(void) {
    double (*df[])(double) = {floor, ceil, trunc, round};
    float (*ff[])(float) = {floorf, ceilf, truncf, roundf};
    static const uint64_t special[] = {0, 0x8000000000000000ull, 1, 0x800fffffffffffffull, 0x7ff0000000000000ull, 0xfff0000000000000ull,
                                       0x7ff8000000000000ull, 0xfff8000000001234ull, 0x7ff0000000000001ull, 0x7ff4000000abcdefull, 0x4330000000000000ull,
                                       0x4330000000000001ull, 0x432fffffffffffffull, 0xc32fffffffffffffull, 0x3fe0000000000000ull, 0xbfe0000000000000ull,
                                       0x3fdfffffffffffffull, 0x3ff8000000000000ull, 0xc004000000000000ull, 0x7fefffffffffffffull};
    unsigned long n = 0;
    uint64_t h = 1469598103934665603ull;
    for (unsigned k = 0; k < 4; ++k) {
        for (unsigned i = 0; i < sizeof special / sizeof *special; ++i) {
            printf("d%u %016llx -> %016llx\n", k, (unsigned long long)special[i], (unsigned long long)b_of(df[k](d_of(special[i]))));
        }
        for (int i = -40000; i <= 40000; ++i) {
            double x = i / 64.0 + i * 1e-9;
            h = (h ^ b_of(df[k](x))) * 1099511628211ull, ++n;
            h = (h ^ b_of(df[k](x * 1e10))) * 1099511628211ull, ++n;
            float y = (float)(i / 32.0);
            h = (h ^ bf_of(ff[k](y))) * 1099511628211ull, ++n;
            h = (h ^ bf_of(ff[k](y * 1e5f))) * 1099511628211ull, ++n;
        }
        static const uint32_t fspecial[] = {0, 0x80000000u, 1, 0x807fffffu, 0x7f800000u, 0xff800000u, 0x7fc00000u, 0xffc01234u, 0x7f800001u,
                                            0x7fa00abcu, 0x4b000000u, 0x4b000001u, 0x4affffffu, 0xcaffffffu, 0x3f000000u, 0xbf000000u, 0x3effffffu};
        for (unsigned i = 0; i < sizeof fspecial / sizeof *fspecial; ++i) {
            printf("f%u %08x -> %08x\n", k, fspecial[i], bf_of(ff[k](f_of(fspecial[i]))));
        }
    }
    printf("grid %lu %016llx\n", n, (unsigned long long)h);
    return 0;
}
