/* MMX: every integer op in register and memory forms, the shifts, the
 * shuffles, the SSE-era (pshufw, pextrw, pinsrw, pmovmskb, pavg*, pmin/max,
 * psadbw, pmulhuw, maskmovq) and SSSE3 (pshufb, palignr, phadd*, ...) forms,
 * movd/movq in every direction and the cvt*pi conversions, each on vectors
 * chosen to hit the saturation and sign edges. Inline assembly, not
 * <mmintrin.h>: compilers lower those to SSE2. Printed as hex, so a
 * difference names the op and the input. */
#include <stdint.h>
#include <stdio.h>
#include <string.h>

static uint64_t in[] = {
    0, ~0ull, 0x8000800080008000ull, 0x7fff7fff7fff7fffull,
    0x0123456789abcdefull, 0xfedcba9876543210ull, 0x80007fff00ff0100ull,
    0x00ff7f80ff017e81ull, 0x7f7f80808181fefeull, 0x0000000100000002ull,
};
#define N (sizeof in / sizeof in[0])

#define BIN(name)                                                              \
    static void t_##name(void) {                                               \
        for (unsigned i = 0; i < N; i++)                                       \
            for (unsigned j = 0; j < N; j++) {                                 \
                uint64_t r, m, a = in[i], b = in[j];                           \
                __asm__ volatile("movq %1, %%mm0\n movq %2, %%mm1\n"           \
                                 #name " %%mm1, %%mm0\n movq %%mm0, %0\n emms" \
                                 : "=m"(r) : "m"(a), "m"(b) : "mm0", "mm1");   \
                __asm__ volatile("movq %1, %%mm2\n" #name " %2, %%mm2\n"       \
                                 "movq %%mm2, %0\n emms"                       \
                                 : "=m"(m) : "m"(a), "m"(b) : "mm2");          \
                printf(#name " %u %u %016llx %016llx\n", i, j,                 \
                       (unsigned long long)r, (unsigned long long)m);          \
            }                                                                  \
    }
#define IMM(name, imm)                                                         \
    static void t_##name##_##imm(void) {                                       \
        for (unsigned i = 0; i < N; i++) {                                     \
            uint64_t r, a = in[i];                                             \
            __asm__ volatile("movq %1, %%mm3\n" #name " $" #imm ", %%mm3\n"    \
                             "movq %%mm3, %0\n emms"                           \
                             : "=m"(r) : "m"(a) : "mm3");                      \
            printf(#name " $" #imm " %u %016llx\n", i, (unsigned long long)r); \
        }                                                                      \
    }
#define IMM3(name, imm)                                                        \
    static void t3_##name##_##imm(void) {                                      \
        for (unsigned i = 0; i < N; i++)                                       \
            for (unsigned j = 0; j < N; j += 3) {                              \
                uint64_t r, a = in[i], b = in[j];                              \
                __asm__ volatile("movq %1, %%mm4\n movq %2, %%mm5\n"           \
                                 #name " $" #imm ", %%mm5, %%mm4\n"            \
                                 "movq %%mm4, %0\n emms"                       \
                                 : "=m"(r) : "m"(a), "m"(b) : "mm4", "mm5");   \
                printf(#name " $" #imm " %u %u %016llx\n", i, j,               \
                       (unsigned long long)r);                                 \
            }                                                                  \
    }

#define BINS(X) X(paddb) X(paddw) X(paddd) X(paddq) X(paddsb) X(paddsw)      \
    X(paddusb) X(paddusw) X(psubb) X(psubw) X(psubd) X(psubq) X(psubsb)        \
    X(psubsw) X(psubusb) X(psubusw) X(pmullw) X(pmulhw) X(pmulhuw)           \
    X(pmuludq) X(pmaddwd) X(pand) X(pandn) X(por) X(pxor) X(pcmpeqb)          \
    X(pcmpeqw) X(pcmpeqd) X(pcmpgtb) X(pcmpgtw) X(pcmpgtd) X(packsswb)        \
    X(packssdw) X(packuswb) X(punpcklbw) X(punpcklwd) X(punpckldq)            \
    X(punpckhbw) X(punpckhwd) X(punpckhdq) X(pavgb) X(pavgw) X(pminub)        \
    X(pmaxub) X(pminsw) X(pmaxsw) X(psadbw) X(psllw) X(pslld) X(psllq)        \
    X(psrlw) X(psrld) X(psrlq) X(psraw) X(psrad) X(pshufb) X(phaddw)          \
    X(phaddd) X(phaddsw) X(phsubw) X(phsubd) X(phsubsw) X(pmaddubsw)          \
    X(pmulhrsw) X(psignb) X(psignw) X(psignd) X(pabsb) X(pabsw) X(pabsd)
BINS(BIN)
IMM(psllw, 3) IMM(psllw, 16) IMM(pslld, 7) IMM(pslld, 40) IMM(psllq, 13)
IMM(psllq, 64) IMM(psrlw, 5) IMM(psrlw, 15) IMM(psrld, 31) IMM(psrlq, 33)
IMM(psraw, 4) IMM(psraw, 20) IMM(psrad, 9) IMM(psrad, 32)
IMM3(pshufw, 0x1b) IMM3(pshufw, 0xe4) IMM3(pshufw, 0x55)
IMM3(palignr, 0) IMM3(palignr, 3) IMM3(palignr, 8) IMM3(palignr, 11)
IMM3(palignr, 16)

