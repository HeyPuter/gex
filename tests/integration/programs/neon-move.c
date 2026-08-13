/* Moving lanes between the vector and general register files: smov/umov/ins,
 * and the rev16/rev32/rev64 reverse family. Inline assembly, to reach each
 * encoding on purpose rather than let the compiler choose. Only the *valid*
 * reverse combinations are listed (an element as wide as its group is a
 * reserved encoding), to confirm rejecting the reserved ones spared the real
 * ones. */
#include <arm_neon.h>
#include <stdint.h>
#include <stdio.h>

int main(void) {
    unsigned char buf[16];
    for (int i = 0; i < 16; i++) {
        buf[i] = (unsigned char)(i * 29 + 137);
    }
    uint8x16_t v = vld1q_u8(buf);
    uint64_t x;
    uint32_t w;

    /* smov into a 64-bit register: the case that failed to validate. Every
       lane named here has its top bit set, which is the whole point. */
    __asm__("smov %0, %1.b[3]" : "=r"(x) : "w"(v));
    printf("smov_x_b   %016llx\n", (unsigned long long)x);
    __asm__("smov %0, %1.h[5]" : "=r"(x) : "w"(v));
    printf("smov_x_h   %016llx\n", (unsigned long long)x);
    __asm__("smov %0, %1.s[2]" : "=r"(x) : "w"(v));
    printf("smov_x_s   %016llx\n", (unsigned long long)x);

    /* And into a 32-bit one, which was always fine. */
    __asm__("smov %w0, %1.b[7]" : "=r"(w) : "w"(v));
    printf("smov_w_b   %08x\n", w);
    __asm__("smov %w0, %1.h[1]" : "=r"(w) : "w"(v));
    printf("smov_w_h   %08x\n", w);

    /* umov, which zero-extends, at every width the encoding allows. */
    __asm__("umov %w0, %1.b[11]" : "=r"(w) : "w"(v));
    printf("umov_w_b   %08x\n", w);
    __asm__("umov %w0, %1.h[6]" : "=r"(w) : "w"(v));
    printf("umov_w_h   %08x\n", w);
    __asm__("umov %w0, %1.s[1]" : "=r"(w) : "w"(v));
    printf("umov_w_s   %08x\n", w);
    __asm__("umov %0, %1.d[1]" : "=r"(x) : "w"(v));
    printf("umov_x_d   %016llx\n", (unsigned long long)x);

    /* Lane zero of each width, since a zero index is its own encoding path. */
    __asm__("smov %0, %1.b[0]" : "=r"(x) : "w"(v));
    printf("smov_x_b0  %016llx\n", (unsigned long long)x);
    __asm__("umov %0, %1.d[0]" : "=r"(x) : "w"(v));
    printf("umov_x_d0  %016llx\n", (unsigned long long)x);

    /* The other direction: a general register into a lane, then read back as
       two doublewords so a wrong lane shows up. */
    uint8x16_t z = v;
    uint64_t hi = 0xfedcba9876543210ULL;
    __asm__("ins %0.b[4], %w1" : "+w"(z) : "r"((uint32_t)0x5a));
    __asm__("ins %0.h[2], %w1" : "+w"(z) : "r"((uint32_t)0xbeef));
    __asm__("ins %0.s[3], %w1" : "+w"(z) : "r"((uint32_t)0xdeadbeef));
    __asm__("ins %0.d[0], %1" : "+w"(z) : "r"(hi));
    printf("ins_lo     %016llx\n",
           (unsigned long long)vgetq_lane_u64(vreinterpretq_u64_u8(z), 0));
    printf("ins_hi     %016llx\n",
           (unsigned long long)vgetq_lane_u64(vreinterpretq_u64_u8(z), 1));

    /* Element-to-element insert, which moves a lane without going through a
       general register at all. */
    uint8x16_t y = vdupq_n_u8(0);
    __asm__("ins %0.h[3], %1.h[6]" : "+w"(y) : "w"(v));
    __asm__("ins %0.d[1], %1.d[0]" : "+w"(y) : "w"(v));
    printf("ins_elt_lo %016llx\n",
           (unsigned long long)vgetq_lane_u64(vreinterpretq_u64_u8(y), 0));
    printf("ins_elt_hi %016llx\n",
           (unsigned long long)vgetq_lane_u64(vreinterpretq_u64_u8(y), 1));

    /* Every element/group combination the reverse family actually allows. */
#define REV(name, insn) do { uint8x16_t r_ = v; \
        __asm__(insn : "+w"(r_)); \
        printf("%-10s %016llx %016llx\n", name, \
               (unsigned long long)vgetq_lane_u64(vreinterpretq_u64_u8(r_), 0), \
               (unsigned long long)vgetq_lane_u64(vreinterpretq_u64_u8(r_), 1)); \
    } while (0)
    REV("rev16_16b", "rev16 %0.16b, %0.16b");
    REV("rev16_8b",  "rev16 %0.8b, %0.8b");
    REV("rev32_16b", "rev32 %0.16b, %0.16b");
    REV("rev32_8b",  "rev32 %0.8b, %0.8b");
    REV("rev32_8h",  "rev32 %0.8h, %0.8h");
    REV("rev32_4h",  "rev32 %0.4h, %0.4h");
    REV("rev64_16b", "rev64 %0.16b, %0.16b");
    REV("rev64_8b",  "rev64 %0.8b, %0.8b");
    REV("rev64_8h",  "rev64 %0.8h, %0.8h");
    REV("rev64_4h",  "rev64 %0.4h, %0.4h");
    REV("rev64_4s",  "rev64 %0.4s, %0.4s");
    REV("rev64_2s",  "rev64 %0.2s, %0.2s");
    return 0;
}
