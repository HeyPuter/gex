/* Floating point on the PowerPC guest: libm, fused multiply-add in both
 * precisions (fma, fmaf), conversions (fctiw/fctiwz, frsp), single precision
 * arithmetic, signed zeros, and the special values. */
#include <math.h>
#include <stdio.h>
#include <stdint.h>
#include <string.h>
static uint64_t bits(double d) { uint64_t u; memcpy(&u, &d, 8); return u; }
static uint32_t fbits(float f) { uint32_t u; memcpy(&u, &f, 4); return u; }
int main(void) {
    volatile double a = 1.0 / 3.0, b = 1e-300, c = -0.0, big = 1e308;
    volatile float fa = 1.0f / 3.0f, fb = 16777217.0f, fz = -0.0f;
    printf("libm %.17g %.17g %.17g %.17g\n", sin(a), exp(a), pow(a, 2.5), log1p(b));
    printf("fma %016llx %016llx %016llx\n", (unsigned long long)bits(fma(a, 3.0, -1.0)),
           (unsigned long long)bits(fma(0.0, -1.0, -0.0)), (unsigned long long)bits(fma(big, 10.0, -big)));
    printf("fmaf %08x %08x\n", fbits(fmaf(fa, 3.0f, -1.0f)), fbits(fmaf(fb, fb, -fb)));
    printf("single %08x %08x %08x\n", fbits(fa * fa), fbits(fa / 7.0f), fbits(sqrtf(fa)));
    printf("conv %d %d %ld %u %d\n", (int)-2.7, (int)2.7, lrint(2.5), (unsigned)3.9e9, (int)(float)a);
    printf("zero %016llx %08x %g %g\n", (unsigned long long)bits(c * 2), fbits(fz + 0.0f), 1.0 / c, copysign(3.0, c));
    printf("special %g %g %d %d\n", big * 10, -big * 10, isnan(sqrt(-1.0)) != 0, isinf(1.0 / 0.0) != 0);
    printf("round %g %g %g %g %g\n", floor(-2.5), ceil(-2.5), round(-2.5), trunc(-2.5), nearbyint(2.5));
    printf("float->double %.9g %.17g\n", (double)fa, (double)(float)a);
    return 0;
}
