/* The fixed-point conversions, which no compiler emits from C.
 *
 * `scvtf d0, w0, #n` reads its integer as a value with n fractional bits, so
 * the result is w0 / 2^n; `fcvtzs w0, d0, #n` is the inverse, truncating
 * toward zero. Both are single correctly-rounded operations on hardware, and
 * gex implements them as a conversion plus an exact power-of-two scale, which
 * is the same answer everywhere except an underflow into subnormals.
 */
#include <stdio.h>
#include <stdint.h>

#define SCVTF_D(v, n) ({ double _r; int32_t _v = (v); \
    __asm__("scvtf %d0, %w1, #" #n : "=w"(_r) : "r"(_v)); _r; })
#define UCVTF_S(v, n) ({ float _r; uint32_t _v = (v); \
    __asm__("ucvtf %s0, %w1, #" #n : "=w"(_r) : "r"(_v)); _r; })
#define SCVTF_DX(v, n) ({ double _r; int64_t _v = (v); \
    __asm__("scvtf %d0, %x1, #" #n : "=w"(_r) : "r"(_v)); _r; })
#define FCVTZS_W(v, n) ({ int32_t _r; double _v = (v); \
    __asm__("fcvtzs %w0, %d1, #" #n : "=r"(_r) : "w"(_v)); _r; })
#define FCVTZU_X(v, n) ({ uint64_t _r; double _v = (v); \
    __asm__("fcvtzu %x0, %d1, #" #n : "=r"(_r) : "w"(_v)); _r; })
#define FCVTZS_W_S(v, n) ({ int32_t _r; float _v = (v); \
    __asm__("fcvtzs %w0, %s1, #" #n : "=r"(_r) : "w"(_v)); _r; })

int main(void) {
    static const int32_t xs[] = {0, 1, -1, 7, -7, 1000, -1000, 65536,
                                 2147483647, -2147483647 - 1};
    for (unsigned i = 0; i < sizeof xs / sizeof *xs; i++) {
        printf("scvtf.1 %.9f  scvtf.15 %.9f  scvtf.32 %.12f\n",
               SCVTF_D(xs[i], 1), SCVTF_D(xs[i], 15), SCVTF_D(xs[i], 32));
    }
    static const uint32_t us[] = {0, 1, 3, 4294967295u, 2147483648u, 12345};
    for (unsigned i = 0; i < sizeof us / sizeof *us; i++)
        printf("ucvtf.4 %.6f  ucvtf.31 %.9f\n", (double)UCVTF_S(us[i], 4),
               (double)UCVTF_S(us[i], 31));

    static const int64_t ls[] = {0, 1, -1, 1LL << 40, -(1LL << 40),
                                 9223372036854775807LL};
    for (unsigned i = 0; i < sizeof ls / sizeof *ls; i++)
        printf("scvtf64.8 %.9f  scvtf64.40 %.12f  scvtf64.64 %.17f\n",
               SCVTF_DX(ls[i], 8), SCVTF_DX(ls[i], 40), SCVTF_DX(ls[i], 64));

    static const double ds[] = {0.0, 1.0, -1.0, 0.5, -0.5, 3.75, -3.75,
                                1e9, -1e9, 1e-9, 0.9999999};
    for (unsigned i = 0; i < sizeof ds / sizeof *ds; i++)
        printf("fcvtzs.2 %d  fcvtzs.16 %d  fcvtzu.8 %llu\n",
               FCVTZS_W(ds[i], 2), FCVTZS_W(ds[i], 16),
               (unsigned long long)FCVTZU_X(ds[i] < 0 ? -ds[i] : ds[i], 8));

    /* Saturation: the result cannot be represented in the target width. */
    printf("sat %d %d\n", FCVTZS_W(1e18, 4), FCVTZS_W(-1e18, 4));
    printf("satf %d %d\n", FCVTZS_W_S(1e18f, 4), FCVTZS_W_S(-1e18f, 4));
    /* NaN converts to zero on AArch64. */
    printf("nan %d\n", FCVTZS_W(__builtin_nan(""), 3));
    return 0;
}
