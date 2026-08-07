/* `ld3`/`st3` and `ld4`/`st4`: three or four registers interleaved in memory.
 *
 *     ld4 {v4.16b-v7.16b}, [x4], #64
 *     st4 {v4.16b-v7.16b}, [x5], #64
 *     ld4 {v4.4s-v7.4s}, [x6]
 *
 * Memory holds lane 0 of every register, then lane 1 of every register, and
 * so on -- which is exactly how pixels are stored, so Mesa de-interleaves
 * RGBA with these. gex decoded `ld1` in one to four registers and `ld2`, but
 * not `ld3` or `ld4`: their opcodes were missing from the table, so the whole
 * instruction was unknown and the block ended in SIGILL.
 *
 * A single wasm shuffle reaches two vectors, so four sources need a tree of
 * them. The arrangements below are chosen to exercise every shape of that
 * tree: 16b needs four source vectors, 4s needs four, 8b needs two, and
 * `ld3` on 8b needs a trailing eight-byte access because 24 is not a whole
 * number of vectors.
 */
#include <arm_neon.h>
#include <stdio.h>
#include <stdint.h>

static void show(const char *tag, const uint8_t *p, int n) {
    printf("%s:", tag);
    for (int i = 0; i < n; i++) printf(" %d", p[i]);
    printf("\n");
}

int main(void) {
    uint8_t src[64], dst[64];
    for (int i = 0; i < 64; i++) src[i] = (uint8_t)(i + 1);

    /* ld4/st4 on bytes: four vectors in, four out. */
    {
        uint8x16x4_t v = vld4q_u8(src);
        show("ld4.16b r0", (const uint8_t *)&v.val[0], 16);
        show("ld4.16b r3", (const uint8_t *)&v.val[3], 16);
        for (int i = 0; i < 64; i++) dst[i] = 0;
        vst4q_u8(dst, v);
        show("st4.16b", dst, 16);
        int same = 1;
        for (int i = 0; i < 64; i++) if (dst[i] != src[i]) same = 0;
        printf("st4 round trip %s\n", same ? "ok" : "BROKEN");
    }

    /* ld4 on 32-bit lanes. */
    {
        uint32_t s32[16], d32[16];
        for (int i = 0; i < 16; i++) s32[i] = 0x1000 + i;
        uint32x4x4_t v = vld4q_u32(s32);
        printf("ld4.4s r0: %x %x %x %x\n", vgetq_lane_u32(v.val[0], 0),
               vgetq_lane_u32(v.val[0], 1), vgetq_lane_u32(v.val[0], 2),
               vgetq_lane_u32(v.val[0], 3));
        printf("ld4.4s r2: %x %x %x %x\n", vgetq_lane_u32(v.val[2], 0),
               vgetq_lane_u32(v.val[2], 1), vgetq_lane_u32(v.val[2], 2),
               vgetq_lane_u32(v.val[2], 3));
        vst4q_u32(d32, v);
        int same = 1;
        for (int i = 0; i < 16; i++) if (d32[i] != s32[i]) same = 0;
        printf("st4.4s round trip %s\n", same ? "ok" : "BROKEN");
    }

    /* ld3 on bytes, full width: 48 bytes, three source vectors. */
    {
        uint8x16x3_t v = vld3q_u8(src);
        show("ld3.16b r0", (const uint8_t *)&v.val[0], 16);
        show("ld3.16b r2", (const uint8_t *)&v.val[2], 16);
        for (int i = 0; i < 64; i++) dst[i] = 0;
        vst3q_u8(dst, v);
        int same = 1;
        for (int i = 0; i < 48; i++) if (dst[i] != src[i]) same = 0;
        printf("st3.16b round trip %s\n", same ? "ok" : "BROKEN");
    }

    /* The half-width forms: ld4 on 8b is 32 bytes, ld3 on 8b is 24 -- the
       one that does not divide into whole vectors. */
    {
        uint8x8x4_t v4 = vld4_u8(src);
        show("ld4.8b r0", (const uint8_t *)&v4.val[0], 8);
        show("ld4.8b r3", (const uint8_t *)&v4.val[3], 8);
        uint8x8x3_t v3 = vld3_u8(src);
        show("ld3.8b r0", (const uint8_t *)&v3.val[0], 8);
        show("ld3.8b r2", (const uint8_t *)&v3.val[2], 8);
        for (int i = 0; i < 64; i++) dst[i] = 0;
        vst3_u8(dst, v3);
        int same = 1;
        for (int i = 0; i < 24; i++) if (dst[i] != src[i]) same = 0;
        printf("st3.8b round trip %s\n", same ? "ok" : "BROKEN");
        /* Nothing beyond the block may be touched. */
        printf("st3.8b tail %d %d\n", dst[24], dst[25]);
    }
    return 0;
}
