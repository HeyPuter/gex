/* Single-lane load/store with a register post-index (`ld1 {v0.d}[0], [x6], x3`),
 * as Mesa emits for image rows. Inline assembly, since the intrinsics compile
 * to the immediate-offset form instead. The stride is deliberately wider than
 * the element, so advancing by the element size would still pass on a
 * contiguous array. */
#include <stdint.h>
#include <stdio.h>

int main(void) {
    static uint64_t src[12] = {
        10, 11, 12, 20, 21, 22, 30, 31, 32, 40, 41, 42,
    };
    static uint64_t dst[12];
    uint64_t stride = 3 * sizeof(uint64_t);

    const uint8_t *p = (const uint8_t *)src;
    uint8_t *q = (uint8_t *)dst;
    for (int i = 0; i < 4; i++) {
        __asm__ volatile(
            "ld1 {v0.d}[0], [%[p]], %[s]\n\t"
            "st1 {v0.d}[0], [%[q]], %[s]\n\t"
            : [p] "+r"(p), [q] "+r"(q)
            : [s] "r"(stride)
            : "v0", "memory");
    }
    for (int i = 0; i < 12; i++) printf("%llu ", (unsigned long long)dst[i]);
    printf("\n");
    /* The pointers must have advanced by four strides, not by four elements. */
    printf("advanced %ld %ld\n", (long)(p - (const uint8_t *)src),
           (long)(q - (uint8_t *)dst));

    /* A byte lane, which the encoding indexes differently, and a 32-bit one. */
    static const uint8_t bytes[16] = {1, 2, 3, 4, 5, 6, 7, 8,
                                      9, 10, 11, 12, 13, 14, 15, 16};
    const uint8_t *bp = bytes;
    uint64_t two = 2;
    uint64_t lane5 = 0, word = 0;
    __asm__ volatile(
        "movi v1.16b, #0\n\t"
        "ld1 {v1.b}[5], [%[bp]], %[two]\n\t"
        "umov %w[out], v1.b[5]\n\t"
        : [out] "=r"(lane5), [bp] "+r"(bp)
        : [two] "r"(two)
        : "v1", "memory");
    __asm__ volatile(
        "movi v2.16b, #0\n\t"
        "ld1 {v2.s}[2], [%[bp]], %[two]\n\t"
        "umov %w[out], v2.s[2]\n\t"
        : [out] "=r"(word), [bp] "+r"(bp)
        : [two] "r"(two)
        : "v2", "memory");
    printf("byte lane 5 = %llu, word lane 2 = %#llx, offset %ld\n",
           (unsigned long long)lane5, (unsigned long long)word,
           (long)(bp - bytes));
    return 0;
}
