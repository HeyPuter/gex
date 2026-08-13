/* Fixed-point conversions in the SIMD shift-by-immediate encoding (scvtf,
 * ucvtf, fcvtzs at #fbits): the lane carries a scale of 2^fbits. They share
 * the shift-immediate encoding with `shl`/`sshr`. Inline assembly, since no
 * intrinsic reaches the scalar forms. Values include negatives and a
 * saturating case, since `fcvtzs` clamps rather than wraps. */
#include <stdint.h>
#include <stdio.h>

int main(void) {
    /* scvtf s, s, #16: 0x00018000 is 1.5 with 16 fractional bits. */
    uint32_t in = 0x00018000, out = 0;
    __asm__ volatile("fmov s0, %w[i]\n\t"
                     "scvtf s0, s0, #16\n\t"
                     "fmov %w[o], s0\n\t"
                     : [o] "=r"(out) : [i] "r"(in) : "v0");
    float f;
    __builtin_memcpy(&f, &out, 4);
    printf("scvtf 1.5 -> %.6f\n", (double)f);

    /* ucvtf with a different scale, and a value with the top bit set: the
       unsigned reading is what distinguishes it from scvtf. */
    uint32_t u = 0x80000000, uo = 0;
    __asm__ volatile("fmov s1, %w[i]\n\t"
                     "ucvtf s1, s1, #2\n\t"
                     "fmov %w[o], s1\n\t"
                     : [o] "=r"(uo) : [i] "r"(u) : "v1");
    __builtin_memcpy(&f, &uo, 4);
    printf("ucvtf -> %.1f\n", (double)f);

    int32_t s = (int32_t)0x80000000, so = 0;
    __asm__ volatile("fmov s2, %w[i]\n\t"
                     "scvtf s2, s2, #2\n\t"
                     "fmov %w[o], s2\n\t"
                     : [o] "=r"(so) : [i] "r"(s) : "v2");
    __builtin_memcpy(&f, &so, 4);
    printf("scvtf signed -> %.1f\n", (double)f);

    /* fcvtzs v.4s, #16 over four floats, one of which overflows. */
    float src[4] = {1.5f, -2.25f, 0.0f, 1e9f};
    int32_t dst[4] = {0, 0, 0, 0};
    __asm__ volatile("ld1 {v3.4s}, [%[s]]\n\t"
                     "fcvtzs v3.4s, v3.4s, #16\n\t"
                     "st1 {v3.4s}, [%[d]]\n\t"
                     :: [s] "r"(src), [d] "r"(dst) : "v3", "memory");
    printf("fcvtzs.4s %d %d %d %d\n", dst[0], dst[1], dst[2], dst[3]);

    /* And the 64-bit lanes, which wasm has no vector conversion for. */
    double dsrc[2] = {3.25, -0.5};
    int64_t ddst[2] = {0, 0};
    __asm__ volatile("ld1 {v4.2d}, [%[s]]\n\t"
                     "fcvtzs v4.2d, v4.2d, #16\n\t"
                     "st1 {v4.2d}, [%[d]]\n\t"
                     :: [s] "r"(dsrc), [d] "r"(ddst) : "v4", "memory");
    printf("fcvtzs.2d %lld %lld\n", (long long)ddst[0], (long long)ddst[1]);

    /* The reverse direction on a vector. */
    int32_t isrc[4] = {65536, -32768, 1, 0};
    float fdst[4] = {0, 0, 0, 0};
    __asm__ volatile("ld1 {v5.4s}, [%[s]]\n\t"
                     "scvtf v5.4s, v5.4s, #16\n\t"
                     "st1 {v5.4s}, [%[d]]\n\t"
                     :: [s] "r"(isrc), [d] "r"(fdst) : "v5", "memory");
    printf("scvtf.4s %.4f %.4f %.4f %.4f\n", (double)fdst[0], (double)fdst[1],
           (double)fdst[2], (double)fdst[3]);
    return 0;
}
