/* Vector `mul`, `mla`, `mls` at every arrangement, and vector `rbit`.
 *
 * wasm has no byte multiply and no bit reverse, so gex builds both: byte
 * lanes multiply as halfwords with the odd bytes shifted into place, and
 * `rbit` looks each nibble up reversed. clang's auto-vectoriser emits both
 * (`mul v.8b` from a byte loop, `rbit v.16b` from a bit-reversal one).
 *
 * Each result is checked against a scalar reference written from the Arm ARM
 * pseudocode, so the program is its own oracle: it prints every mismatch and
 * the number of them, and the output must match QEMU's as well.
 */
#include <arm_neon.h>
#include <stdint.h>
#include <stdio.h>

static int bad;

/* Hide an array's contents from the optimiser, so the intrinsics below are
   executed rather than folded at compile time. */
#define OPAQUE(p) __asm__ volatile("" : : "r"(p) : "memory")

static uint8_t rbit8(uint8_t v) {
    uint8_t r = 0;
    for (int i = 0; i < 8; i++)
        if (v & (1u << i)) r |= (uint8_t)(0x80u >> i);
    return r;
}

#define CHECK(name, got, want, n, fmt) do { \
    for (int i_ = 0; i_ < (n); i_++) \
        if ((got)[i_] != (want)[i_]) { \
            printf("%s lane %d: got " fmt " want " fmt "\n", name, i_, (got)[i_], (want)[i_]); \
            bad++; \
        } \
} while (0)

