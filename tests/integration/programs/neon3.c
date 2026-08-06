/* The NEON instructions a glibc program reaches before `main`.
 *
 * glibc's aarch64 string routines are hand-written, and three of the things
 * they use were not implemented: `addhn`, which folds a comparison mask down
 * to a scalar in `strlen`; `umaxp`, which reduces one in `memchr`; and the
 * read-modify-write forms of the modified-immediate group, `bic vd, #imm`,
 * which `strchrnul` uses to mask a mask.
 *
 * The last of those was the interesting one. It shares its encoding with
 * `movi` and differs only in the low bit of `cmode`, so decoding it as a move
 * did not fail -- it silently replaced a register that was meant to be
 * masked. A wrong answer, not a trap.
 *
 * Intrinsics rather than inline assembly, so the compiler picks registers and
 * the comparison against qemu is of the whole sequence.
 */
#include <arm_neon.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

static unsigned hash(const void *p, unsigned n) {
    const unsigned char *b = p;
    unsigned h = 2166136261u;
    for (unsigned i = 0; i < n; i++) {
        h = (h ^ b[i]) * 16777619u;
    }
    return h;
}

int main(void) {
    unsigned char buf[64];
    for (int i = 0; i < 64; i++) {
        buf[i] = (unsigned char)(i * 7 + 3);
    }
    uint8x16_t a8 = vld1q_u8(buf);
    uint8x16_t b8 = vld1q_u8(buf + 16);
    uint16x8_t a16 = vld1q_u16((uint16_t *)buf);
    uint16x8_t b16 = vld1q_u16((uint16_t *)(buf + 16));
    uint32x4_t a32 = vld1q_u32((uint32_t *)buf);
    uint32x4_t b32 = vld1q_u32((uint32_t *)(buf + 16));

    /* addhn / addhn2 at each width, and their subtracting forms. */
    { uint8x8_t r = vaddhn_u16(a16, b16); printf("addhn_8b   %08x\n", hash(&r, 8)); }
    { uint16x4_t r = vaddhn_u32(a32, b32); printf("addhn_4h   %08x\n", hash(&r, 8)); }
    { uint32x2_t r = vaddhn_u64(vreinterpretq_u64_u8(a8), vreinterpretq_u64_u8(b8));
      printf("addhn_2s   %08x\n", hash(&r, 8)); }
    { uint8x16_t r = vaddhn_high_u16(vaddhn_u16(a16, b16), a16, b16);
      printf("addhn2_16b %08x\n", hash(&r, 16)); }
    { uint8x8_t r = vsubhn_u16(a16, b16); printf("subhn_8b   %08x\n", hash(&r, 8)); }
    { uint16x4_t r = vsubhn_u32(a32, b32); printf("subhn_4h   %08x\n", hash(&r, 8)); }
    { uint8x16_t r = vsubhn_high_u16(vsubhn_u16(a16, b16), a16, b16);
      printf("subhn2_16b %08x\n", hash(&r, 16)); }

    /* The rounding forms add half a result lane before narrowing. */
    { uint8x8_t r = vraddhn_u16(a16, b16); printf("raddhn_8b  %08x\n", hash(&r, 8)); }
    { uint16x4_t r = vraddhn_u32(a32, b32); printf("raddhn_4h  %08x\n", hash(&r, 8)); }
    { uint8x8_t r = vrsubhn_u16(a16, b16); printf("rsubhn_8b  %08x\n", hash(&r, 8)); }

    /* Pairwise maximum and minimum, both register sizes and every width. */
    { uint8x16_t r = vpmaxq_u8(a8, b8); printf("umaxp_16b  %08x\n", hash(&r, 16)); }
    { uint16x8_t r = vpmaxq_u16(a16, b16); printf("umaxp_8h   %08x\n", hash(&r, 16)); }
    { uint32x4_t r = vpmaxq_u32(a32, b32); printf("umaxp_4s   %08x\n", hash(&r, 16)); }
    { uint8x8_t r = vpmax_u8(vget_low_u8(a8), vget_low_u8(b8));
      printf("umaxp_8b   %08x\n", hash(&r, 8)); }
    { int8x16_t r = vpmaxq_s8(vreinterpretq_s8_u8(a8), vreinterpretq_s8_u8(b8));
      printf("smaxp_16b  %08x\n", hash(&r, 16)); }
    { uint8x16_t r = vpminq_u8(a8, b8); printf("uminp_16b  %08x\n", hash(&r, 16)); }
    { int16x8_t r = vpminq_s16(vreinterpretq_s16_u16(a16), vreinterpretq_s16_u16(b16));
      printf("sminp_8h   %08x\n", hash(&r, 16)); }

    /* `bic` and `orr` with a modified immediate: read-modify-write, at both
       lane widths and every shift the encoding allows. */
    { uint16x8_t r = a16; r = vbicq_u16(r, vdupq_n_u16(0x00f0));
      printf("bic_8h_f0  %08x\n", hash(&r, 16)); }
    { uint16x8_t r = a16; r = vbicq_u16(r, vdupq_n_u16(0xf000));
      printf("bic_8h_hi  %08x\n", hash(&r, 16)); }
    { uint32x4_t r = a32; r = vbicq_u32(r, vdupq_n_u32(0x0000ff00));
      printf("bic_4s_s8  %08x\n", hash(&r, 16)); }
    { uint32x4_t r = a32; r = vbicq_u32(r, vdupq_n_u32(0xff000000));
      printf("bic_4s_s24 %08x\n", hash(&r, 16)); }
    { uint16x8_t r = a16; r = vorrq_u16(r, vdupq_n_u16(0x0055));
      printf("orr_8h     %08x\n", hash(&r, 16)); }
    { uint32x4_t r = a32; r = vorrq_u32(r, vdupq_n_u32(0x00aa0000));
      printf("orr_4s     %08x\n", hash(&r, 16)); }
    /* The 64-bit forms, which also clear the top half of the register. */
    { uint16x4_t r = vget_low_u16(a16); r = vbic_u16(r, vdup_n_u16(0x00ff));
      printf("bic_4h     %08x\n", hash(&r, 8)); }
    { uint32x2_t r = vget_low_u32(a32); r = vorr_u32(r, vdup_n_u32(0x0000ff00));
      printf("orr_2s     %08x\n", hash(&r, 8)); }

    /* A right shift by the whole element width. Legal, and it means what it
       says: everything shifted out, so zero unsigned and the replicated sign
       bit signed. wasm has no such shift -- it takes the count modulo the
       lane width, so 32 on a 32-bit lane is a shift by nothing at all and
       hands back the operand. Quietly returning the input is the worst
       available answer and is what gex did until this was written. */
    { uint32x4_t r = a32;
      __asm__ volatile("ushr %0.4s, %1.4s, #32" : "=w"(r) : "w"(a32));
      printf("ushr_full  %08x\n", hash(&r, 16)); }
    { int32x4_t r;
      __asm__ volatile("sshr %0.4s, %1.4s, #32" : "=w"(r) : "w"(a32));
      printf("sshr_full  %08x\n", hash(&r, 16)); }
    { uint16x8_t r;
      __asm__ volatile("ushr %0.8h, %1.8h, #16" : "=w"(r) : "w"(a16));
      printf("ushr_full16 %08x\n", hash(&r, 16)); }
    { int16x8_t r;
      __asm__ volatile("sshr %0.8h, %1.8h, #16" : "=w"(r) : "w"(a16));
      printf("sshr_full16 %08x\n", hash(&r, 16)); }
    { uint8x16_t r;
      __asm__ volatile("ushr %0.16b, %1.16b, #8" : "=w"(r) : "w"(a8));
      printf("ushr_full8 %08x\n", hash(&r, 16)); }

    /* And the sequence glibc actually runs: find the first zero byte by
       comparing, folding with addhn, and reading the result as a scalar. */
    static const char *cases[] = {"", "a", "hello", "0123456789abcdef",
                                  "0123456789abcdefg", "a longer string still"};
    for (unsigned c = 0; c < sizeof cases / sizeof *cases; c++) {
        unsigned char pad[32] = {0};
        memcpy(pad, cases[c], strlen(cases[c]));
        uint8x16_t v = vld1q_u8(pad);
        uint8x16_t zero = vceqq_u8(v, vdupq_n_u8(0));
        uint8x8_t folded = vaddhn_u16(vreinterpretq_u16_u8(zero),
                                      vreinterpretq_u16_u8(zero));
        uint64_t mask;
        memcpy(&mask, &folded, 8);
        printf("strlen %2zu  mask=%016llx\n", strlen(cases[c]),
               (unsigned long long)mask);
    }
    return 0;
}
