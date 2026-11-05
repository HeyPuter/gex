/* `saddlp`/`uaddlp` and the accumulating `sadalp`/`uadalp`: adjacent lanes
 * added into lanes of twice the width. The mnemonic's arrangement names the
 * destination (`uaddlp v4.4s, v4.8h` reads eight halfwords, writes four words).
 * The word-to-doubleword step has no wasm instruction and is built from
 * shuffles, so it gets its own case. */
#include <arm_neon.h>
#include <stdio.h>
#include <stdint.h>

int main(void) {
    uint8_t b[16];
    for (int i = 0; i < 16; i++) b[i] = (uint8_t)(i * 17);

    /* bytes -> halfwords, unsigned and signed. */
    uint16x8_t uh = vpaddlq_u8(vld1q_u8(b));
    uint16_t o[8];
    vst1q_u16(o, uh);
    printf("uaddlp.8h");
    for (int i = 0; i < 8; i++) printf(" %u", o[i]);
    printf("\n");

    int8_t sb[16];
    for (int i = 0; i < 16; i++) sb[i] = (int8_t)(i * 17 - 128);
    int16x8_t sh = vpaddlq_s8(vld1q_s8(sb));
    int16_t so[8];
    vst1q_s16(so, sh);
    printf("saddlp.8h");
    for (int i = 0; i < 8; i++) printf(" %d", so[i]);
    printf("\n");

    /* halfwords -> words. */
    uint32x4_t uw = vpaddlq_u16(uh);
    printf("uaddlp.4s %u %u %u %u\n", vgetq_lane_u32(uw, 0), vgetq_lane_u32(uw, 1),
           vgetq_lane_u32(uw, 2), vgetq_lane_u32(uw, 3));

    /* words -> doublewords, the step wasm has no instruction for. Values with
       the top bit set, so a sign-extension mistake would show. */
    uint32_t w[4] = {0xffffffffu, 1, 0x80000000u, 0x7fffffffu};
    uint64x2_t ud = vpaddlq_u32(vld1q_u32(w));
    printf("uaddlp.2d %llx %llx\n", (unsigned long long)vgetq_lane_u64(ud, 0),
           (unsigned long long)vgetq_lane_u64(ud, 1));

    /* The accumulating forms. */
    uint16x8_t acc = vdupq_n_u16(1000);
    acc = vpadalq_u8(acc, vld1q_u8(b));
    vst1q_u16(o, acc);
    printf("uadalp.8h");
    for (int i = 0; i < 8; i++) printf(" %u", o[i]);
    printf("\n");

    int16x8_t sacc = vdupq_n_s16(-100);
    sacc = vpadalq_s8(sacc, vld1q_s8(sb));
    vst1q_s16(so, sacc);
    printf("sadalp.8h");
    for (int i = 0; i < 8; i++) printf(" %d", so[i]);
    printf("\n");

    /* And the 64-bit-register arrangements. */
    uint16x4_t h4 = vpaddl_u8(vld1_u8(b));
    uint16_t o4[4];
    vst1_u16(o4, h4);
    printf("uaddlp.4h %u %u %u %u\n", o4[0], o4[1], o4[2], o4[3]);
    return 0;
}