int main(void) {
    /* Inputs with every byte extreme: 0, 1, 0x7f, 0x80, 0xff and the rest. */
    uint8_t a[16], b[16], c[16];
    for (int i = 0; i < 16; i++) {
        a[i] = (uint8_t)(i * 37 + 3);
        b[i] = (uint8_t)(0xff - i * 19);
        c[i] = (uint8_t)(i * 91 + 128);
    }
    a[0] = 0; a[1] = 0xff; a[2] = 0x80; a[3] = 0x7f; b[4] = 0x80; b[5] = 0xff; a[5] = 0xff;
    OPAQUE(a); OPAQUE(b); OPAQUE(c);

    /* 8-bit lanes: mul, mla, mls, both widths. */
    {
        uint8_t got[16], want[16];
        vst1q_u8(got, vmulq_u8(vld1q_u8(a), vld1q_u8(b)));
        for (int i = 0; i < 16; i++) want[i] = (uint8_t)(a[i] * b[i]);
        CHECK("mul.16b", got, want, 16, "%u");
        vst1_u8(got, vmul_u8(vld1_u8(a + 8), vld1_u8(b)));
        for (int i = 0; i < 8; i++) want[i] = (uint8_t)(a[i + 8] * b[i]);
        CHECK("mul.8b", got, want, 8, "%u");
        vst1q_u8(got, vmlaq_u8(vld1q_u8(c), vld1q_u8(a), vld1q_u8(b)));
        for (int i = 0; i < 16; i++) want[i] = (uint8_t)(c[i] + a[i] * b[i]);
        CHECK("mla.16b", got, want, 16, "%u");
        vst1_u8(got, vmls_u8(vld1_u8(c), vld1_u8(a), vld1_u8(b + 8)));
        for (int i = 0; i < 8; i++) want[i] = (uint8_t)(c[i] - a[i] * b[i + 8]);
        CHECK("mls.8b", got, want, 8, "%u");
        /* Signed lanes are the same bits. */
        int8_t sgot[16], swant[16];
        vst1q_s8(sgot, vmulq_s8(vld1q_s8((int8_t *)a), vld1q_s8((int8_t *)c)));
        for (int i = 0; i < 16; i++) swant[i] = (int8_t)((int8_t)a[i] * (int8_t)c[i]);
        CHECK("mul.16b signed", sgot, swant, 16, "%d");
    }

    /* 16-bit lanes. */
    {
        uint16_t x[8], y[8], z[8], got[8], want[8];
        for (int i = 0; i < 8; i++) {
            x[i] = (uint16_t)(i * 9001 + 7);
            y[i] = (uint16_t)(0xffff - i * 4099);
            z[i] = (uint16_t)(i * 30011);
        }
        x[0] = 0x8000; y[0] = 0xffff; x[1] = 0x7fff; y[1] = 0x7fff;
        OPAQUE(x); OPAQUE(y); OPAQUE(z);
        vst1q_u16(got, vmulq_u16(vld1q_u16(x), vld1q_u16(y)));
        for (int i = 0; i < 8; i++) want[i] = (uint16_t)(x[i] * y[i]);
        CHECK("mul.8h", got, want, 8, "%u");
        vst1_u16(got, vmul_u16(vld1_u16(x + 4), vld1_u16(y)));
        for (int i = 0; i < 4; i++) want[i] = (uint16_t)(x[i + 4] * y[i]);
        CHECK("mul.4h", got, want, 4, "%u");
        vst1q_u16(got, vmlaq_u16(vld1q_u16(z), vld1q_u16(x), vld1q_u16(y)));
        for (int i = 0; i < 8; i++) want[i] = (uint16_t)(z[i] + x[i] * y[i]);
        CHECK("mla.8h", got, want, 8, "%u");
    }

    /* 32-bit lanes. */
    {
        uint32_t x[4] = {0x80000000u, 0xffffffffu, 0x12345678u, 7};
        uint32_t y[4] = {3, 0xffffffffu, 0x9abcdef1u, 0x7fffffffu};
        uint32_t got[4], want[4];
        OPAQUE(x); OPAQUE(y);
        vst1q_u32(got, vmulq_u32(vld1q_u32(x), vld1q_u32(y)));
        for (int i = 0; i < 4; i++) want[i] = x[i] * y[i];
        CHECK("mul.4s", got, want, 4, "%u");
        vst1_u32(got, vmul_u32(vld1_u32(x + 2), vld1_u32(y)));
        for (int i = 0; i < 2; i++) want[i] = x[i + 2] * y[i];
        CHECK("mul.2s", got, want, 2, "%u");
        vst1q_u32(got, vmlsq_u32(vld1q_u32(y), vld1q_u32(x), vld1q_u32(x)));
        for (int i = 0; i < 4; i++) want[i] = y[i] - x[i] * x[i];
        CHECK("mls.4s", got, want, 4, "%u");
    }

    /* rbit on every byte value, both widths. */
    {
        uint8_t in[256], got[16], want[16];
        for (int i = 0; i < 256; i++) in[i] = (uint8_t)i;
        OPAQUE(in);
        for (int base = 0; base < 256; base += 16) {
            vst1q_u8(got, vrbitq_u8(vld1q_u8(in + base)));
            for (int i = 0; i < 16; i++) want[i] = rbit8(in[base + i]);
            CHECK("rbit.16b", got, want, 16, "%u");
        }
        uint8_t hi[16];
        for (int i = 0; i < 16; i++) hi[i] = 0xaa;
        vst1q_u8(hi, vcombine_u8(vrbit_u8(vld1_u8(in + 0x31)), vdup_n_u8(0xaa)));
        for (int i = 0; i < 8; i++) want[i] = rbit8(in[0x31 + i]);
        CHECK("rbit.8b", hi, want, 8, "%u");
    }

    /* The shape clang auto-vectorises into `mul v.16b` and `rbit v.16b`. */
    {
        static uint8_t p[1024], q[1024], r[1024], s[1024];
        for (int i = 0; i < 1024; i++) { p[i] = (uint8_t)(i * 7); q[i] = (uint8_t)(i * 13 + 1); }
        OPAQUE(p); OPAQUE(q);
        for (int i = 0; i < 1024; i++) r[i] = (uint8_t)(p[i] * q[i]);
#if defined(__clang__)
        for (int i = 0; i < 1024; i++) s[i] = (uint8_t)__builtin_bitreverse8(p[i]);
#else
        for (int i = 0; i < 1024; i++) s[i] = rbit8(p[i]);
#endif
        unsigned h = 0;
        for (int i = 0; i < 1024; i++) {
            if (r[i] != (uint8_t)((i * 7 & 0xff) * ((i * 13 + 1) & 0xff))) bad++;
            if (s[i] != rbit8((uint8_t)(i * 7))) bad++;
            h = h * 31 + r[i] + s[i];
        }
        printf("loop hash %08x\n", h);
    }

    printf("%d mismatches\n", bad);
    return bad != 0;
}
