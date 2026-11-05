/* FRINTA rounds to nearest with ties away from zero; FRINTN ties to even.
 * Scalar double and single, and the vector 2D and 4S forms, on halfway
 * values, values a hair below a half, the edges of the integers, -0, the
 * infinities and NaN. Printed as bit patterns so a sign of zero counts. */
#include <stdint.h>
#include <stdio.h>
#include <string.h>

static uint64_t bits_d(double d) { uint64_t u; memcpy(&u, &d, 8); return u; }
static uint32_t bits_s(float f) { uint32_t u; memcpy(&u, &f, 4); return u; }

static double frinta_d(double x) { double r; __asm__("frinta %d0, %d1" : "=w"(r) : "w"(x)); return r; }
static double frintn_d(double x) { double r; __asm__("frintn %d0, %d1" : "=w"(r) : "w"(x)); return r; }
static float frinta_s(float x) { float r; __asm__("frinta %s0, %s1" : "=w"(r) : "w"(x)); return r; }

typedef double v2d __attribute__((vector_size(16)));
typedef float v4s __attribute__((vector_size(16)));

int main(void) {
    static const double d[] = {
        0.5, -0.5, 1.5, -1.5, 2.5, -2.5, 3.5, 0.49999999999999994, -0.49999999999999994,
        0.3, -0.3, -0.0, 0.0, 4503599627370495.5, -4503599627370495.5, 4503599627370497.0,
        1e300, -1e300, 1.0 / 0.0, -1.0 / 0.0, 0.0 / 0.0, 2.4999999999999996, 1e-310, -1e-310,
    };
    for (unsigned i = 0; i < sizeof d / sizeof d[0]; i++)
        printf("d %016llx a %016llx n %016llx\n", (unsigned long long)bits_d(d[i]),
               (unsigned long long)bits_d(frinta_d(d[i])), (unsigned long long)bits_d(frintn_d(d[i])));
    static const float s[] = {
        0.5f, -0.5f, 1.5f, -2.5f, 0.49999997f, -0.49999997f, 8388607.5f, -8388607.5f,
        -0.0f, -0.3f, 1.0f / 0.0f, 0.0f / 0.0f, 1e-40f,
    };
    for (unsigned i = 0; i < sizeof s / sizeof s[0]; i++)
        printf("s %08x a %08x\n", bits_s(s[i]), bits_s(frinta_s(s[i])));
    for (unsigned i = 0; i + 1 < sizeof d / sizeof d[0]; i += 2) {
        v2d v = {d[i], d[i + 1]}, r;
        __asm__("frinta %0.2d, %1.2d" : "=w"(r) : "w"(v));
        printf("2d %016llx %016llx\n", (unsigned long long)bits_d(r[0]), (unsigned long long)bits_d(r[1]));
    }
    for (unsigned i = 0; i + 3 < sizeof s / sizeof s[0]; i += 4) {
        v4s v = {s[i], s[i + 1], s[i + 2], s[i + 3]}, r;
        __asm__("frinta %0.4s, %1.4s" : "=w"(r) : "w"(v));
        printf("4s %08x %08x %08x %08x\n", bits_s(r[0]), bits_s(r[1]), bits_s(r[2]), bits_s(r[3]));
        float lo[2] = {s[i], s[i + 1]};
        __attribute__((vector_size(8))) float v2 = {lo[0], lo[1]}, r2;
        __asm__("frinta %0.2s, %1.2s" : "=w"(r2) : "w"(v2));
        printf("2s %08x %08x\n", bits_s(r2[0]), bits_s(r2[1]));
    }
    return 0;
}
