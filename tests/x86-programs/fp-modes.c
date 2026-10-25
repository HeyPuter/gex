/* The rounding controls: SSE arithmetic and conversions under each MXCSR
 * rounding mode (gex honours MXCSR's rounding control, not FTZ or DAZ), and
 * x87 arithmetic and stores under each rounding and precision control of
 * the control word.
 * Each line is one operation in one mode, a hash of its results over every
 * pair of a set of operands (denormals, values a rounding mode moves, ones
 * that overflow or underflow), compared with the hardware's.
 *
 * The modes are set with ldmxcsr / fldcw and left set across the operations,
 * which run on operands loaded after the mode is set (volatile loads behind
 * an asm with a memory clobber), so the compiler neither folds nor hoists
 * them. The SSE part is x86-64's; the x87 part runs on both. */
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#if defined(__x86_64__)
#include <immintrin.h>
#endif

static uint64_t h;
static void mix(uint64_t v) {
    for (int i = 0; i < 8; i++) h = (h ^ ((v >> (8 * i)) & 0xff)) * 0x100000001b3ull;
}
static uint64_t dbits(double d) { uint64_t u; memcpy(&u, &d, 8); return u; }
static uint32_t fbits(float f) { uint32_t u; memcpy(&u, &f, 4); return u; }

static volatile double D[] = {
    1.0, 1.0 / 3.0, -2.0 / 7.0, 0.1, -1e-300, 1e300, -1.7976931348623157e308,
    2.2250738585072014e-308, 1.5 * 2.2250738585072014e-308, -4.9406564584124654e-324,
    1.1125369292536007e-308, 3.0e-310, 1e-160, -7.5, 1e16 + 3.0, 0.0, 1e-39, -3.5e-41,
    /* x * 2^-1022 is tiny for the first (it rounds to the smallest normal
     * only in the denormals), and the next two's product is not. */
    0.99999999999999989, 1.0000000000000002, 2.2250738585072009e-308,
};
static volatile float F[] = {
    1.0f, 1.0f / 3.0f, -2.0f / 7.0f, 0.1f, -1e-30f, 1e30f, -3.4028235e38f,
    1.17549435e-38f, 1.5f * 1.17549435e-38f, -1.4e-45f, 5.9e-39f, 3.0e-40f, 1e-20f, -7.5f,
    16777217.0f, 0.0f, 0.99999994f, 1.00000012f, 1.17549421e-38f,
};
static volatile int64_t I[] = {
    1, -3, 16777217, -16777219, 9007199254740993ll, -9007199254740995ll,
    0x7fffffffffffffffll, 123456789123ll, 2147483647, -2147483647 - 1, 0, 33554435,
};
#define ND (int)(sizeof D / sizeof D[0])
#define NF (int)(sizeof F / sizeof F[0])
#define NI (int)(sizeof I / sizeof I[0])

#define FENCE() __asm__ volatile("" ::: "memory")

#if defined(__x86_64__)
static void setmx(uint32_t v) { __asm__ volatile("ldmxcsr %0" : : "m"(v) : "memory"); }

