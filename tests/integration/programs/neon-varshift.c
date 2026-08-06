#include <arm_neon.h>
#include <stdint.h>
#include <stdio.h>
/* The register-variable lane shifts. The count is the low byte of each lane
   of the second operand, signed: negative shifts right, and a count at or
   past the lane width shifts everything out. */
static unsigned hash(const void *p, unsigned n) {
    const unsigned char *b = p; unsigned h = 2166136261u;
    for (unsigned i = 0; i < n; i++) h = (h ^ b[i]) * 16777619u;
    return h;
}
#define SHOW(name, expr, bytes) do { typeof(expr) r_ = (expr); \
    printf("%-14s %08x\n", name, hash(&r_, bytes)); } while (0)
int main(void) {
    unsigned char buf[32];
    for (int i = 0; i < 32; i++) buf[i] = (unsigned char)(i * 11 + 5);
    uint8x16_t a8 = vld1q_u8(buf);
    uint16x8_t a16 = vld1q_u16((uint16_t *)buf);
    uint32x4_t a32 = vld1q_u32((uint32_t *)buf);
    uint64x2_t a64 = vld1q_u64((uint64_t *)buf);

    /* Counts covering every case: zero, in range each way, exactly the lane
       width, and well past it. */
    int8_t c8v[16]  = {0,1,7,8,-1,-7,-8,-9,3,-3,15,-15,127,-128,4,-4};
    int16_t c16v[8] = {0,1,15,16,-1,-15,-16,-17};
    int32_t c32v[4] = {0,31,-31,-32};
    int64_t c64v[2] = {0,-64};
    int8x16_t c8 = vld1q_s8(c8v);
    int16x8_t c16 = vld1q_s16(c16v);
    int32x4_t c32 = vld1q_s32(c32v);
    int64x2_t c64 = vld1q_s64(c64v);

    SHOW("ushl_16b", vshlq_u8(a8, c8), 16);
    SHOW("sshl_16b", vshlq_s8(vreinterpretq_s8_u8(a8), c8), 16);
    SHOW("ushl_8h",  vshlq_u16(a16, c16), 16);
    SHOW("sshl_8h",  vshlq_s16(vreinterpretq_s16_u16(a16), c16), 16);
    SHOW("ushl_4s",  vshlq_u32(a32, c32), 16);
    SHOW("sshl_4s",  vshlq_s32(vreinterpretq_s32_u32(a32), c32), 16);
    SHOW("ushl_2d",  vshlq_u64(a64, c64), 16);
    SHOW("sshl_2d",  vshlq_s64(vreinterpretq_s64_u64(a64), c64), 16);
    /* The 64-bit forms, which clear the register's upper half. */
    SHOW("ushl_8b",  vshl_u8(vget_low_u8(a8), vget_low_s8(c8)), 8);
    SHOW("sshl_4h",  vshl_s16(vget_low_s16(vreinterpretq_s16_u16(a16)), vget_low_s16(c16)), 8);
    SHOW("ushl_2s",  vshl_u32(vget_low_u32(a32), vget_low_s32(c32)), 8);
    /* And the scalar form, on lane 0 alone. */
    SHOW("ushl_d",   vshl_u64(vget_low_u64(a64), vget_low_s64(c64)), 8);
    SHOW("sshl_d",   vshl_s64(vget_low_s64(vreinterpretq_s64_u64(a64)), vget_low_s64(c64)), 8);
    /* Shift and insert, both directions and every width. The bits the shift
       vacates come from the destination, so both registers matter. */
    uint8x16_t d8 = vld1q_u8(buf + 16);
    uint16x8_t d16 = vld1q_u16((uint16_t *)(buf + 16));
    uint32x4_t d32 = vld1q_u32((uint32_t *)(buf + 16));
    uint64x2_t d64 = vld1q_u64((uint64_t *)(buf + 16));
    SHOW("sli_16b", vsliq_n_u8(d8, a8, 3), 16);
    SHOW("sli_8h",  vsliq_n_u16(d16, a16, 9), 16);
    SHOW("sli_4s",  vsliq_n_u32(d32, a32, 25), 16);
    SHOW("sli_2d",  vsliq_n_u64(d64, a64, 63), 16);
    SHOW("sli_0",   vsliq_n_u32(d32, a32, 0), 16);
    SHOW("sri_16b", vsriq_n_u8(d8, a8, 3), 16);
    SHOW("sri_8h",  vsriq_n_u16(d16, a16, 9), 16);
    SHOW("sri_4s",  vsriq_n_u32(d32, a32, 25), 16);
    SHOW("sri_2d",  vsriq_n_u64(d64, a64, 64), 16);
    SHOW("sri_8b",  vsri_n_u8(vget_low_u8(d8), vget_low_u8(a8), 1), 8);

    /* Shift right and accumulate, including the full-width count that only a
       right shift may use. */
    SHOW("usra_16b", vsraq_n_u8(d8, a8, 3), 16);
    SHOW("ssra_8h",  vsraq_n_s16(vreinterpretq_s16_u16(d16), vreinterpretq_s16_u16(a16), 9), 16);
    SHOW("usra_4s",  vsraq_n_u32(d32, a32, 32), 16);
    SHOW("ssra_4s",  vsraq_n_s32(vreinterpretq_s32_u32(d32), vreinterpretq_s32_u32(a32), 32), 16);
    SHOW("usra_2d",  vsraq_n_u64(d64, a64, 1), 16);
    SHOW("ssra_2s",  vsra_n_s32(vget_low_s32(vreinterpretq_s32_u32(d32)),
                                vget_low_s32(vreinterpretq_s32_u32(a32)), 7), 8);
    return 0;
}
