/* `clz` on vector lanes, at every width AArch64 allows.
 *
 * wasm has no vector count-leading-zeros, so gex builds it: smear each set
 * bit down to the bottom of its lane, then count the ones. That turns a
 * leading-zero count into a population count, which wasm does have -- per
 * byte, so wider lanes need the pairwise adds on top.
 *
 * Zero is the case worth naming: it smears to zero, counts zero, and gives
 * the full lane width, which is what the hardware says.
 */
#include <arm_neon.h>
#include <stdio.h>
#include <stdint.h>

int main(void) {
    /* 32-bit lanes, including zero and the top bit. */
    uint32_t s32[4] = {0, 1, 0x80000000u, 0x00ffffffu};
    uint32x4_t v32 = vclzq_u32(vld1q_u32(s32));
    printf("clz.4s %u %u %u %u\n", vgetq_lane_u32(v32, 0), vgetq_lane_u32(v32, 1),
           vgetq_lane_u32(v32, 2), vgetq_lane_u32(v32, 3));

    /* The 64-bit-register form, which is what Mesa uses. */
    uint32_t s2[2] = {0x0000ffffu, 0};
    uint32x2_t v2 = vclz_u32(vld1_u32(s2));
    printf("clz.2s %u %u\n", vget_lane_u32(v2, 0), vget_lane_u32(v2, 1));

    /* 16-bit lanes. */
    uint16_t s16[8] = {0, 1, 2, 0x8000, 0x00ff, 0x0100, 0xffff, 0x4000};
    uint16x8_t v16 = vclzq_u16(vld1q_u16(s16));
    printf("clz.8h");
    uint16_t o16[8];
    vst1q_u16(o16, v16);
    for (int i = 0; i < 8; i++) printf(" %u", o16[i]);
    printf("\n");

    /* 8-bit lanes, where the population count needs no pairwise add. */
    uint8_t s8[16] = {0, 1, 2, 3, 4, 8, 16, 32, 64, 128, 255, 127, 63, 31, 15, 7};
    uint8x16_t v8 = vclzq_u8(vld1q_u8(s8));
    uint8_t o8[16];
    vst1q_u8(o8, v8);
    printf("clz.16b");
    for (int i = 0; i < 16; i++) printf(" %u", o8[i]);
    printf("\n");
    return 0;
}