#define LINE(name, ...) do { h = 0xcbf29ce484222325ull; __VA_ARGS__; printf("mx %04x %-10s %016llx\n", mode, name, (unsigned long long)h); } while (0)
#define PAIRS_D(expr) for (int i = 0; i < ND; i++) for (int j = 0; j < ND; j++) { FENCE(); double a = D[i], b = D[j]; (void)b; __m128d A = _mm_set_sd(a), B = _mm_set_sd(b); (void)A; (void)B; mix(expr); }
#define PAIRS_F(expr) for (int i = 0; i < NF; i++) for (int j = 0; j < NF; j++) { FENCE(); float a = F[i], b = F[j]; (void)b; __m128 A = _mm_set_ss(a), B = _mm_set_ss(b); (void)A; (void)B; mix(expr); }
#define EACH_D(expr) for (int i = 0; i < ND; i++) { FENCE(); double a = D[i]; __m128d A = _mm_set_sd(a); (void)A; mix(expr); }
#define EACH_F(expr) for (int i = 0; i < NF; i++) { FENCE(); float a = F[i]; __m128 A = _mm_set_ss(a); (void)A; mix(expr); }
#define EACH_I(expr) for (int i = 0; i < NI; i++) { FENCE(); int64_t n = I[i]; (void)n; mix(expr); }
/* Packed: four (two) lanes from consecutive operands. */
#define PACK_F(k) _mm_set_ps(F[(k) % NF], F[((k) + 3) % NF], F[((k) + 7) % NF], F[((k) + 11) % NF])
#define PACK_D(k) _mm_set_pd(D[(k) % ND], D[((k) + 5) % ND])
#define PAIRS_PF(expr) for (int i = 0; i < NF; i++) for (int j = 0; j < NF; j++) { FENCE(); __m128 A = PACK_F(i), B = PACK_F(j); __m128i r = _mm_castps_si128(expr); mix((uint64_t)_mm_cvtsi128_si64(r)); mix((uint64_t)_mm_cvtsi128_si64(_mm_unpackhi_epi64(r, r))); }
#define PAIRS_PD(expr) for (int i = 0; i < ND; i++) for (int j = 0; j < ND; j++) { FENCE(); __m128d A = PACK_D(i), B = PACK_D(j); __m128i r = _mm_castpd_si128(expr); mix((uint64_t)_mm_cvtsi128_si64(r)); mix((uint64_t)_mm_cvtsi128_si64(_mm_unpackhi_epi64(r, r))); }
#define EACH_PI(expr) for (int i = 0; i < NF; i++) { FENCE(); __m128 A = PACK_F(i); __m128d AD = PACK_D(i); __m128i AI = _mm_set_epi32((int)I[i % NI], (int)I[(i + 2) % NI], (int)I[(i + 3) % NI], (int)I[(i + 5) % NI]); (void)A; (void)AD; (void)AI; __m128i r = (expr); mix((uint64_t)_mm_cvtsi128_si64(r)); mix((uint64_t)_mm_cvtsi128_si64(_mm_unpackhi_epi64(r, r))); }

/* roundsd/roundps by MXCSR's mode (SSE4.1, which every x86-64 host that
 * runs this has). */
__attribute__((target("sse4.1"))) static void sse41(uint32_t mode) {
    LINE("roundsd", EACH_D(dbits(_mm_cvtsd_f64(_mm_round_sd(A, A, _MM_FROUND_CUR_DIRECTION)))));
    LINE("roundps", EACH_PI(_mm_castps_si128(_mm_round_ps(A, _MM_FROUND_CUR_DIRECTION))));
}

static void sse(uint32_t mode) {
    setmx(0x1f80 | mode);
    LINE("addsd", PAIRS_D(dbits(_mm_cvtsd_f64(_mm_add_sd(A, B)))));
    LINE("subsd", PAIRS_D(dbits(_mm_cvtsd_f64(_mm_sub_sd(A, B)))));
    LINE("mulsd", PAIRS_D(dbits(_mm_cvtsd_f64(_mm_mul_sd(A, B)))));
    LINE("divsd", PAIRS_D(dbits(_mm_cvtsd_f64(_mm_div_sd(A, B)))));
    LINE("sqrtsd", EACH_D(dbits(_mm_cvtsd_f64(_mm_sqrt_sd(A, A)))));
    LINE("addss", PAIRS_F(fbits(_mm_cvtss_f32(_mm_add_ss(A, B)))));
    LINE("subss", PAIRS_F(fbits(_mm_cvtss_f32(_mm_sub_ss(A, B)))));
    LINE("mulss", PAIRS_F(fbits(_mm_cvtss_f32(_mm_mul_ss(A, B)))));
    LINE("divss", PAIRS_F(fbits(_mm_cvtss_f32(_mm_div_ss(A, B)))));
    LINE("sqrtss", EACH_F(fbits(_mm_cvtss_f32(_mm_sqrt_ss(A)))));
    LINE("addps", PAIRS_PF(_mm_add_ps(A, B)));
    LINE("subps", PAIRS_PF(_mm_sub_ps(A, B)));
    LINE("mulps", PAIRS_PF(_mm_mul_ps(A, B)));
    LINE("divps", PAIRS_PF(_mm_div_ps(A, B)));
    LINE("sqrtps", PAIRS_PF(_mm_sqrt_ps(A)));
    LINE("addpd", PAIRS_PD(_mm_add_pd(A, B)));
    LINE("mulpd", PAIRS_PD(_mm_mul_pd(A, B)));
    LINE("divpd", PAIRS_PD(_mm_div_pd(A, B)));
    LINE("sqrtpd", PAIRS_PD(_mm_sqrt_pd(A)));
    LINE("cvtsd2ss", EACH_D(fbits(_mm_cvtss_f32(_mm_cvtsd_ss(_mm_setzero_ps(), A)))));
    LINE("cvtss2sd", EACH_F(dbits(_mm_cvtsd_f64(_mm_cvtss_sd(_mm_setzero_pd(), A)))));
    LINE("cvtsd2si", EACH_D((uint64_t)_mm_cvtsd_si64(A)));
    LINE("cvtsd2si32", EACH_D((uint32_t)_mm_cvtsd_si32(A)));
    LINE("cvtss2si", EACH_F((uint64_t)_mm_cvtss_si64(A)));
    LINE("cvtsi2sd", EACH_I(dbits(_mm_cvtsd_f64(_mm_cvtsi64_sd(_mm_setzero_pd(), n)))));
    LINE("cvtsi2ss", EACH_I(fbits(_mm_cvtss_f32(_mm_cvtsi64_ss(_mm_setzero_ps(), n)))));
    LINE("cvtsi2ss32", EACH_I(fbits(_mm_cvtss_f32(_mm_cvtsi32_ss(_mm_setzero_ps(), (int)n)))));
    LINE("cvtpd2ps", EACH_PI(_mm_castps_si128(_mm_cvtpd_ps(AD))));
    LINE("cvtps2pd", EACH_PI(_mm_castpd_si128(_mm_cvtps_pd(A))));
    LINE("cvtps2dq", EACH_PI(_mm_cvtps_epi32(A)));
    LINE("cvtpd2dq", EACH_PI(_mm_cvtpd_epi32(AD)));
    LINE("cvtdq2ps", EACH_PI(_mm_castps_si128(_mm_cvtepi32_ps(AI))));
    sse41(mode);
    setmx(0x1f80);
}
#endif