static void t_misc(void) {
    for (unsigned i = 0; i < N; i++) {
        uint64_t a = in[i], r, x = 0x1111222233334444ull;
        uint32_t lo, w0, w3, mask;
        __asm__ volatile("movq %1, %%mm6\n movd %%mm6, %0\n emms" : "=r"(lo) : "m"(a) : "mm6");
        __asm__ volatile("movd %1, %%mm6\n movq %%mm6, %0\n emms" : "=m"(r) : "r"((uint32_t)(a >> 7)) : "mm6");
        printf("movd %u %08x %016llx\n", i, lo, (unsigned long long)r);
        __asm__ volatile("movq %2, %%mm7\n pextrw $0, %%mm7, %0\n pextrw $7, %%mm7, %1\n emms"
                         : "=r"(w0), "=r"(w3) : "m"(a) : "mm7");
        __asm__ volatile("movq %1, %%mm7\n pmovmskb %%mm7, %0\n emms" : "=r"(mask) : "m"(a) : "mm7");
        printf("pextrw/pmovmskb %u %x %x %x\n", i, w0, w3, mask);
        __asm__ volatile("movq %1, %%mm0\n pinsrw $2, %2, %%mm0\n pinsrw $5, %3, %%mm0\n"
                         "movq %%mm0, %0\n emms"
                         : "=m"(r) : "m"(x), "r"((uint32_t)a), "m"(*(uint16_t *)&in[(i + 1) % N]) : "mm0");
        printf("pinsrw %u %016llx\n", i, (unsigned long long)r);
        /* maskmovq: bytes of mm0 whose mm1 byte is negative, to [edi]. */
        unsigned char buf[16];
        memset(buf, 0xcc, sizeof buf);
        void *p = buf + 3;
        __asm__ volatile("movq %1, %%mm0\n movq %2, %%mm1\n maskmovq %%mm1, %%mm0\n emms"
                         : "+D"(p) : "m"(x), "m"(a) : "mm0", "mm1", "memory");
        printf("maskmovq %u", i);
        for (int k = 0; k < 16; k++) printf(" %02x", buf[k]);
        printf("\n");
        /* movq2dq / movdq2q, movntq. */
        uint64_t q[2], nt;
        __asm__ volatile("movq %2, %%mm2\n pcmpeqd %%xmm3, %%xmm3\n movq2dq %%mm2, %%xmm3\n"
                         "movdqu %%xmm3, %0\n movdq2q %%xmm3, %%mm4\n movntq %%mm4, %1\n emms"
                         : "=m"(q), "=m"(nt) : "m"(a) : "mm2", "mm4", "xmm3");
        printf("movq2dq %u %016llx %016llx %016llx\n", i, (unsigned long long)q[0],
               (unsigned long long)q[1], (unsigned long long)nt);
    }
}

static void t_cvt(void) {
    static const float f[][4] = {{1.5f, -2.5f, 9.f, 9.f}, {2.5f, 3.5f, 0, 0},
                                 {3e9f, -3e9f, 0, 0}, {-0.4f, 1e-3f, 0, 0}};
    static const double d[][2] __attribute__((aligned(16))) = {{1.5, -2.5}, {2.5, -3.5}, {3e9, -1e10}, {-0.6, 7.49}};
    static const int32_t s[][2] = {{1, -1}, {0x7fffffff, (int32_t)0x80000000}, {16777217, -3}};
    for (unsigned k = 0; k < 4; k++) {
        uint64_t a, b, c, e;
        __asm__ volatile("movups %4, %%xmm0\n cvtps2pi %%xmm0, %%mm0\n cvttps2pi %%xmm0, %%mm1\n"
                         "cvtpd2pi %5, %%mm2\n cvttpd2pi %5, %%mm3\n"
                         "movq %%mm0, %0\n movq %%mm1, %1\n movq %%mm2, %2\n movq %%mm3, %3\n emms"
                         : "=m"(a), "=m"(b), "=m"(c), "=m"(e) : "m"(f[k]), "m"(d[k])
                         : "xmm0", "mm0", "mm1", "mm2", "mm3");
        printf("cvt*2pi %u %016llx %016llx %016llx %016llx\n", k, (unsigned long long)a,
               (unsigned long long)b, (unsigned long long)c, (unsigned long long)e);
    }
    for (unsigned k = 0; k < 3; k++) {
        float o[4];
        double od[2];
        __asm__ volatile("movups %2, %%xmm1\n cvtpi2ps %3, %%xmm1\n movups %%xmm1, %0\n"
                         "movq %3, %%mm5\n cvtpi2pd %%mm5, %%xmm2\n movupd %%xmm2, %1\n emms"
                         : "=m"(o), "=m"(od) : "m"(f[0]), "m"(s[k]) : "xmm1", "xmm2", "mm5");
        printf("cvtpi2p* %u %a %a %a %a %a %a\n", k, o[0], o[1], o[2], o[3], od[0], od[1]);
    }
}

int main(void) {
#define CALL(name) t_##name();
    BINS(CALL)
    t_psllw_3(); t_psllw_16(); t_pslld_7(); t_pslld_40(); t_psllq_13(); t_psllq_64();
    t_psrlw_5(); t_psrlw_15(); t_psrld_31(); t_psrlq_33(); t_psraw_4(); t_psraw_20();
    t_psrad_9(); t_psrad_32();
    t3_pshufw_0x1b(); t3_pshufw_0xe4(); t3_pshufw_0x55();
    t3_palignr_0(); t3_palignr_3(); t3_palignr_8(); t3_palignr_11(); t3_palignr_16();
    t_misc();
    t_cvt();
    return 0;
}
