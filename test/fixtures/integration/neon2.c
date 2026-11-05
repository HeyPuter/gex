// The structure loads and lane accesses, driven through ACLE intrinsics so the
// encodings are chosen deliberately rather than left to the vectoriser.
#include <arm_neon.h>
#include <stdio.h>

static unsigned char buf[256];
static unsigned hash(const void *p, int n) {
    const unsigned char *b = p; unsigned h = 2166136261u;
    for (int i = 0; i < n; i++) { h ^= b[i]; h *= 16777619u; }
    return h;
}

int main(void) {
    for (int i = 0; i < 256; i++) buf[i] = (unsigned char)(i * 7 + 3);

    // ld1r across every width.
    { uint8x16_t r = vld1q_dup_u8(buf + 5); printf("ld1r8  %08x\n", hash(&r, 16)); }
    { uint16x8_t r = vld1q_dup_u16((uint16_t *)(buf + 8)); printf("ld1r16 %08x\n", hash(&r, 16)); }
    { uint32x4_t r = vld1q_dup_u32((uint32_t *)(buf + 12)); printf("ld1r32 %08x\n", hash(&r, 16)); }
    { uint64x2_t r = vld1q_dup_u64((uint64_t *)(buf + 16)); printf("ld1r64 %08x\n", hash(&r, 16)); }
    { uint8x8_t r = vld1_dup_u8(buf + 21); printf("ld1r8d %08x\n", hash(&r, 8)); }

    // Single-lane load and store.
    uint32x4_t v = vdupq_n_u32(0xaaaaaaaa);
    v = vld1q_lane_u32((uint32_t *)(buf + 20), v, 2);
    printf("lane32 %08x\n", hash(&v, 16));
    uint8x16_t w = vdupq_n_u8(0x11);
    w = vld1q_lane_u8(buf + 33, w, 9);
    printf("lane8  %08x\n", hash(&w, 16));
    uint64x2_t x = vdupq_n_u64(0);
    x = vld1q_lane_u64((uint64_t *)(buf + 40), x, 1);
    printf("lane64 %08x\n", hash(&x, 16));
    unsigned char out[32] = {0};
    vst1q_lane_u32((uint32_t *)(out + 4), v, 1);
    vst1q_lane_u8(out + 12, w, 9);
    vst1q_lane_u64((uint64_t *)(out + 16), x, 1);
    printf("stlane %08x\n", hash(out, 32));

    // ld2/st2 at every element width, both register sizes.
    { uint8x16x2_t p = vld2q_u8(buf); printf("ld2_16b %08x %08x\n", hash(&p.val[0],16), hash(&p.val[1],16)); 
      unsigned char o[32]; vst2q_u8(o, p); printf("st2_16b %08x\n", hash(o, 32)); }
    { uint16x8x2_t p = vld2q_u16((uint16_t *)buf); printf("ld2_8h  %08x %08x\n", hash(&p.val[0],16), hash(&p.val[1],16));
      unsigned short o[16]; vst2q_u16(o, p); printf("st2_8h  %08x\n", hash(o, 32)); }
    { uint32x4x2_t p = vld2q_u32((uint32_t *)buf); printf("ld2_4s  %08x %08x\n", hash(&p.val[0],16), hash(&p.val[1],16));
      unsigned o[8]; vst2q_u32(o, p); printf("st2_4s  %08x\n", hash(o, 32)); }
    { uint64x2x2_t p = vld2q_u64((uint64_t *)buf); printf("ld2_2d  %08x %08x\n", hash(&p.val[0],16), hash(&p.val[1],16));
      uint64_t o[4]; vst2q_u64(o, p); printf("st2_2d  %08x\n", hash(o, 32)); }
    { uint8x8x2_t p = vld2_u8(buf); printf("ld2_8b  %08x %08x\n", hash(&p.val[0],8), hash(&p.val[1],8));
      unsigned char o[16]; vst2_u8(o, p); printf("st2_8b  %08x\n", hash(o, 16)); }
    { uint32x2x2_t p = vld2_u32((uint32_t *)buf); printf("ld2_2s  %08x %08x\n", hash(&p.val[0],8), hash(&p.val[1],8));
      unsigned o[4]; vst2_u32(o, p); printf("st2_2s  %08x\n", hash(o, 16)); }
    return 0;
}