/* x87: the control word's rounding (bits 10-11) and precision (8-9). */
static void setcw(uint16_t v) { __asm__ volatile("fldcw %0" : : "m"(v) : "memory"); }
#define XLINE(name, ...) do { h = 0xcbf29ce484222325ull; __VA_ARGS__; printf("cw %04x %-10s %016llx\n", cw, name, (unsigned long long)h); } while (0)
#define XPAIRS(expr) for (int i = 0; i < ND; i++) for (int j = 0; j < ND; j++) { FENCE(); long double a = D[i], b = D[j]; long double r = (expr); FENCE(); volatile long double v = r; uint64_t m; uint16_t e; memcpy(&m, (const void *)&v, 8); memcpy(&e, (const char *)&v + 8, 2); mix(m); mix(e); }

static void x87(uint16_t cw) {
    setcw(cw);
    XLINE("fadd", XPAIRS(a + b));
    XLINE("fmul", XPAIRS(a * b));
    XLINE("fdiv", XPAIRS(a / b));
    XLINE("fadd3", XPAIRS(a / 3 + b));
    XLINE("fst32", for (int i = 0; i < ND; i++) for (int j = 0; j < ND; j++) { FENCE(); long double a = D[i], b = D[j]; volatile float f = (float)(a * b); mix(fbits(f)); });
    XLINE("fst64", for (int i = 0; i < ND; i++) for (int j = 0; j < ND; j++) { FENCE(); long double a = D[i], b = D[j]; volatile double d = (double)(a / b); mix(dbits(d)); });
    XLINE("fistp", for (int i = 0; i < ND; i++) { FENCE(); long double a = D[i] * 3.7L; int64_t n; __asm__ volatile("fistpll %0" : "=m"(n) : "t"(a) : "st"); mix((uint64_t)n); });
    setcw(0x37f);
}

int main(void) {
#if defined(__x86_64__)
    static const uint32_t modes[] = {0x0000, 0x2000, 0x4000, 0x6000};
    for (unsigned k = 0; k < sizeof modes / sizeof modes[0]; k++) sse(modes[k]);
#endif
    static const uint16_t cws[] = {0x037f, 0x077f, 0x0b7f, 0x0f7f, 0x007f, 0x027f, 0x0e7f, 0x047f};
    for (unsigned k = 0; k < sizeof cws / sizeof cws[0]; k++) x87(cws[k]);
    return 0;
}
